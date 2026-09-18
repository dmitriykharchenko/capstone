use super::{
    EmbeddedChunk, EmbeddingIdentity, IndexSnapshot, IndexStatus, LexicalStatus, PreparedOperation,
    RetrievalError, RetrievalResult,
    ranking::Bm25Index,
    types::{ExchangeVector, check, nonempty, seal},
};
use crate::{
    config::{validate_database_path, validate_root},
    runtime::Cancellation,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlite::{Connection, OpenFlags, State, Value};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const APP_ID: i64 = 0x52545631;
const VERSION: i64 = 1;
const SCHEMA: &str = "
CREATE TABLE retrieval_meta (id INTEGER PRIMARY KEY CHECK(id=1), model TEXT NOT NULL, dimension INTEGER NOT NULL, metric TEXT NOT NULL CHECK(metric='squared_l2'), revision INTEGER NOT NULL CHECK(revision>=0));
CREATE TABLE chunks (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, paper_id TEXT NOT NULL, chunk_index INTEGER NOT NULL, payload TEXT NOT NULL, UNIQUE(paper_id,chunk_index));
CREATE TABLE exchanges (seq INTEGER PRIMARY KEY AUTOINCREMENT, exchange_id INTEGER NOT NULL UNIQUE, session_id TEXT NOT NULL, payload TEXT NOT NULL);
CREATE TABLE lexical (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);
";

/// Operation-local connections: reads use a single SQLite snapshot; writes use
/// BEGIN IMMEDIATE and rollback-on-drop. No partial paper replacement is visible.
#[derive(Clone)]
pub struct VectorStore {
    root: PathBuf,
    path: PathBuf,
    identity: EmbeddingIdentity,
    lexical_memo: Arc<Mutex<Option<(String, Bm25Index)>>>,
}
impl VectorStore {
    /// No I/O mutation. The caller supplies a distinct, app-owned root from config.
    pub fn new(root: &Path, path: &Path, identity: EmbeddingIdentity) -> RetrievalResult<Self> {
        identity.validate()?;
        Ok(Self {
            root: validate_root(root).map_err(|_| RetrievalError::InvalidPath)?,
            path: validate_database_path(root, path).map_err(|_| RetrievalError::InvalidPath)?,
            identity,
            lexical_memo: Arc::new(Mutex::new(None)),
        })
    }
    pub fn identity(&self) -> &EmbeddingIdentity {
        &self.identity
    }
    pub fn database_path(&self) -> &Path {
        &self.path
    }
    fn check_path(&self) -> RetrievalResult<()> {
        if validate_database_path(&self.root, &self.path)
            .map_err(|_| RetrievalError::InvalidPath)?
            != self.path
        {
            return Err(RetrievalError::InvalidPath);
        }
        for suffix in ["-journal", "-wal", "-shm"] {
            let mut path = self.path.as_os_str().to_os_string();
            path.push(suffix);
            validate_database_path(&self.root, Path::new(&path))
                .map_err(|_| RetrievalError::InvalidPath)?;
        }
        Ok(())
    }
    fn connection(&self, write: bool) -> RetrievalResult<Connection> {
        self.check_path()?;
        let flags = if write {
            OpenFlags::new().with_read_write()
        } else {
            OpenFlags::new().with_read_only()
        };
        let mut db = Connection::open_with_flags(&self.path, flags)?;
        db.set_busy_timeout(1000)?;
        db.execute("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON")?;
        if write {
            db.execute("PRAGMA synchronous=FULL")?;
        }
        Ok(db)
    }
    /// Create only when absent; incompatible/corrupt/legacy files are never reset.
    /// Creation publishes a complete temporary SQLite file without clobbering an
    /// existing target. It is itself a final mutation, with its own request token.
    pub fn initialize(&self, c: &Cancellation) -> RetrievalResult<()> {
        self.initialize_inner(c, || Ok(()), || Ok(()))
    }
    fn initialize_inner(
        &self,
        c: &Cancellation,
        before_publish: impl FnOnce() -> RetrievalResult<()>,
        after_publish: impl FnOnce() -> RetrievalResult<()>,
    ) -> RetrievalResult<()> {
        check(c)?;
        self.check_path()?;
        if self.path.exists() {
            return self.snapshot(c).map(|_| ());
        }
        fs::create_dir_all(&self.root)?;
        self.check_path()?;
        let staging = Staging(
            self.root
                .join(format!(".retrieval-{}.sqlite", uuid::Uuid::new_v4())),
        );
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging.0)?;
        let db = sqlite::open(&staging.0)?;
        db.execute("PRAGMA synchronous=FULL; BEGIN IMMEDIATE")?;
        db.execute(SCHEMA)?;
        db.execute(format!(
            "PRAGMA application_id={APP_ID}; PRAGMA user_version={VERSION}"
        ))?;
        execute(
            &db,
            "INSERT INTO retrieval_meta VALUES(1,?,?,'squared_l2',0)",
            &[
                self.identity.model.clone().into(),
                (self.identity.dimension as i64).into(),
            ],
        )?;
        db.execute("COMMIT")?;
        drop(db);
        file.sync_all()?;
        self.check_path()?;
        before_publish()?;
        super::publication::publish_no_replace(&staging.0, &self.path, c)?;
        // The staging guard is still alive: publication must not rely on its Drop.
        after_publish()
    }
    pub fn snapshot(&self, c: &Cancellation) -> RetrievalResult<IndexSnapshot> {
        check(c)?;
        let db = self.connection(false)?;
        db.execute("BEGIN")?;
        let snapshot = read_snapshot(&db, c, &self.lexical_memo)?;
        if snapshot.status.identity != self.identity {
            return Err(RetrievalError::ReindexRequired);
        }
        check(c)?;
        Ok(snapshot)
    }
    pub fn status(&self, c: &Cancellation) -> RetrievalResult<IndexStatus> {
        Ok(self.snapshot(c)?.status)
    }
    pub fn read_paper(
        &self,
        paper_id: &str,
        c: &Cancellation,
    ) -> RetrievalResult<Vec<EmbeddedChunk>> {
        nonempty(paper_id)?;
        Ok(self
            .snapshot(c)?
            .chunks
            .into_iter()
            .filter(|chunk| chunk.paper_id == paper_id)
            .collect())
    }
    /// Apply all operations in order in ONE store transaction. A reset must be
    /// first, and must name this handle's identity; old handles reject new data.
    /// expected_revision provides an optimistic snapshot/preparation guard.
    /// No transaction handle/permit escapes, and no cross-store atomicity is implied.
    pub fn apply(
        &self,
        operations: &[PreparedOperation],
        expected_revision: Option<i64>,
        c: &Cancellation,
    ) -> RetrievalResult<IndexStatus> {
        self.apply_inner(operations, expected_revision, c, |_| Ok(()))
    }
    fn apply_inner(
        &self,
        operations: &[PreparedOperation],
        expected_revision: Option<i64>,
        c: &Cancellation,
        before_commit: impl FnOnce(&Connection) -> RetrievalResult<()>,
    ) -> RetrievalResult<IndexStatus> {
        if operations.is_empty() {
            return Err(RetrievalError::InvalidInput);
        }
        check(c)?;
        let db = self.connection(true)?;
        db.execute("BEGIN IMMEDIATE")?;
        // Connection drop rolls back on every error or unwind, including COMMIT failure.
        let before = read_snapshot(&db, c, &self.lexical_memo)?;
        if expected_revision.is_some_and(|revision| revision != before.status.revision) {
            return Err(RetrievalError::Conflict);
        }
        let reset = matches!(operations.first(), Some(PreparedOperation::Reset(_)));
        if !reset && before.status.identity != self.identity {
            return Err(RetrievalError::ReindexRequired);
        }
        let mut dirty = false;
        let mut rebuild = false;
        for (i, operation) in operations.iter().enumerate() {
            check(c)?;
            match operation {
                PreparedOperation::Reset(prepared) => {
                    if i != 0 || prepared.identity != self.identity {
                        return Err(RetrievalError::InvalidInput);
                    }
                    db.execute("DELETE FROM chunks; DELETE FROM exchanges; DELETE FROM lexical")?;
                    execute(
                        &db,
                        "UPDATE retrieval_meta SET model=?,dimension=? WHERE id=1",
                        &[
                            self.identity.model.clone().into(),
                            (self.identity.dimension as i64).into(),
                        ],
                    )?;
                    dirty = true;
                }
                PreparedOperation::ReplacePaper(paper) => {
                    if paper.identity != self.identity {
                        return Err(RetrievalError::ReindexRequired);
                    }
                    let existing = query(
                        &db,
                        "SELECT id,payload FROM chunks WHERE paper_id=? ORDER BY seq",
                        &[paper.paper_id.clone().into()],
                        c,
                    )?;
                    let current: Vec<EmbeddedChunk> = existing
                        .iter()
                        .map(|r| decode(&r[1]))
                        .collect::<RetrievalResult<_>>()?;
                    if current == paper.chunks {
                        continue;
                    }
                    let ids: BTreeSet<_> = paper.chunks.iter().map(|v| v.id.as_str()).collect();
                    // Delete removed records, then upsert retained IDs without changing
                    // their insertion order. Reject identity theft from another paper.
                    for row in &existing {
                        check(c)?;
                        if !ids.contains(text(&row[0])?) {
                            execute(&db, "DELETE FROM chunks WHERE id=?", &[row[0].clone()])?;
                        }
                    }
                    // Remove old ordinal conflicts while retaining seq for existing IDs.
                    execute(
                        &db,
                        "UPDATE chunks SET chunk_index=-seq WHERE paper_id=?",
                        &[paper.paper_id.clone().into()],
                    )?;
                    for chunk in &paper.chunks {
                        check(c)?;
                        let owners = query(
                            &db,
                            "SELECT paper_id FROM chunks WHERE id=?",
                            &[chunk.id.clone().into()],
                            c,
                        )?;
                        if owners
                            .first()
                            .is_some_and(|r| r[0] != Value::String(paper.paper_id.clone()))
                        {
                            return Err(RetrievalError::Conflict);
                        }
                        execute(
                            &db,
                            "INSERT INTO chunks(id,paper_id,chunk_index,payload) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET chunk_index=excluded.chunk_index,payload=excluded.payload",
                            &[
                                chunk.id.clone().into(),
                                chunk.paper_id.clone().into(),
                                i64::from(chunk.chunk_index).into(),
                                encode(chunk)?.into(),
                            ],
                        )?;
                    }
                    dirty = true;
                }
                PreparedOperation::DeletePaper(id) => {
                    nonempty(id)?;
                    execute(
                        &db,
                        "DELETE FROM chunks WHERE paper_id=?",
                        &[id.clone().into()],
                    )?;
                    dirty = true;
                }
                PreparedOperation::UpsertExchange(exchange) => {
                    if exchange.identity != self.identity {
                        return Err(RetrievalError::ReindexRequired);
                    }
                    let e = &exchange.record;
                    e.validate(&self.identity)?;
                    let existing = query(
                        &db,
                        "SELECT payload FROM exchanges WHERE exchange_id=?",
                        &[e.exchange_id.into()],
                        c,
                    )?;
                    if existing.first().is_some_and(|row| {
                        decode::<ExchangeVector>(&row[0]).is_ok_and(|old| old == *e)
                    }) {
                        continue;
                    }
                    execute(
                        &db,
                        "INSERT INTO exchanges(exchange_id,session_id,payload) VALUES(?,?,?) ON CONFLICT(exchange_id) DO UPDATE SET session_id=excluded.session_id,payload=excluded.payload",
                        &[
                            e.exchange_id.into(),
                            e.session_id.clone().into(),
                            encode(e)?.into(),
                        ],
                    )?;
                    dirty = true;
                }
                PreparedOperation::DeleteExchange(id) => {
                    if *id <= 0 {
                        return Err(RetrievalError::InvalidInput);
                    }
                    execute(
                        &db,
                        "DELETE FROM exchanges WHERE exchange_id=?",
                        &[(*id).into()],
                    )?;
                    dirty = true;
                }
                PreparedOperation::RebuildLexical => {
                    rebuild = true;
                }
            }
        }
        if dirty {
            db.execute(
                "UPDATE retrieval_meta SET revision=revision+1 WHERE id=1; DELETE FROM lexical",
            )?;
        }
        let mut after = read_snapshot(&db, c, &self.lexical_memo)?;
        if rebuild {
            let cache = LexicalCache {
                version: 1,
                revision: after.status.revision,
                fingerprint: fingerprint(&after.chunks)?,
                index: after.lexical.clone(),
            };
            execute(
                &db,
                "INSERT INTO lexical VALUES(1,?) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
                &[encode(&cache)?.into()],
            )?;
            after.status.lexical = LexicalStatus::Ready;
        }
        before_commit(&db)?;
        check(c)?;
        let _permit = seal(c)?;
        db.execute("COMMIT")?;
        Ok(after.status)
    }
}

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LexicalCache {
    version: u32,
    revision: i64,
    fingerprint: String,
    index: Bm25Index,
}
fn fingerprint(chunks: &[EmbeddedChunk]) -> RetrievalResult<String> {
    Ok(Sha256::digest(encode(chunks)?.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn read_snapshot(
    db: &Connection,
    c: &Cancellation,
    memo: &Mutex<Option<(String, Bm25Index)>>,
) -> RetrievalResult<IndexSnapshot> {
    check(c)?;
    if scalar(db, "PRAGMA application_id", c)? != APP_ID
        || scalar(db, "PRAGMA user_version", c)? != VERSION
    {
        return Err(RetrievalError::InvalidStore);
    }
    if query(db, "PRAGMA quick_check", &[], c)? != vec![vec![Value::String("ok".into())]] {
        return Err(RetrievalError::InvalidStore);
    }
    let tables = query(
        db,
        "SELECT type,name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
        &[],
        c,
    )?;
    let expected: Vec<Vec<Value>> = ["chunks", "exchanges", "lexical", "retrieval_meta"]
        .into_iter()
        .map(|s| vec!["table".into(), s.into()])
        .collect();
    if tables != expected {
        return Err(RetrievalError::InvalidStore);
    }
    let rows = query(
        db,
        "SELECT id,model,dimension,metric,revision FROM retrieval_meta",
        &[],
        c,
    )?;
    if rows.len() != 1 {
        return Err(RetrievalError::InvalidStore);
    }
    let row = &rows[0];
    let dimension = usize::try_from(integer(&row[2])?).map_err(|_| RetrievalError::InvalidStore)?;
    let identity = EmbeddingIdentity {
        model: text(&row[1])?.to_owned(),
        dimension,
    };
    identity
        .validate()
        .map_err(|_| RetrievalError::InvalidStore)?;
    let revision = integer(&row[4])?;
    if integer(&row[0])? != 1 || text(&row[3])? != "squared_l2" || revision < 0 {
        return Err(RetrievalError::InvalidStore);
    }
    let mut chunks = vec![];
    let mut ids = BTreeSet::new();
    let mut provenance = BTreeSet::new();
    for row in query(
        db,
        "SELECT id,paper_id,chunk_index,payload FROM chunks ORDER BY seq",
        &[],
        c,
    )? {
        let chunk: EmbeddedChunk = decode(&row[3])?;
        chunk
            .validate(&identity)
            .map_err(|_| RetrievalError::InvalidStore)?;
        if chunk.id != text(&row[0])?
            || chunk.paper_id != text(&row[1])?
            || i64::from(chunk.chunk_index) != integer(&row[2])?
            || !ids.insert(chunk.id.clone())
            || !provenance.insert((chunk.paper_id.clone(), chunk.chunk_index))
        {
            return Err(RetrievalError::InvalidStore);
        }
        chunks.push(chunk);
    }
    let mut exchanges = vec![];
    for row in query(
        db,
        "SELECT exchange_id,session_id,payload FROM exchanges ORDER BY seq",
        &[],
        c,
    )? {
        let e: ExchangeVector = decode(&row[2])?;
        e.validate(&identity)
            .map_err(|_| RetrievalError::InvalidStore)?;
        if e.exchange_id != integer(&row[0])? || e.session_id != text(&row[1])? {
            return Err(RetrievalError::InvalidStore);
        }
        exchanges.push(e);
    }
    let content_fingerprint = fingerprint(&chunks)?;
    let cached = memo
        .lock()
        .map_err(|_| RetrievalError::Storage)?
        .as_ref()
        .filter(|(fingerprint, _)| fingerprint == &content_fingerprint)
        .map(|(_, index)| index.clone());
    let fresh = match cached {
        Some(index) => index,
        None => {
            let index = Bm25Index::build(chunks.iter().map(|c| c.text.as_str()), c)?;
            *memo.lock().map_err(|_| RetrievalError::Storage)? =
                Some((content_fingerprint.clone(), index.clone()));
            index
        }
    };
    // Derived lexical corruption degrades to freshly computed exact BM25, never
    // to FTS or stale scores. Validate statistics, not just counts/JSON shape.
    let cache_rows = query(db, "SELECT id,payload FROM lexical", &[], c)?;
    let cache = cache_rows
        .first()
        .filter(|r| cache_rows.len() == 1 && r[0] == Value::Integer(1))
        .and_then(|r| decode::<LexicalCache>(&r[1]).ok());
    let cache = cache.filter(|cache| {
        cache.version == 1
            && cache.revision == revision
            && content_fingerprint == cache.fingerprint
            && cache.index == fresh
    });
    let lexical_status = if cache.is_some() {
        LexicalStatus::Ready
    } else {
        LexicalStatus::Stale
    };
    let lexical = cache.map(|c| c.index).unwrap_or(fresh);
    let status = IndexStatus {
        identity,
        revision,
        chunk_count: chunks.len(),
        paper_count: chunks
            .iter()
            .map(|c| &c.paper_id)
            .collect::<BTreeSet<_>>()
            .len(),
        exchange_count: exchanges.len(),
        lexical: lexical_status,
    };
    Ok(IndexSnapshot {
        status,
        chunks,
        exchanges,
        lexical,
    })
}
fn query(
    db: &Connection,
    sql: &str,
    values: &[Value],
    c: &Cancellation,
) -> RetrievalResult<Vec<Vec<Value>>> {
    check(c)?;
    let mut s = db.prepare(sql)?;
    for (i, v) in values.iter().enumerate() {
        s.bind((i + 1, v))?;
    }
    let mut rows = vec![];
    while s.next()? == State::Row {
        check(c)?;
        rows.push(
            (0..s.column_count())
                .map(|i| s.read::<Value, _>(i))
                .collect::<Result<_, _>>()?,
        );
    }
    Ok(rows)
}
fn execute(db: &Connection, sql: &str, values: &[Value]) -> RetrievalResult<()> {
    let mut s = db.prepare(sql)?;
    for (i, v) in values.iter().enumerate() {
        s.bind((i + 1, v))?;
    }
    if s.next()? != State::Done {
        return Err(RetrievalError::InvalidStore);
    }
    Ok(())
}
fn scalar(db: &Connection, sql: &str, c: &Cancellation) -> RetrievalResult<i64> {
    let rows = query(db, sql, &[], c)?;
    rows.first()
        .and_then(|r| r.first())
        .ok_or(RetrievalError::InvalidStore)
        .and_then(integer)
}
fn text(v: &Value) -> RetrievalResult<&str> {
    if let Value::String(s) = v {
        Ok(s)
    } else {
        Err(RetrievalError::InvalidStore)
    }
}
fn integer(v: &Value) -> RetrievalResult<i64> {
    if let Value::Integer(i) = v {
        Ok(*i)
    } else {
        Err(RetrievalError::InvalidStore)
    }
}
fn encode<T: Serialize + ?Sized>(v: &T) -> RetrievalResult<String> {
    serde_json::to_string(v).map_err(|_| RetrievalError::InvalidInput)
}
fn decode<T: for<'de> Deserialize<'de>>(v: &Value) -> RetrievalResult<T> {
    serde_json::from_str(text(v)?).map_err(|_| RetrievalError::InvalidStore)
}

#[cfg(test)]
#[path = "transaction_tests.rs"]
mod tests;
