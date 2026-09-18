use super::{HistoryError, HistoryResult};
use sqlite::{Connection, State, Value};

pub(super) const APP_ID: i64 = 0x43505354;
pub(super) const VERSION: i64 = 1;
pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS exchanges (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, exchange_id INTEGER REFERENCES exchanges(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS exchange_sources (exchange_id INTEGER NOT NULL REFERENCES exchanges(id) ON DELETE CASCADE, rank INTEGER NOT NULL, paper_id TEXT NOT NULL, chunk_index INTEGER NOT NULL, filename TEXT NOT NULL, title TEXT NOT NULL, page INTEGER NOT NULL, score REAL NOT NULL, snippet TEXT NOT NULL, PRIMARY KEY(exchange_id, rank));
CREATE TABLE IF NOT EXISTS paper_index_status (paper_id TEXT PRIMARY KEY, filename TEXT NOT NULL, status TEXT NOT NULL, chunk_count INTEGER NOT NULL DEFAULT 0, error TEXT, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS app_history_import (id INTEGER PRIMARY KEY CHECK(id=1), fingerprint TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id,id);
CREATE INDEX IF NOT EXISTS idx_messages_exchange_id ON messages(exchange_id,id);
";

pub(super) fn query(
    db: &Connection,
    sql: &str,
    bindings: &[Value],
) -> HistoryResult<Vec<Vec<Value>>> {
    let mut s = db.prepare(sql)?;
    for (i, value) in bindings.iter().enumerate() {
        s.bind((i + 1, value))?;
    }
    let mut rows = Vec::new();
    while s.next()? == State::Row {
        rows.push(
            (0..s.column_count())
                .map(|i| s.read::<Value, _>(i))
                .collect::<Result<_, _>>()?,
        );
    }
    Ok(rows)
}

pub(super) fn execute(db: &Connection, sql: &str, values: &[Value]) -> HistoryResult<()> {
    let mut s = db.prepare(sql)?;
    for (i, value) in values.iter().enumerate() {
        s.bind((i + 1, value))?;
    }
    if s.next()? != State::Done {
        return Err(HistoryError::InvalidSchema);
    }
    Ok(())
}
pub(super) fn integer(v: &Value) -> HistoryResult<i64> {
    if let Value::Integer(n) = v {
        Ok(*n)
    } else {
        Err(HistoryError::InvalidSchema)
    }
}
pub(super) fn text(v: &Value) -> HistoryResult<String> {
    if let Value::String(s) = v {
        Ok(s.clone())
    } else {
        Err(HistoryError::InvalidSchema)
    }
}
pub(super) fn optional_text(v: &Value) -> HistoryResult<Option<String>> {
    if matches!(v, Value::Null) {
        Ok(None)
    } else {
        text(v).map(Some)
    }
}
pub(super) fn scalar(db: &Connection, sql: &str) -> HistoryResult<i64> {
    integer(&query(db, sql, &[])?[0][0])
}

pub(super) fn check_schema(db: &Connection, legacy: bool) -> HistoryResult<()> {
    if query(db, "PRAGMA integrity_check", &[])? != vec![vec![Value::String("ok".into())]] {
        return Err(HistoryError::InvalidSchema);
    }
    for row in query(
        db,
        "SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
        &[],
    )? {
        let kind = text(&row[0])?;
        let name = text(&row[1])?;
        if kind == "index" {
            continue;
        }
        if kind != "table"
            || ![
                "messages",
                "exchanges",
                "exchange_sources",
                "paper_index_status",
                "app_history_import",
            ]
            .contains(&name.as_str())
        {
            return Err(HistoryError::InvalidSchema);
        }
    }
    for (table, required) in [
        (
            "messages",
            vec!["id", "session_id", "role", "content", "exchange_id"],
        ),
        ("exchanges", vec!["id", "session_id"]),
        (
            "exchange_sources",
            vec![
                "exchange_id",
                "rank",
                "paper_id",
                "chunk_index",
                "filename",
                "title",
                "page",
                "score",
                "snippet",
            ],
        ),
        (
            "paper_index_status",
            vec![
                "paper_id",
                "filename",
                "status",
                "chunk_count",
                "error",
                "updated_at",
            ],
        ),
        ("app_history_import", vec!["id", "fingerprint"]),
    ] {
        let rows = query(db, &format!("PRAGMA table_info({table})"), &[])?;
        if rows.is_empty() && legacy && table != "messages" {
            continue;
        }
        let names = rows
            .iter()
            .map(|r| text(&r[1]))
            .collect::<HistoryResult<Vec<_>>>()?;
        let expected = if legacy && table == "messages" && names.len() == 4 {
            &required[..4]
        } else {
            &required[..]
        };
        if names != expected {
            return Err(HistoryError::InvalidSchema);
        }
        let expected_pk = if table == "exchange_sources" {
            vec![1, 2, 0, 0, 0, 0, 0, 0, 0]
        } else {
            let mut pk = vec![0; rows.len()];
            pk[0] = 1;
            pk
        };
        if rows
            .iter()
            .map(|row| integer(&row[5]))
            .collect::<HistoryResult<Vec<_>>>()?
            != expected_pk
        {
            return Err(HistoryError::InvalidSchema);
        }
        if (table == "messages" || table == "exchanges")
            && text(&rows[0][2])?.to_uppercase() != "INTEGER"
        {
            return Err(HistoryError::InvalidSchema);
        }
    }
    Ok(())
}

pub(super) fn migrate(db: &Connection) -> HistoryResult<()> {
    let version = scalar(db, "PRAGMA user_version")?;
    let app = scalar(db, "PRAGMA application_id")?;
    if version > VERSION || ![0, APP_ID].contains(&app) || (version != 0 && app != APP_ID) {
        return Err(HistoryError::InvalidSchema);
    }
    check_schema(db, true)?;
    db.execute("BEGIN IMMEDIATE")?;
    let result = (|| {
        let columns = query(db, "PRAGMA table_info(messages)", &[])?;
        if columns.len() == 4 {
            db.execute("ALTER TABLE messages ADD COLUMN exchange_id INTEGER REFERENCES exchanges(id) ON DELETE CASCADE")?;
        }
        db.execute(SCHEMA)?;
        validate_data(db)?;
        db.execute(format!(
            "PRAGMA application_id={APP_ID}; PRAGMA user_version={VERSION}"
        ))?;
        db.execute("COMMIT")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = db.execute("ROLLBACK");
    }
    result
}

pub(super) fn validate_data(db: &Connection) -> HistoryResult<()> {
    check_schema(db, false)?;
    if !query(db, "PRAGMA foreign_key_check", &[])?.is_empty() {
        return Err(HistoryError::InvalidSchema);
    }
    for r in query(
        db,
        "SELECT id,session_id,role,content,exchange_id FROM messages ORDER BY id",
        &[],
    )? {
        if integer(&r[0])? <= 0 {
            return Err(HistoryError::InvalidSchema);
        }
        super::store::nonempty(&text(&r[1])?)?;
        super::store::valid_role(&text(&r[2])?)?;
        super::store::nonempty(&text(&r[3])?)?;
        if !matches!(r[4], Value::Null) && integer(&r[4])? <= 0 {
            return Err(HistoryError::InvalidSchema);
        }
    }
    if scalar(
        db,
        "SELECT COUNT(*) FROM messages m LEFT JOIN exchanges e ON m.exchange_id=e.id WHERE m.exchange_id IS NOT NULL AND (e.id IS NULL OR e.session_id != m.session_id)",
    )? != 0
        || scalar(
            db,
            "SELECT COUNT(*) FROM exchange_sources s LEFT JOIN exchanges e ON s.exchange_id=e.id WHERE e.id IS NULL",
        )? != 0
    {
        return Err(HistoryError::InvalidSchema);
    }
    for r in query(db, "SELECT id,session_id FROM exchanges ORDER BY id", &[])? {
        let id = integer(&r[0])?;
        if id <= 0 {
            return Err(HistoryError::InvalidSchema);
        }
        super::store::nonempty(&text(&r[1])?)?;
        let roles = query(
            db,
            "SELECT role FROM messages WHERE exchange_id=? ORDER BY id",
            &[id.into()],
        )?;
        if roles
            != vec![
                vec![Value::String("user".into())],
                vec![Value::String("assistant".into())],
            ]
        {
            return Err(HistoryError::InvalidSchema);
        }
        super::store::sources(db, id)?;
    }
    super::store::paper_statuses(db)?;
    Ok(())
}
