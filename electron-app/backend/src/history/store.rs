use super::schema::*;
use crate::{
    config::{validate_database_path, validate_root},
    domain::{Message, SourceRecord},
    protocol::ErrorCode,
    runtime::Cancellation,
};
use serde::{Deserialize, Serialize};
use sqlite::{Connection, OpenFlags, Value};
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryError {
    InvalidInput,
    InvalidPath,
    InvalidSchema,
    AlreadyExists,
    Storage,
    Cancelled,
    NotFound,
    Conflict,
}
pub type HistoryResult<T> = Result<T, HistoryError>;
impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "History operation failed ({self:?}).")
    }
}
impl std::error::Error for HistoryError {}
impl From<sqlite::Error> for HistoryError {
    fn from(_: sqlite::Error) -> Self {
        Self::Storage
    }
}
impl From<std::io::Error> for HistoryError {
    fn from(_: std::io::Error) -> Self {
        Self::Storage
    }
}
impl From<HistoryError> for ErrorCode {
    fn from(e: HistoryError) -> Self {
        match e {
            HistoryError::Cancelled => Self::Cancelled,
            HistoryError::InvalidInput | HistoryError::InvalidPath => Self::InvalidParams,
            _ => Self::Internal,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub session_id: String,
    pub title: String,
    pub message_count: u64,
    pub exchange_count: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExchangeRecord {
    pub exchange_id: i64,
    pub session_id: String,
    pub user_message: Message,
    pub assistant_message: Message,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PaperStatusRecord {
    pub paper_id: String,
    pub filename: String,
    pub status: String,
    pub chunk_count: u32,
    pub error: Option<String>,
    pub updated_at: String,
}

/// Connections are operation-local, serialized within a handle and transactional
/// across handles. Construction does not create a database: import before initialize.
#[derive(Clone)]
pub struct SqliteHistory {
    pub(super) root: PathBuf,
    pub(super) path: PathBuf,
    pub(super) gate: Arc<Mutex<()>>,
}
impl SqliteHistory {
    pub fn new(root: &Path, database: &Path) -> HistoryResult<Self> {
        Ok(Self {
            root: validate_root(root).map_err(|_| HistoryError::InvalidPath)?,
            path: validate_database_path(root, database).map_err(|_| HistoryError::InvalidPath)?,
            gate: Arc::new(Mutex::new(())),
        })
    }
    pub fn database_path(&self) -> &Path {
        &self.path
    }
    pub(super) fn check_path(&self) -> HistoryResult<()> {
        if validate_database_path(&self.root, &self.path).map_err(|_| HistoryError::InvalidPath)?
            != self.path
        {
            return Err(HistoryError::InvalidPath);
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            validate_database_path(
                &self.root,
                &PathBuf::from(format!(
                    "{}{suffix}",
                    self.path.to_str().ok_or(HistoryError::InvalidPath)?
                )),
            )
            .map_err(|_| HistoryError::InvalidPath)?;
        }
        Ok(())
    }
    pub(super) fn prepare_root(&self) -> HistoryResult<()> {
        self.check_path()?;
        fs::create_dir_all(&self.root)?;
        self.check_path()
    }
    pub(super) fn connection(&self) -> HistoryResult<Connection> {
        self.check_path()?;
        let mut db = Connection::open_with_flags(&self.path, OpenFlags::new().with_read_write())?;
        db.set_busy_timeout(5000)?;
        db.execute("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL")?;
        if scalar(&db, "PRAGMA application_id")? != APP_ID
            || scalar(&db, "PRAGMA user_version")? != VERSION
        {
            return Err(HistoryError::InvalidSchema);
        }
        Ok(db)
    }
    pub fn initialize(&self, cancellation: &Cancellation) -> HistoryResult<()> {
        let _guard = self.gate.lock().map_err(|_| HistoryError::Storage)?;
        check_cancel(cancellation)?;
        self.prepare_root()?;
        if self.path.exists() {
            let db = Connection::open_with_flags(&self.path, OpenFlags::new().with_read_write())?;
            if scalar(&db, "PRAGMA application_id")? == APP_ID
                && scalar(&db, "PRAGMA user_version")? == 1
            {
                db.execute("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL; BEGIN IMMEDIATE")?;
                validate_data(&db)?;
                db.execute(SCHEMA)?;
                db.execute("PRAGMA user_version=2")?;
                validate_data(&db)?;
                let _seal = seal(cancellation)?;
                db.execute("COMMIT")?;
            }
            drop(db);
            validate_data(&self.connection()?)?;
            return Ok(());
        }
        // Publish a complete new database just like an import, never expose a half schema.
        let temp = super::migration::Staging::new(&self.root)?;
        let db = sqlite::open(&temp.path)?;
        db.execute(SCHEMA)?;
        db.execute(format!(
            "PRAGMA application_id={APP_ID}; PRAGMA user_version={VERSION}"
        ))?;
        drop(db);
        self.check_path()?;
        temp.publish(&self.path, cancellation)?;
        Ok(())
    }
    pub fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        cancellation: &Cancellation,
    ) -> HistoryResult<()> {
        nonempty(session_id)?;
        nonempty(content)?;
        let role = role.trim().to_lowercase();
        valid_role(&role)?;
        self.write(cancellation, |db| {
            execute(
                db,
                "INSERT INTO messages(session_id,role,content) VALUES(?,?,?)",
                &[session_id.into(), role.into(), content.into()],
            )
        })
    }
    pub fn append_exchange_sync(
        &self,
        session_id: &str,
        user: &str,
        assistant: &str,
        sources: &[SourceRecord],
        cancellation: &Cancellation,
    ) -> HistoryResult<i64> {
        nonempty(session_id)?;
        nonempty(user)?;
        nonempty(assistant)?;
        validate_sources(sources)?;
        self.write(cancellation, |db| {
            insert_exchange(db, session_id, user, assistant, sources)
        })
    }
    pub(super) fn write<T>(
        &self,
        cancellation: &Cancellation,
        op: impl FnOnce(&Connection) -> HistoryResult<T>,
    ) -> HistoryResult<T> {
        let _guard = self.gate.lock().map_err(|_| HistoryError::Storage)?;
        check_cancel(cancellation)?;
        let db = self.connection()?;
        db.execute("BEGIN IMMEDIATE")?;
        let result = (|| {
            let result = op(&db)?;
            let _permit = seal(cancellation)?;
            db.execute("COMMIT")?;
            Ok(result)
        })();
        if result.is_err() {
            let _ = db.execute("ROLLBACK");
        }
        result
    }
    pub fn get_messages(&self, session_id: &str) -> HistoryResult<Vec<Message>> {
        nonempty(session_id)?;
        let db = self.connection()?;
        db.execute("BEGIN")?;
        let rows = query(
            &db,
            "SELECT role,content,exchange_id FROM messages WHERE session_id=? ORDER BY id",
            &[session_id.into()],
        )?;
        rows.into_iter()
            .map(|r| {
                let role = text(&r[0])?;
                let id = if r[2] == Value::Null {
                    None
                } else {
                    Some(integer(&r[2])?)
                };
                let sources = if role == "assistant" {
                    id.map(|id| sources(&db, id))
                        .transpose()?
                        .unwrap_or_default()
                } else {
                    vec![]
                };
                Ok(Message {
                    role,
                    content: text(&r[1])?,
                    exchange_id: id,
                    sources,
                })
            })
            .collect()
    }
    pub fn get_exchange(
        &self,
        exchange_id: i64,
        session_id: &str,
    ) -> HistoryResult<ExchangeRecord> {
        if exchange_id <= 0 {
            return Err(HistoryError::InvalidInput);
        }
        nonempty(session_id)?;
        let messages = self
            .get_messages(session_id)?
            .into_iter()
            .filter(|m| m.exchange_id == Some(exchange_id))
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Err(HistoryError::NotFound);
        }
        if messages.len() != 2 || messages[0].role != "user" || messages[1].role != "assistant" {
            return Err(HistoryError::InvalidSchema);
        }
        Ok(ExchangeRecord {
            exchange_id,
            session_id: session_id.into(),
            user_message: messages[0].clone(),
            assistant_message: messages[1].clone(),
        })
    }
    pub fn list_sessions(&self) -> HistoryResult<Vec<SessionRecord>> {
        let db = self.connection()?;
        query(&db, "SELECT session_id,COUNT(*),(SELECT COUNT(*) FROM exchanges e WHERE e.session_id=m.session_id),(SELECT content FROM messages u WHERE u.session_id=m.session_id AND role='user' ORDER BY id LIMIT 1) FROM messages m GROUP BY session_id ORDER BY MAX(id) DESC,session_id", &[])?.into_iter().map(|r| {
            let content = optional_text(&r[3])?.unwrap_or_default();
            let words = content.split_whitespace().collect::<Vec<_>>();
            let mut title = words.iter().take(6).copied().collect::<Vec<_>>().join(" ");
            if words.len() > 6 { title.push('…'); }
            Ok(SessionRecord { session_id: text(&r[0])?, title, message_count: integer(&r[1])? as u64, exchange_count: integer(&r[2])? as u64 })
        }).collect()
    }
    pub fn record_paper_status(
        &self,
        paper_id: &str,
        filename: &str,
        status: &str,
        chunk_count: u32,
        error: Option<&str>,
        cancellation: &Cancellation,
    ) -> HistoryResult<()> {
        nonempty(paper_id)?;
        nonempty(filename)?;
        nonempty(status)?;
        self.write(cancellation, |db| execute(db, "INSERT INTO paper_index_status(paper_id,filename,status,chunk_count,error,updated_at) VALUES(?,?,?,?,?,?) ON CONFLICT(paper_id) DO UPDATE SET filename=excluded.filename,status=excluded.status,chunk_count=excluded.chunk_count,error=excluded.error,updated_at=excluded.updated_at", &[
            paper_id.into(), filename.into(), status.into(), i64::from(chunk_count).into(), error.map(Value::from).unwrap_or(Value::Null), chrono::Utc::now().to_rfc3339().into()
        ]))
    }
    pub fn clear_paper_statuses(&self, cancellation: &Cancellation) -> HistoryResult<()> {
        self.write(cancellation, |db| {
            Ok(db.execute("DELETE FROM paper_index_status")?)
        })
    }
    pub fn get_paper_statuses(&self) -> HistoryResult<Vec<PaperStatusRecord>> {
        paper_statuses(&self.connection()?)
    }
}

pub(super) fn check_cancel(c: &Cancellation) -> HistoryResult<()> {
    c.check().map_err(|_| HistoryError::Cancelled)
}

pub(super) fn insert_exchange(
    db: &Connection,
    session: &str,
    user: &str,
    assistant: &str,
    sources: &[SourceRecord],
) -> HistoryResult<i64> {
    execute(
        db,
        "INSERT INTO exchanges(session_id) VALUES(?)",
        &[session.into()],
    )?;
    let id = scalar(db, "SELECT last_insert_rowid()")?;
    for (role, content) in [("user", user), ("assistant", assistant)] {
        execute(
            db,
            "INSERT INTO messages(session_id,role,content,exchange_id) VALUES(?,?,?,?)",
            &[session.into(), role.into(), content.into(), id.into()],
        )?;
    }
    for s in sources {
        execute(
            db,
            "INSERT INTO exchange_sources(exchange_id,rank,paper_id,chunk_index,filename,title,page,score,snippet) VALUES(?,?,?,?,?,?,?,?,?)",
            &[
                id.into(),
                i64::from(s.rank).into(),
                s.paper_id.clone().into(),
                i64::from(s.chunk_index).into(),
                s.filename.clone().into(),
                s.title.clone().into(),
                i64::from(s.page).into(),
                s.score.into(),
                s.snippet.clone().into(),
            ],
        )?;
    }
    Ok(id)
}
pub(super) fn seal(c: &Cancellation) -> HistoryResult<crate::runtime::CommitPermit> {
    c.begin_commit().map_err(|e| match e {
        ErrorCode::Cancelled => HistoryError::Cancelled,
        _ => HistoryError::Storage,
    })
}
pub(super) fn nonempty(s: &str) -> HistoryResult<()> {
    if s.trim().is_empty() {
        Err(HistoryError::InvalidInput)
    } else {
        Ok(())
    }
}
pub(super) fn valid_role(role: &str) -> HistoryResult<()> {
    if [
        "system",
        "user",
        "assistant",
        "human",
        "ai",
        "tool",
        "function",
        "developer",
    ]
    .contains(&role)
    {
        Ok(())
    } else {
        Err(HistoryError::InvalidInput)
    }
}
pub(super) fn validate_sources(sources: &[SourceRecord]) -> HistoryResult<()> {
    if sources.len() > 35 {
        return Err(HistoryError::InvalidInput);
    }
    for (i, s) in sources.iter().enumerate() {
        if s.rank as usize != i + 1
            || s.page == 0
            || !s.score.is_finite()
            || s.snippet.chars().count() > 280
        {
            return Err(HistoryError::InvalidInput);
        }
        for text in [&s.paper_id, &s.filename, &s.title, &s.snippet] {
            nonempty(text)?;
        }
    }
    Ok(())
}
pub(super) fn sources(db: &Connection, id: i64) -> HistoryResult<Vec<SourceRecord>> {
    let sources = query(db, "SELECT rank,paper_id,chunk_index,filename,title,page,score,snippet FROM exchange_sources WHERE exchange_id=? ORDER BY rank", &[id.into()])?.into_iter().map(|r| {
        let number = |i| u32::try_from(integer(&r[i])?).map_err(|_| HistoryError::InvalidSchema);
        let score = match r[6] { Value::Float(v) => v, Value::Integer(v) => v as f64, _ => return Err(HistoryError::InvalidSchema) };
        Ok(SourceRecord { rank: number(0)?, paper_id: text(&r[1])?, chunk_index: number(2)?, filename: text(&r[3])?, title: text(&r[4])?, page: number(5)?, score, snippet: text(&r[7])? })
    }).collect::<HistoryResult<Vec<_>>>()?;
    validate_sources(&sources)?;
    Ok(sources)
}
pub(super) fn paper_statuses(db: &Connection) -> HistoryResult<Vec<PaperStatusRecord>> {
    query(db, "SELECT paper_id,filename,status,chunk_count,error,updated_at FROM paper_index_status ORDER BY paper_id", &[])?.into_iter().map(|r| {
        let result = PaperStatusRecord { paper_id: text(&r[0])?, filename: text(&r[1])?, status: text(&r[2])?, chunk_count: u32::try_from(integer(&r[3])?).map_err(|_| HistoryError::InvalidSchema)?, error: optional_text(&r[4])?, updated_at: text(&r[5])? };
        for value in [&result.paper_id, &result.filename, &result.status, &result.updated_at] { nonempty(value)?; }
        Ok(result)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store(root: &Path) -> SqliteHistory {
        let store = SqliteHistory::new(root, &root.join("history.sqlite3")).unwrap();
        store.initialize(&Cancellation::default()).unwrap();
        store
    }

    #[test]
    fn cancellation_after_insert_before_commit_rolls_back() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let c = Cancellation::default();
        let result = store.write(&c, |db| {
            execute(
                db,
                "INSERT INTO messages(session_id,role,content) VALUES('s','user','not committed')",
                &[],
            )?;
            assert!(c.cancel());
            Ok(())
        });
        assert_eq!(result, Err(HistoryError::Cancelled));
        assert!(store.get_messages("s").unwrap().is_empty());
        assert_eq!(
            store.append_exchange_sync("s", "q", "a", &[], &c),
            Err(HistoryError::Cancelled)
        );
    }

    #[test]
    fn commit_failure_rolls_back_but_never_unseals_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let c = Cancellation::default();
        let result = store.write(&c, |db| {
            db.execute("PRAGMA defer_foreign_keys=ON")?;
            execute(db,"INSERT INTO messages(session_id,role,content,exchange_id) VALUES('s','user','orphan',999)",&[])
        });
        assert_eq!(result, Err(HistoryError::Storage));
        assert!(!c.cancel());
        assert!(store.get_messages("s").unwrap().is_empty());
        let fresh = Cancellation::default();
        store
            .append_exchange_sync("s", "q", "a", &[], &fresh)
            .unwrap();
        assert!(!fresh.cancel());
        assert_eq!(store.get_messages("s").unwrap().len(), 2);
    }
}
