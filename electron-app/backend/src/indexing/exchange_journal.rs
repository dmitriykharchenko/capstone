use super::*;
use crate::{history::ExchangeIntent, retrieval::PreparedExchangeArtifact};
const MAX_EXCHANGE: usize = 4 * 1024 * 1024;
fn integer(v: &Value) -> IndexResult<i64> {
    match v {
        Value::Integer(i) => Ok(*i),
        _ => Err(IndexError::InvalidJournal),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(in crate::indexing) struct ExchangeEntry {
    pub intent: ExchangeIntent,
    pub epoch: u64,
    pub artifact: Option<PreparedExchangeArtifact>,
    pub complete: bool,
}
impl ExchangeEntry {
    pub fn validate(&self) -> IndexResult<()> {
        let i = &self.intent;
        if i.version != 1
            || i.exchange_id <= 0
            || i.session_id.is_empty()
            || i.session_id.len() > 128
            || i.turn_id.is_empty()
            || i.turn_id.len() > 128
            || (self.complete && self.artifact.is_none())
        {
            return Err(IndexError::InvalidJournal);
        }
        hash(&i.canonical_digest)?;
        hash(&i.request_fingerprint)?;
        if self
            .artifact
            .as_ref()
            .is_some_and(|a| a.exchange_id() != i.exchange_id)
        {
            return Err(IndexError::InvalidJournal);
        }
        Ok(())
    }
}
pub(super) fn entry(db: &Connection, id: i64) -> IndexResult<Option<ExchangeEntry>> {
    let e: Option<ExchangeEntry> = payload(
        db,
        "SELECT payload FROM exchange_entries WHERE exchange_id=?",
        &[id.into()],
        MAX_EXCHANGE,
    )?;
    if let Some(e) = &e {
        e.validate()?;
        if e.intent.exchange_id != id {
            return Err(IndexError::InvalidJournal);
        }
    }
    Ok(e)
}
pub(super) fn save_entry(db: &Connection, e: &ExchangeEntry) -> IndexResult<()> {
    e.validate()?;
    execute(
        db,
        "INSERT INTO exchange_entries VALUES(?,?) ON CONFLICT(exchange_id) DO UPDATE SET payload=excluded.payload",
        &[e.intent.exchange_id.into(), encode(e, MAX_EXCHANGE)?.into()],
    )
}
impl Journal {
    pub fn exchange_entry(&self, id: i64) -> IndexResult<Option<ExchangeEntry>> {
        entry(&self.connection(false)?, id)
    }
    pub fn next_exchange(&self, after: i64) -> IndexResult<Option<ExchangeEntry>> {
        let db = self.connection(false)?;
        if scalar(
            &db,
            "SELECT count(*) FROM exchange_entries WHERE typeof(exchange_id)!='integer' OR exchange_id<=0 OR typeof(payload)!='text'",
        )? != 0
        {
            return Err(IndexError::InvalidJournal);
        }
        let ids = rows(
            &db,
            "SELECT exchange_id FROM exchange_entries WHERE exchange_id>? ORDER BY exchange_id LIMIT 1",
            &[after.into()],
            1,
        )?;
        match ids.first() {
            Some(r) => entry(&db, integer(&r[0])?),
            None => Ok(None),
        }
    }
    pub fn scan_cursor(&self) -> IndexResult<i64> {
        let db = self.connection(false)?;
        let rows = rows(
            &db,
            "SELECT typeof(id),id,typeof(payload) FROM exchange_scan",
            &[],
            2,
        )?;
        if rows.is_empty() {
            return Ok(0);
        }
        if rows.len() != 1
            || text(&rows[0][0])? != "integer"
            || integer(&rows[0][1])? != 1
            || text(&rows[0][2])? != "text"
        {
            return Err(IndexError::InvalidJournal);
        }
        let cursor: i64 = payload(&db, "SELECT payload FROM exchange_scan", &[], 8192)?
            .ok_or(IndexError::InvalidJournal)?;
        if cursor < 0 {
            return Err(IndexError::InvalidJournal);
        }
        Ok(cursor)
    }
    pub fn seed_intents(
        &self,
        intents: &[ExchangeIntent],
        cursor: Option<i64>,
        c: &Cancellation,
    ) -> IndexResult<()> {
        if intents.len() > 256 {
            return Err(IndexError::Limit);
        }
        self.write(c, |db| {
            let meta = load_meta(db)?.ok_or(IndexError::InvalidJournal)?;
            for intent in intents {
                if let Some(e) = entry(db,intent.exchange_id)? {
                    if e.intent != *intent || e.epoch != meta.epoch { return Err(IndexError::InvalidJournal); }
                } else { save_entry(db,&ExchangeEntry { intent:intent.clone(),epoch:meta.epoch,artifact:None,complete:false })?; }
            }
            if let Some(cursor) = cursor {
                if intents.last().is_none_or(|i| i.exchange_id != cursor) { return Err(IndexError::InvalidJournal); }
                execute(db,"INSERT INTO exchange_scan VALUES(1,?) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", &[encode(&cursor,8192)?.into()])?;
            }
            Ok(())
        })
    }
}
