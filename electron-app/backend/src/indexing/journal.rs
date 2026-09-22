//! App-owned SQLite authority. One pending operation; all artifacts are strict,
//! versioned, bounded and hashed. Hashes detect drift, not malicious authenticity.
use super::{IndexError, IndexResult};
use crate::{
    config::{validate_database_path, validate_root},
    graph::{Fence, PaperGraph, PaperRecoveryArtifact as GraphArtifact, Scope, Transition},
    retrieval::{EmbeddingIdentity, PaperRecoveryArtifact as RetrievalArtifact},
    runtime::Cancellation,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlite::{Connection, OpenFlags, State, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_RECORD: usize = 272 * 1024 * 1024;
const MAX_VERSION: usize = 132 * 1024 * 1024;
const SCHEMA: &str = "
CREATE TABLE index_meta(id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);
CREATE TABLE operations(id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);
CREATE TABLE active_papers(paper_id TEXT PRIMARY KEY, version_id TEXT NOT NULL UNIQUE, payload TEXT NOT NULL);
CREATE TABLE global_labels(node_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
CREATE TABLE paper_node_memberships(paper_id TEXT NOT NULL, version_id TEXT NOT NULL, node_id TEXT NOT NULL, PRIMARY KEY(node_id,paper_id));
PRAGMA application_id=1230523953; PRAGMA user_version=1;";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Version {
    pub id: String,
    pub source: String,
    pub retrieval: String,
    pub graph: String,
}
impl Version {
    pub fn validate(&self) -> IndexResult<(RetrievalArtifact, GraphArtifact)> {
        uuid(&self.id)?;
        hash(&self.source)?;
        let r = RetrievalArtifact::from_json(self.retrieval.as_bytes())?;
        let g = GraphArtifact::from_json(self.graph.as_bytes())?;
        if r.paper_id() != g.paper_id() || r.has_rows() != g.paper().is_some() {
            return Err(IndexError::InvalidJournal);
        }
        Ok((r, g))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Meta {
    pub fence: Fence,
    pub revision: i64,
    pub epoch: u64,
    pub identity: EmbeddingIdentity,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum Phase {
    Prepared,
    RetrievalApplied,
    GraphApplied,
    Compensating,
    Complete,
    Aborted,
    ResetPrepared,
    ResetFinalizing,
    ResetRetrieval,
    ResetGraph,
}
impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Aborted)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct PaperOp {
    pub prior: Option<Version>,
    pub desired: Version,
    pub transition: Transition,
    pub affected: BTreeMap<String, String>,
}
impl PaperOp {
    pub fn before_retrieval(&self) -> IndexResult<RetrievalArtifact> {
        if let Some(v) = &self.prior {
            Ok(v.validate()?.0)
        } else {
            let r = self.desired.validate()?.0;
            Ok(RetrievalArtifact::no_rows(
                r.paper_id().into(),
                r.identity().clone(),
            )?)
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) enum Kind {
    Paper(Box<PaperOp>),
    Reset { next: Fence },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Operation {
    pub id: String,
    pub base: Meta,
    pub phase: Phase,
    pub kind: Kind,
}
impl Operation {
    pub fn check_size(&self) -> IndexResult<()> {
        if let Kind::Paper(p) = &self.kind {
            encode(&p.desired, MAX_VERSION)?;
            if let Some(v) = &p.prior {
                encode(v, MAX_VERSION)?;
            }
        }
        encode(self, MAX_RECORD)?;
        Ok(())
    }
    pub fn validate(&self) -> IndexResult<()> {
        uuid(&self.id)?;
        self.base.fence.validate()?;
        self.base.identity.validate()?;
        if self.base.revision < 0 {
            return Err(IndexError::InvalidJournal);
        }
        match &self.kind {
            Kind::Paper(p) => {
                p.transition.validate()?;
                let (r, g) = p.desired.validate()?;
                let before = p.before_retrieval()?;
                if p.transition.base != self.base.fence
                    || r.identity() != &self.base.identity
                    || before.identity() != r.identity()
                    || before.paper_id() != r.paper_id()
                    || p.transition.desired.paper_id != g.paper_id()
                    || p.transition.desired.paper.as_ref() != g.paper()
                {
                    return Err(IndexError::InvalidJournal);
                }
                if let Some(prior) = &p.prior {
                    let (pr, pg) = prior.validate()?;
                    if pr != before
                        || membership(pg.paper()) != membership(p.transition.before.paper.as_ref())
                        || pr.paper_id() != r.paper_id()
                        || prior.id == p.desired.id
                    {
                        return Err(IndexError::InvalidJournal);
                    }
                } else if before.has_rows() || p.transition.before.paper.is_some() {
                    return Err(IndexError::InvalidJournal);
                }
                let ids: BTreeSet<_> = p
                    .transition
                    .before
                    .nodes
                    .iter()
                    .flat_map(|n| n.papers.iter().cloned())
                    .collect();
                if p.affected.keys().cloned().collect::<BTreeSet<_>>() != ids {
                    return Err(IndexError::InvalidJournal);
                }
                for id in p.affected.values() {
                    uuid(id)?;
                }
                if matches!(
                    self.phase,
                    Phase::ResetPrepared
                        | Phase::ResetFinalizing
                        | Phase::ResetRetrieval
                        | Phase::ResetGraph
                ) {
                    return Err(IndexError::InvalidJournal);
                }
            }
            Kind::Reset { next } => {
                next.validate()?;
                if next.direction != "coordinated_reset"
                    || next.generation
                        != self
                            .base
                            .fence
                            .generation
                            .checked_add(1)
                            .ok_or(IndexError::InvalidJournal)?
                    || next.digest != digest(b"empty-derived-index-v1")
                    || self.base.epoch == u64::MAX
                    || !matches!(
                        self.phase,
                        Phase::ResetPrepared
                            | Phase::ResetFinalizing
                            | Phase::ResetRetrieval
                            | Phase::ResetGraph
                            | Phase::Complete
                            | Phase::Aborted
                    )
                {
                    return Err(IndexError::InvalidJournal);
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Label {
    name: String,
    sequence: i64,
    writer: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    version: u32,
    body: T,
    sha256: String,
}
pub(super) fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(super) fn hash(s: &str) -> IndexResult<()> {
    if s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(IndexError::InvalidJournal)
    }
}
fn uuid(s: &str) -> IndexResult<()> {
    let id = uuid::Uuid::parse_str(s).map_err(|_| IndexError::InvalidJournal)?;
    if id.get_version() == Some(uuid::Version::Random) && id.to_string() == s {
        Ok(())
    } else {
        Err(IndexError::InvalidJournal)
    }
}
fn encode<T: Serialize>(body: &T, max: usize) -> IndexResult<String> {
    let raw = serde_json::to_vec(body).map_err(|_| IndexError::InvalidJournal)?;
    if raw.len() > max.saturating_sub(128) {
        return Err(IndexError::Limit);
    }
    let text = serde_json::to_string(&Envelope {
        version: 1,
        body,
        sha256: digest(&raw),
    })
    .map_err(|_| IndexError::InvalidJournal)?;
    if text.len() > max {
        return Err(IndexError::Limit);
    }
    Ok(text)
}
fn decode<T: Serialize + DeserializeOwned>(text: &str, max: usize) -> IndexResult<T> {
    if text.len() > max {
        return Err(IndexError::Limit);
    }
    let e: Envelope<T> = serde_json::from_str(text).map_err(|_| IndexError::InvalidJournal)?;
    if e.version != 1
        || digest(&serde_json::to_vec(&e.body).map_err(|_| IndexError::InvalidJournal)?) != e.sha256
        || encode(&e.body, max)? != text
    {
        return Err(IndexError::InvalidJournal);
    }
    Ok(e.body)
}
pub(super) fn membership(p: Option<&PaperGraph>) -> (Option<String>, BTreeSet<String>) {
    match p {
        None => (None, BTreeSet::new()),
        Some(p) => (
            Some(p.title.clone()),
            p.authors
                .iter()
                .map(|a| a.author_id.clone())
                .chain(p.topics.iter().map(|t| t.topic_id.clone()))
                .collect(),
        ),
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WritePoint {
    BeforeSeal,
    AfterSeal,
    AfterCommit,
}
#[cfg(test)]
type WriteHook = Arc<dyn Fn(WritePoint, &Cancellation) -> IndexResult<()> + Send + Sync>;
#[derive(Clone)]
pub(super) struct Journal {
    root: PathBuf,
    path: PathBuf,
    _lease: Arc<File>,
    #[cfg(test)]
    pub hook: Option<WriteHook>,
}
impl Journal {
    pub fn open(root: &Path, path: &Path, c: &Cancellation) -> IndexResult<Self> {
        let root = validate_root(root).map_err(|_| IndexError::InvalidPath)?;
        let path = validate_database_path(&root, path).map_err(|_| IndexError::InvalidPath)?;
        std::fs::create_dir_all(&root)?;
        let lease = lock_file(&root, &path.with_extension("sqlite3.lock"))?;
        if !path.exists() {
            let temp = root.join(format!(".index-{}.staging", uuid::Uuid::new_v4()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&temp)?;
            let staging = Staging(temp);
            let db = sqlite::open(&staging.0)?;
            db.execute("PRAGMA synchronous=FULL; BEGIN IMMEDIATE")?;
            db.execute(SCHEMA)?;
            db.execute("COMMIT")?;
            drop(db);
            file.sync_all()?;
            crate::retrieval::publish_index_journal(&staging.0, &path, c)?;
        }
        let this = Self {
            root,
            path,
            _lease: Arc::new(lease),
            #[cfg(test)]
            hook: None,
        };
        let db = this.connection(false)?;
        if scalar(&db, "PRAGMA application_id")? != 1230523953
            || scalar(&db, "PRAGMA user_version")? != 1
        {
            return Err(IndexError::InvalidJournal);
        }
        let tables = rows(
            &db,
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
            &[],
            7,
        )?;
        let expected = [
            "active_papers",
            "global_labels",
            "index_meta",
            "operations",
            "paper_node_memberships",
        ];
        if tables.len() != expected.len()
            || tables
                .iter()
                .zip(expected)
                .any(|(r, n)| text(&r[0]).ok() != Some(n))
        {
            return Err(IndexError::InvalidJournal);
        }
        for definition in SCHEMA
            .split(';')
            .map(str::trim)
            .filter(|s| s.starts_with("CREATE TABLE"))
        {
            let name = definition
                .split_whitespace()
                .nth(2)
                .ok_or(IndexError::InvalidJournal)?
                .split('(')
                .next()
                .unwrap();
            if rows_first(
                &db,
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?",
                &[name.into()],
            )?
            .as_deref()
                != Some(definition)
            {
                return Err(IndexError::InvalidJournal);
            }
        }
        if scalar(
            &db,
            "SELECT count(*) FROM sqlite_master WHERE type NOT IN ('table','index') OR (type='index' AND sql IS NOT NULL)",
        )? != 0
        {
            return Err(IndexError::InvalidJournal);
        }
        Ok(this)
    }
    fn connection(&self, write: bool) -> IndexResult<Connection> {
        validate_database_path(&self.root, &self.path).map_err(|_| IndexError::InvalidPath)?;
        for suffix in ["-journal", "-wal", "-shm"] {
            validate_database_path(
                &self.root,
                Path::new(&format!("{}{suffix}", self.path.display())),
            )
            .map_err(|_| IndexError::InvalidPath)?;
        }
        let flags = if write {
            OpenFlags::new().with_read_write()
        } else {
            OpenFlags::new().with_read_only()
        };
        let mut db = Connection::open_with_flags(&self.path, flags)?;
        db.set_busy_timeout(1000)?;
        db.execute("PRAGMA trusted_schema=OFF")?;
        if write {
            db.execute("PRAGMA synchronous=FULL; BEGIN IMMEDIATE")?;
        } else {
            db.execute("BEGIN")?;
        }
        Ok(db)
    }
    fn write<T>(
        &self,
        c: &Cancellation,
        f: impl FnOnce(&Connection) -> IndexResult<T>,
    ) -> IndexResult<T> {
        c.check()?;
        let db = self.connection(true)?;
        let value = f(&db)?;
        #[cfg(test)]
        if let Some(h) = &self.hook {
            h(WritePoint::BeforeSeal, c)?;
        }
        let _seal = c.begin_commit()?;
        #[cfg(test)]
        if let Some(h) = &self.hook {
            h(WritePoint::AfterSeal, c)?;
        }
        db.execute("COMMIT")?;
        #[cfg(test)]
        if let Some(h) = &self.hook {
            h(WritePoint::AfterCommit, c)?;
        }
        Ok(value)
    }
    pub fn meta(&self) -> IndexResult<Option<Meta>> {
        load_meta(&self.connection(false)?)
    }
    pub fn operation(&self) -> IndexResult<Option<Operation>> {
        load_op(&self.connection(false)?)
    }
    pub fn active(&self, paper: &str) -> IndexResult<Option<Version>> {
        active(&self.connection(false)?, paper)
    }
    pub fn next_active(&self, after: &str) -> IndexResult<Option<(String, Version)>> {
        let db = self.connection(false)?;
        let list = rows(
            &db,
            "SELECT paper_id FROM active_papers WHERE paper_id>? ORDER BY paper_id LIMIT 1",
            &[after.into()],
            1,
        )?;
        if let Some(r) = list.first() {
            let id = text(&r[0])?;
            Ok(Some((
                id.into(),
                active(&db, id)?.ok_or(IndexError::InvalidJournal)?,
            )))
        } else {
            Ok(None)
        }
    }
    pub fn bootstrap(&self, meta: &Meta, c: &Cancellation) -> IndexResult<()> {
        self.write(c, |db| {
            if load_meta(db)?.is_some()
                || load_op(db)?.is_some()
                || scalar(db, "SELECT count(*) FROM active_papers")? != 0
                || scalar(db, "SELECT count(*) FROM global_labels")? != 0
                || scalar(db, "SELECT count(*) FROM paper_node_memberships")? != 0
            {
                return Err(IndexError::InvalidJournal);
            }
            save_meta(db, meta)
        })
    }
    pub fn validate_scope(&self, scope: &Scope) -> IndexResult<BTreeMap<String, String>> {
        authority(&self.connection(false)?, scope)
    }
    pub fn prepare(&self, op: &Operation, c: &Cancellation) -> IndexResult<()> {
        op.validate()?;
        let payload = encode(op, MAX_RECORD)?;
        self.write(c, |db| {
            if load_meta(db)?.as_ref() != Some(&op.base) || load_op(db)?.is_some_and(|o| !o.phase.terminal()) { return Err(IndexError::Conflict); }
            check_base(db, op)?;
            execute(db, "INSERT INTO operations VALUES(1,?) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", &[payload.into()])
        })
    }
    pub fn phase(&self, id: &str, next: Phase, c: &Cancellation) -> IndexResult<()> {
        self.write(c, |db| {
            let mut op = load_op(db)?.ok_or(IndexError::InvalidJournal)?;
            if op.id != id || op.phase.terminal() {
                return Err(IndexError::Conflict);
            }
            check_base(db, &op)?;
            let valid = matches!(
                (op.phase, next),
                (Phase::Prepared, Phase::RetrievalApplied)
                    | (Phase::RetrievalApplied, Phase::GraphApplied)
                    | (
                        Phase::Prepared
                            | Phase::RetrievalApplied
                            | Phase::GraphApplied
                            | Phase::Compensating,
                        Phase::Compensating
                    )
                    | (Phase::ResetPrepared, Phase::ResetFinalizing)
                    | (Phase::ResetFinalizing, Phase::ResetRetrieval)
                    | (Phase::ResetRetrieval, Phase::ResetGraph)
            );
            if !valid {
                return Err(IndexError::InvalidJournal);
            }
            op.phase = next;
            save_op(db, &op)
        })
    }
    pub fn finish(
        &self,
        id: &str,
        publish: bool,
        fence: Fence,
        revision: i64,
        c: &Cancellation,
    ) -> IndexResult<()> {
        self.write(c, |db| {
            let mut op = load_op(db)?.ok_or(IndexError::InvalidJournal)?;
            if op.id != id || op.phase.terminal() { return Err(IndexError::Conflict); }
            check_base(db,&op)?;
            let mut meta = op.base.clone(); meta.fence = fence.clone(); meta.revision = revision;
            match &op.kind {
                Kind::Paper(p) => {
                    if publish {
                        if fence != p.transition.forward { return Err(IndexError::InvalidJournal); }
                        let (r,g) = p.desired.validate()?;
                        execute(db,"INSERT INTO active_papers VALUES(?,?,?) ON CONFLICT(paper_id) DO UPDATE SET version_id=excluded.version_id,payload=excluded.payload", &[r.paper_id().into(),p.desired.id.clone().into(),encode(&p.desired,MAX_VERSION)?.into()])?;
                        execute(db,"DELETE FROM paper_node_memberships WHERE paper_id=?", &[r.paper_id().into()])?;
                        for node in membership(g.paper()).1 { execute(db,"INSERT INTO paper_node_memberships VALUES(?,?,?)", &[r.paper_id().into(),p.desired.id.clone().into(),node.into()])?; }
                        let desired_nodes = membership(g.paper()).1;
                        for n in &p.transition.desired.nodes {
                            if let Some(name) = &n.name {
                                if desired_nodes.contains(&n.id) { execute(db,"INSERT INTO global_labels VALUES(?,?) ON CONFLICT(node_id) DO UPDATE SET payload=excluded.payload", &[n.id.clone().into(), encode(&Label { name: name.clone(), sequence: fence.generation, writer: op.id.clone() },8192)?.into()])?; }
                            } else { execute(db,"DELETE FROM global_labels WHERE node_id=?", &[n.id.clone().into()])?; }
                        }
                    } else if fence != p.transition.base && fence != p.transition.reverse { return Err(IndexError::InvalidJournal); }
                }
                Kind::Reset { next } => {
                    if publish {
                        if op.phase == Phase::ResetPrepared || &fence != next { return Err(IndexError::InvalidJournal); }
                        db.execute("DELETE FROM active_papers; DELETE FROM paper_node_memberships; DELETE FROM global_labels")?;
                        meta.epoch = meta.epoch.checked_add(1).ok_or(IndexError::InvalidJournal)?;
                    } else if op.phase != Phase::ResetPrepared || fence != op.base.fence || revision != op.base.revision { return Err(IndexError::InvalidJournal); }
                }
            }
            op.phase = if publish { Phase::Complete } else { Phase::Aborted };
            save_meta(db,&meta)?; save_op(db,&op)
        })
    }
    pub fn validate_pending(&self, op: &Operation) -> IndexResult<()> {
        let db = self.connection(false)?;
        if load_op(&db)?.as_ref() != Some(op) {
            return Err(IndexError::Conflict);
        }
        check_base(&db, op)
    }
    pub fn validate_summary(&self, authors: usize, topics: usize, edges: usize) -> IndexResult<()> {
        let db = self.connection(false)?;
        if scalar(&db, "SELECT count(*) FROM global_labels")? != (authors + topics) as i64
            || scalar(&db, "SELECT count(*) FROM paper_node_memberships")? != edges as i64
            || scalar(
                &db,
                "SELECT count(*) FROM paper_node_memberships m LEFT JOIN active_papers a ON m.paper_id=a.paper_id AND m.version_id=a.version_id LEFT JOIN global_labels l ON m.node_id=l.node_id WHERE a.paper_id IS NULL OR l.node_id IS NULL",
            )? != 0
            || scalar(
                &db,
                "SELECT count(*) FROM global_labels l WHERE NOT EXISTS (SELECT 1 FROM paper_node_memberships m WHERE m.node_id=l.node_id)",
            )? != 0
        {
            return Err(IndexError::InvalidJournal);
        }
        let meta = load_meta(&db)?.ok_or(IndexError::InvalidJournal)?;
        if let Some(op) = load_op(&db)? {
            if !op.phase.terminal() || meta.identity != op.base.identity {
                return Err(IndexError::InvalidJournal);
            }
            match &op.kind {
                Kind::Paper(p) => {
                    let published = op.phase == Phase::Complete;
                    let expected = if published {
                        Some(p.desired.clone())
                    } else {
                        p.prior.clone()
                    };
                    let prior = p.before_retrieval()?;
                    let desired = p.desired.validate()?.0;
                    let forward = op
                        .base
                        .revision
                        .checked_add(i64::from(prior != desired || !desired.has_rows()))
                        .ok_or(IndexError::InvalidJournal)?;
                    let reverse = forward
                        .checked_add(i64::from(prior != desired || !prior.has_rows()))
                        .ok_or(IndexError::InvalidJournal)?;
                    let valid = if published {
                        meta.fence == p.transition.forward && meta.revision == forward
                    } else {
                        (meta.fence == p.transition.base || meta.fence == p.transition.reverse)
                            && (meta.revision == op.base.revision || meta.revision == reverse)
                    };
                    if !valid
                        || meta.epoch != op.base.epoch
                        || active(&db, &p.transition.before.paper_id)? != expected
                    {
                        return Err(IndexError::InvalidJournal);
                    }
                }
                Kind::Reset { next } => {
                    if op.phase == Phase::Complete {
                        if meta.fence != *next
                            || meta.epoch != op.base.epoch + 1
                            || meta.revision
                                != op
                                    .base
                                    .revision
                                    .checked_add(1)
                                    .ok_or(IndexError::InvalidJournal)?
                            || scalar(&db, "SELECT count(*) FROM active_papers")? != 0
                        {
                            return Err(IndexError::InvalidJournal);
                        }
                    } else if meta != op.base {
                        return Err(IndexError::InvalidJournal);
                    }
                }
            }
        } else if meta.fence.generation != 0 || meta.revision != 0 || meta.epoch != 0 {
            return Err(IndexError::InvalidJournal);
        }
        Ok(())
    }
}
fn check_base(db: &Connection, op: &Operation) -> IndexResult<()> {
    if load_meta(db)?.as_ref() != Some(&op.base) {
        return Err(IndexError::Conflict);
    }
    if let Kind::Paper(p) = &op.kind
        && (active(db, &p.transition.before.paper_id)? != p.prior
            || authority(db, &p.transition.before)? != p.affected)
    {
        return Err(IndexError::InvalidJournal);
    }
    Ok(())
}
fn load_meta(db: &Connection) -> IndexResult<Option<Meta>> {
    validate_singletons(db)?;
    let meta: Option<Meta> = payload(db, "SELECT payload FROM index_meta", &[], 8192)?;
    if let Some(m) = &meta {
        m.fence.validate()?;
        m.identity.validate()?;
        if m.revision < 0 {
            return Err(IndexError::InvalidJournal);
        }
    }
    Ok(meta)
}
fn save_meta(db: &Connection, m: &Meta) -> IndexResult<()> {
    execute(
        db,
        "INSERT INTO index_meta VALUES(1,?) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
        &[encode(m, 8192)?.into()],
    )
}
fn save_op(db: &Connection, op: &Operation) -> IndexResult<()> {
    op.validate()?;
    execute(
        db,
        "UPDATE operations SET payload=? WHERE id=1",
        &[encode(op, MAX_RECORD)?.into()],
    )
}
fn load_op(db: &Connection) -> IndexResult<Option<Operation>> {
    validate_singletons(db)?;
    let op: Option<Operation> = payload(db, "SELECT payload FROM operations", &[], MAX_RECORD)?;
    if let Some(o) = &op {
        o.validate()?;
    }
    Ok(op)
}
fn validate_singletons(db: &Connection) -> IndexResult<()> {
    // CHECK constraints are not evidence about stored rows. Inspect both
    // headers before either payload can authorize bootstrap or recovery writes.
    // The same read transaction pins these headers and the later payload read.
    for table in ["index_meta", "operations"] {
        let headers = rows(
            db,
            &format!(
                "SELECT typeof(id), CASE WHEN typeof(id)='integer' THEN id END, typeof(payload) FROM {table} LIMIT 2"
            ),
            &[],
            2,
        )?;
        match headers.as_slice() {
            [] => (), // Fresh metadata and an absent operation are legitimate.
            [row] if matches!(row.as_slice(), [Value::String(key_type), Value::Integer(1), Value::String(payload_type)] if key_type == "integer" && payload_type == "text") =>
                {}
            _ => return Err(IndexError::InvalidJournal),
        }
    }
    Ok(())
}
fn active(db: &Connection, paper: &str) -> IndexResult<Option<Version>> {
    let v: Option<Version> = payload(
        db,
        "SELECT payload FROM active_papers WHERE paper_id=?",
        &[paper.into()],
        MAX_VERSION,
    )?;
    if let Some(v) = &v {
        let (r, g) = v.validate()?;
        let rows = rows(
            db,
            "SELECT version_id,node_id FROM paper_node_memberships WHERE paper_id=? ORDER BY node_id LIMIT 8193",
            &[paper.into()],
            8193,
        )?;
        let ids: BTreeSet<_> = rows
            .iter()
            .map(|r| text(&r[1]).map(str::to_owned))
            .collect::<IndexResult<_>>()?;
        let version = rows_first(
            db,
            "SELECT version_id FROM active_papers WHERE paper_id=?",
            &[paper.into()],
        )?;
        if r.paper_id() != paper
            || ids != membership(g.paper()).1
            || rows.iter().any(|r| text(&r[0]).ok() != Some(v.id.as_str()))
            || version.as_deref() != Some(&v.id)
        {
            return Err(IndexError::InvalidJournal);
        }
    } else if !rows(
        db,
        "SELECT node_id FROM paper_node_memberships WHERE paper_id=? LIMIT 1",
        &[paper.into()],
        1,
    )?
    .is_empty()
    {
        return Err(IndexError::InvalidJournal);
    }
    Ok(v)
}
fn authority(db: &Connection, scope: &Scope) -> IndexResult<BTreeMap<String, String>> {
    scope.validate()?;
    let mut affected = BTreeMap::new();
    for n in &scope.nodes {
        let label: Option<Label> = payload(
            db,
            "SELECT payload FROM global_labels WHERE node_id=?",
            &[n.id.clone().into()],
            8192,
        )?;
        if label.as_ref().map(|l| &l.name) != n.name.as_ref() {
            return Err(IndexError::InvalidJournal);
        }
        if let Some(l) = label {
            uuid(&l.writer)?;
            if l.sequence <= 0
                || l.sequence
                    > load_meta(db)?
                        .ok_or(IndexError::InvalidJournal)?
                        .fence
                        .generation
            {
                return Err(IndexError::InvalidJournal);
            }
        }
        let rs = rows(
            db,
            "SELECT paper_id,version_id FROM paper_node_memberships WHERE node_id=? ORDER BY paper_id LIMIT 4097",
            &[n.id.clone().into()],
            4097,
        )?;
        let papers = rs
            .iter()
            .map(|r| text(&r[0]).map(str::to_owned))
            .collect::<IndexResult<Vec<_>>>()?;
        if papers != n.papers {
            return Err(IndexError::InvalidJournal);
        }
        for r in rs {
            let id = text(&r[0])?;
            let version = text(&r[1])?;
            if !affected.contains_key(id) {
                let a = active(db, id)?.ok_or(IndexError::InvalidJournal)?;
                if a.id != version
                    || a.validate()?.1.paper().map(|p| &p.title) != scope.titles.get(id)
                {
                    return Err(IndexError::InvalidJournal);
                }
                affected.insert(id.into(), version.into());
            } else if affected.get(id).map(String::as_str) != Some(version) {
                return Err(IndexError::InvalidJournal);
            }
        }
    }
    let current = active(db, &scope.paper_id)?;
    let graph = current
        .as_ref()
        .map(Version::validate)
        .transpose()?
        .map(|(_, g)| g);
    if membership(graph.as_ref().and_then(|g| g.paper())) != membership(scope.paper.as_ref()) {
        return Err(IndexError::InvalidJournal);
    }
    Ok(affected)
}
fn payload<T: Serialize + DeserializeOwned>(
    db: &Connection,
    sql: &str,
    args: &[Value],
    max: usize,
) -> IndexResult<Option<T>> {
    // Ask length first: do not copy an oversized SQLite value into Rust memory.
    let sized = sql.replacen("SELECT payload", "SELECT length(CAST(payload AS BLOB))", 1);
    let sizes = rows(db, &sized, args, 2)?;
    if sizes.len() > 1 {
        return Err(IndexError::InvalidJournal);
    }
    if let Some(r) = sizes.first() {
        if !matches!(r[0],Value::Integer(n) if n >= 0 && n <= max as i64) {
            return Err(IndexError::Limit);
        }
    } else {
        return Ok(None);
    }
    let rows = rows(db, sql, args, 1)?;
    Ok(Some(decode(text(&rows[0][0])?, max)?))
}
fn rows_first(db: &Connection, sql: &str, args: &[Value]) -> IndexResult<Option<String>> {
    rows(db, sql, args, 1)?
        .first()
        .map(|r| text(&r[0]).map(str::to_owned))
        .transpose()
}
fn rows(db: &Connection, sql: &str, args: &[Value], max: usize) -> IndexResult<Vec<Vec<Value>>> {
    let mut s = db.prepare(sql)?;
    for (i, v) in args.iter().enumerate() {
        s.bind((i + 1, v))?;
    }
    let mut rows = vec![];
    while s.next()? == State::Row {
        if rows.len() == max {
            return Err(IndexError::Limit);
        }
        rows.push(
            (0..s.column_count())
                .map(|i| s.read::<Value, _>(i))
                .collect::<Result<_, _>>()?,
        );
    }
    Ok(rows)
}
fn execute(db: &Connection, sql: &str, args: &[Value]) -> IndexResult<()> {
    let mut s = db.prepare(sql)?;
    for (i, v) in args.iter().enumerate() {
        s.bind((i + 1, v))?;
    }
    if s.next()? != State::Done {
        return Err(IndexError::InvalidJournal);
    }
    Ok(())
}
fn scalar(db: &Connection, sql: &str) -> IndexResult<i64> {
    match rows(db, sql, &[], 1)?.first().and_then(|r| r.first()) {
        Some(Value::Integer(n)) => Ok(*n),
        _ => Err(IndexError::InvalidJournal),
    }
}
fn text(v: &Value) -> IndexResult<&str> {
    if let Value::String(s) = v {
        Ok(s)
    } else {
        Err(IndexError::InvalidJournal)
    }
}
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
#[cfg(unix)]
fn lock_file(root: &Path, path: &Path) -> IndexResult<File> {
    use std::os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    };
    validate_database_path(root, path).map_err(|_| IndexError::InvalidPath)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if file.metadata()?.nlink() != 1 || !file.metadata()?.is_file() {
        return Err(IndexError::InvalidPath);
    }
    // SAFETY: owned live descriptor; advisory exclusive nonblocking process lease.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(IndexError::Conflict);
    }
    Ok(file)
}
#[cfg(not(unix))]
fn lock_file(_: &Path, _: &Path) -> IndexResult<File> {
    Err(IndexError::InvalidPath)
}
