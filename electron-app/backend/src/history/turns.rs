//! Immutable request identity and canonical-first exchange eligibility. Mutable
//! indexing phases belong ONLY to the shared index-state journal.
use super::{SqliteHistory, schema::*, store::*};
use crate::{domain::SourceRecord, runtime::Cancellation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlite::{Connection, Value};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRequest {
    pub turn_id: String,
    pub session_id: String,
    pub message: String,
}
impl TurnRequest {
    pub fn validate(&self) -> HistoryResult<()> {
        for id in [&self.turn_id, &self.session_id] {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
            {
                return Err(HistoryError::InvalidInput);
            }
        }
        nonempty(&self.message)?;
        if self.message.len() > 64 * 1024 {
            return Err(HistoryError::InvalidInput);
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> HistoryResult<String> {
        self.validate()?;
        digest(&(1, &self.session_id, &self.message, "adaptive-rag-v1"))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExchangeIntent {
    pub version: u32,
    pub exchange_id: i64,
    pub session_id: String,
    pub turn_id: String,
    pub request_fingerprint: String,
    pub canonical_digest: String,
}
#[derive(Clone)]
pub struct CommittedTurn {
    pub intent: ExchangeIntent,
    pub exchange: ExchangeRecord,
}
pub enum Admission {
    Owner,
    Existing,
}

impl SqliteHistory {
    pub fn admit_turn(&self, request: &TurnRequest, c: &Cancellation) -> HistoryResult<Admission> {
        let fingerprint = request.fingerprint()?;
        self.write(c, |db| {
            if request_row(db, request, &fingerprint)?.is_some() {
                return Ok(Admission::Existing);
            }
            execute(
                db,
                "INSERT INTO chat_turn_requests VALUES(?,?,?,NULL)",
                &[
                    request.turn_id.clone().into(),
                    request.session_id.clone().into(),
                    fingerprint.into(),
                ],
            )?;
            Ok(Admission::Owner)
        })
    }

    pub fn append_turn(
        &self,
        request: &TurnRequest,
        reply: &str,
        sources: &[SourceRecord],
        c: &Cancellation,
    ) -> HistoryResult<i64> {
        let fingerprint = request.fingerprint()?;
        nonempty(reply)?;
        validate_sources(sources)?;
        self.write(c, |db| {
            if request_row(db, request, &fingerprint)? != Some(Value::Null) {
                return Err(HistoryError::Conflict);
            }
            let id = insert_exchange(db, &request.session_id, &request.message, reply, sources)?;
            if id > 9_007_199_254_740_991 {
                return Err(HistoryError::InvalidInput);
            }
            let intent = ExchangeIntent {
                version: 1,
                exchange_id: id,
                session_id: request.session_id.clone(),
                turn_id: request.turn_id.clone(),
                request_fingerprint: fingerprint.clone(),
                canonical_digest: digest(&(
                    1,
                    id,
                    &request.session_id,
                    &request.turn_id,
                    &fingerprint,
                    &request.message,
                    reply,
                    sources,
                ))?,
            };
            execute(
                db,
                "INSERT INTO exchange_index_intents VALUES(?,?)",
                &[
                    id.into(),
                    serde_json::to_string(&intent)
                        .map_err(|_| HistoryError::InvalidInput)?
                        .into(),
                ],
            )?;
            execute(
                db,
                "UPDATE chat_turn_requests SET exchange_id=? WHERE turn_id=?",
                &[id.into(), request.turn_id.clone().into()],
            )?;
            Ok(id)
        })
    }

    /// Equality is checked in storage, never only in the renderer. None is a
    /// durable admission without a verified exchange, NOT permission to replay.
    pub fn get_turn_outcome(&self, request: &TurnRequest) -> HistoryResult<Option<CommittedTurn>> {
        let fingerprint = request.fingerprint()?;
        let db = self.connection()?;
        db.execute("BEGIN")?;
        match request_row(&db, request, &fingerprint)? {
            None => Err(HistoryError::NotFound),
            Some(Value::Null) => Ok(None),
            Some(value) => {
                let id = integer(&value)?;
                let committed = verified_intent(&db, id)?;
                if committed.intent.turn_id != request.turn_id
                    || committed.intent.session_id != request.session_id
                    || committed.intent.request_fingerprint != fingerprint
                    || committed.exchange.user_message.content != request.message
                {
                    return Err(HistoryError::InvalidSchema);
                }
                Ok(Some(committed))
            }
        }
    }
    pub fn verified_intent(&self, id: i64) -> HistoryResult<CommittedTurn> {
        let db = self.connection()?;
        db.execute("BEGIN")?;
        verified_intent(&db, id)
    }
    pub fn list_exchange_index_intents(
        &self,
        after: i64,
        limit: usize,
    ) -> HistoryResult<Vec<ExchangeIntent>> {
        if after < 0 || !(1..=256).contains(&limit) {
            return Err(HistoryError::InvalidInput);
        }
        let db = self.connection()?;
        db.execute("BEGIN")?;
        query(&db,"SELECT exchange_id FROM exchange_index_intents WHERE exchange_id>? ORDER BY exchange_id LIMIT ?", &[after.into(),(limit as i64).into()])?
            .into_iter().map(|r| Ok(verified_intent(&db, integer(&r[0])?)?.intent)).collect()
    }
}
fn request_row(
    db: &Connection,
    r: &TurnRequest,
    fingerprint: &str,
) -> HistoryResult<Option<Value>> {
    let rows = query(
        db,
        "SELECT session_id,request_fingerprint,exchange_id FROM chat_turn_requests WHERE turn_id=?",
        &[r.turn_id.clone().into()],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if rows.len() != 1 || text(&row[0])? != r.session_id || text(&row[1])? != fingerprint {
        return Err(HistoryError::Conflict);
    }
    Ok(Some(row[2].clone()))
}
fn verified_intent(db: &Connection, id: i64) -> HistoryResult<CommittedTurn> {
    let rows = query(
        db,
        "SELECT payload FROM exchange_index_intents WHERE exchange_id=? AND length(CAST(payload AS BLOB))<=8192",
        &[id.into()],
    )?;
    let row = rows.first().ok_or(HistoryError::InvalidSchema)?;
    let payload = text(&row[0])?;
    let intent: ExchangeIntent =
        serde_json::from_str(&payload).map_err(|_| HistoryError::InvalidSchema)?;
    if intent.version != 1
        || intent.exchange_id != id
        || serde_json::to_string(&intent).ok().as_deref() != Some(&payload)
    {
        return Err(HistoryError::InvalidSchema);
    }
    let rows = query(
        db,
        "SELECT session_id,role,content FROM messages WHERE exchange_id=? ORDER BY id",
        &[id.into()],
    )?;
    if rows.len() != 2
        || text(&rows[0][1])? != "user"
        || text(&rows[1][1])? != "assistant"
        || text(&rows[0][0])? != intent.session_id
        || text(&rows[1][0])? != intent.session_id
    {
        return Err(HistoryError::InvalidSchema);
    }
    let user = text(&rows[0][2])?;
    let reply = text(&rows[1][2])?;
    let sources = sources(db, id)?;
    if digest(&(
        1,
        id,
        &intent.session_id,
        &intent.turn_id,
        &intent.request_fingerprint,
        &user,
        &reply,
        &sources,
    ))? != intent.canonical_digest
    {
        return Err(HistoryError::InvalidSchema);
    }
    let request = TurnRequest {
        turn_id: intent.turn_id.clone(),
        session_id: intent.session_id.clone(),
        message: user.clone(),
    };
    if request.fingerprint()? != intent.request_fingerprint
        || request_row(db, &request, &intent.request_fingerprint)? != Some(id.into())
    {
        return Err(HistoryError::InvalidSchema);
    }
    let exchange = ExchangeRecord {
        exchange_id: id,
        session_id: intent.session_id.clone(),
        user_message: crate::domain::Message {
            role: "user".into(),
            content: user,
            exchange_id: Some(id),
            sources: vec![],
        },
        assistant_message: crate::domain::Message {
            role: "assistant".into(),
            content: reply,
            exchange_id: Some(id),
            sources,
        },
    };
    Ok(CommittedTurn { intent, exchange })
}
fn digest(value: &impl Serialize) -> HistoryResult<String> {
    Ok(
        Sha256::digest(serde_json::to_vec(value).map_err(|_| HistoryError::InvalidInput)?)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}
