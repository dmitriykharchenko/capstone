//! The same closed wire schema as Python, with stateful validation before IPC.
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::OnceLock;

pub const ERROR: &str = "Chat interrupted. Resume or discard the pending run.";
pub const MAX_BYTES: usize = 1_048_576;
type Checked<T> = Result<T, &'static str>;

fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::from_str(include_str!("../../../src/orchestration_wire.json"))
            .expect("embedded schema")
    })
}

fn safe_text(value: &str) -> bool {
    let folded: String = value
        .chars()
        .map(|c| {
            if ('\u{ff01}'..='\u{ff5e}').contains(&c) {
                char::from_u32(c as u32 - 0xfee0).unwrap()
            } else {
                c
            }
        })
        .collect();
    let folded = folded
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    !schema()["private_markers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| folded.contains(m.as_str().unwrap()))
        && !value.chars().any(|c| {
            let n = c as u32;
            (n < 32 && !matches!(c, '\n' | '\r' | '\t'))
                || (127..=159).contains(&n)
                || (0x200b..=0x200f).contains(&n)
                || (0x202a..=0x202e).contains(&n)
                || (0x2060..=0x206f).contains(&n)
                || matches!(n, 0xfeff | 0xfffc)
        })
}

pub fn validate_type(value: &Value, name: &str) -> Checked<()> {
    let rule = &schema()["types"][name];
    let valid = match rule["kind"].as_str().ok_or(ERROR)? {
        "nullable" => {
            if !value.is_null() {
                validate_type(value, rule["item"].as_str().unwrap())?;
            }
            true
        }
        "object" => {
            fields(value, rule["fields"].as_object().unwrap())?;
            true
        }
        "array" => {
            let items = value.as_array().ok_or(ERROR)?;
            if items.len() > rule["max"].as_u64().unwrap() as usize {
                return Err(ERROR);
            }
            let mut keys = HashSet::new();
            for item in items {
                validate_type(item, rule["item"].as_str().unwrap())?;
                let key = if let Some(key) = rule["key"].as_str() {
                    &item[key]
                } else {
                    item
                };
                if (rule.get("key").is_some() || rule["unique"] == true)
                    && !keys.insert(key.to_string())
                {
                    return Err(ERROR);
                }
            }
            true
        }
        "id" => value.as_str().is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.as_bytes()[0].is_ascii_alphanumeric()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.:/-".contains(&b))
                && safe_text(s)
        }),
        "int" => value.as_u64().is_some_and(|n| {
            n >= rule["min"].as_u64().unwrap() && n <= rule["max"].as_u64().unwrap()
        }),
        "number" => value.as_f64().is_some_and(|n| {
            n.is_finite()
                && n >= rule["min"].as_f64().unwrap()
                && n <= rule["max"].as_f64().unwrap()
        }),
        "bool" => value.is_boolean(),
        "enum" => value.is_string() && rule["values"].as_array().unwrap().contains(value),
        "text" => value.as_str().is_some_and(|s| {
            s.len() >= rule["min"].as_u64().unwrap() as usize
                && s.len() <= rule["max"].as_u64().unwrap() as usize
                && (rule["min"] == 0 || !s.trim().is_empty())
                && safe_text(s)
        }),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ERROR)
    }
}

fn fields(value: &Value, fields: &Map<String, Value>) -> Checked<()> {
    let object = value.as_object().ok_or(ERROR)?;
    if object.len() != fields.len() {
        return Err(ERROR);
    }
    for (key, name) in fields {
        validate_type(object.get(key).ok_or(ERROR)?, name.as_str().unwrap())?;
    }
    Ok(())
}

pub fn validate_event(value: &Value) -> Checked<()> {
    let kind = value["event"].as_str().ok_or(ERROR)?;
    let extra = schema()["events"][kind].as_object().ok_or(ERROR)?;
    let mut expected = schema()["envelope"].as_object().unwrap().clone();
    expected.extend(extra.clone());
    let mut copy = value.as_object().ok_or(ERROR)?.clone();
    copy.remove("event");
    fields(&Value::Object(copy), &expected)?;
    if serde_json::to_vec(value).map_err(|_| ERROR)?.len() > MAX_BYTES {
        return Err(ERROR);
    }
    Ok(())
}

// Value's default deserializer silently accepts duplicate keys. Reject them at
// every nesting level before the allowlist validator sees the payload.
fn integer_fields() -> &'static HashSet<String> {
    static FIELDS: OnceLock<HashSet<String>> = OnceLock::new();
    FIELDS.get_or_init(|| {
        std::iter::once(schema()["envelope"].as_object().unwrap())
            .chain(
                schema()["events"]
                    .as_object()
                    .unwrap()
                    .values()
                    .filter_map(Value::as_object),
            )
            .chain(
                schema()["types"]
                    .as_object()
                    .unwrap()
                    .values()
                    .filter_map(|r| r["fields"].as_object()),
            )
            .flat_map(|fields| fields.iter())
            .filter(|(_, name)| {
                schema()["types"][name.as_str().unwrap()]["kind"] == "int"
                    || *name == "optional_exchange"
            })
            .map(|(key, _)| key.clone())
            .collect()
    })
}

struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("bounded JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Unique, E> {
                if v.is_finite() {
                    Ok(Unique(json!(v)))
                } else {
                    Err(E::custom(ERROR))
                }
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_none<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
                let mut items = Vec::new();
                while let Some(Unique(item)) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Unique(Value::Array(items)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Unique, A::Error> {
                let mut fields = Map::new();
                while let Some((key, Unique(value))) = map.next_entry::<String, Unique>()? {
                    // serde_json retains -0, fractions and exponents as floats;
                    // only unsigned decimal tokens produce a bounded integer.
                    if integer_fields().contains(&key)
                        && !value.is_null()
                        && value.as_u64().is_none()
                    {
                        return Err(de::Error::custom(ERROR));
                    }
                    if fields.insert(key, value).is_some() {
                        return Err(de::Error::custom(ERROR));
                    }
                }
                Ok(Unique(Value::Object(fields)))
            }
        }
        d.deserialize_any(V)
    }
}

pub fn parse(line: &str) -> Checked<Value> {
    if line.len() > MAX_BYTES {
        return Err(ERROR);
    }
    serde_json::from_str::<Unique>(line)
        .map(|v| v.0)
        .map_err(|_| ERROR)
}

fn array<'a>(v: &'a Value, key: &str) -> &'a Vec<Value> {
    v[key].as_array().unwrap()
}
fn terminal_work(v: &Value) -> bool {
    matches!(v.as_str(), Some("completed" | "failed" | "cancelled"))
}

fn question_transition(old: &Value, new: &Value) -> bool {
    matches!(
        (old.as_str(), new.as_str()),
        (Some("queued"), Some("running" | "cancelled"))
            | (
                Some("running"),
                Some("waiting" | "completed" | "failed" | "cancelled")
            )
            | (
                Some("waiting"),
                Some("running" | "completed" | "failed" | "cancelled")
            )
            | (Some("failed"), Some("queued"))
    )
}

fn ranked(items: &[Value]) -> Checked<()> {
    for (i, item) in items.iter().enumerate() {
        if item["source"]["rank"] != json!(i + 1) {
            return Err(ERROR);
        }
        if i > 0 {
            let prev = &items[i - 1];
            let left = (
                -prev["source"]["score"].as_f64().unwrap(),
                prev["source"]["paper_id"].as_str().unwrap(),
                prev["source"]["chunk_index"].as_u64().unwrap(),
                prev["source"]["snippet"].as_str().unwrap(),
                prev["evidence_id"].as_str().unwrap(),
            );
            let right = (
                -item["source"]["score"].as_f64().unwrap(),
                item["source"]["paper_id"].as_str().unwrap(),
                item["source"]["chunk_index"].as_u64().unwrap(),
                item["source"]["snippet"].as_str().unwrap(),
                item["evidence_id"].as_str().unwrap(),
            );
            if left > right {
                return Err(ERROR);
            }
        }
    }
    Ok(())
}

fn snapshot(state: &Value) -> Checked<()> {
    validate_type(state, "snapshot")?;
    for (name, key) in [("questions", "question_id"), ("agents", "agent_id")] {
        for item in array(state, name) {
            let parent = array(state, name)
                .iter()
                .find(|p| p[key] == item["parent_id"]);
            let depth = parent.map_or(0, |p| p["depth"].as_u64().unwrap() + 1);
            if item["depth"] != json!(depth) || (!item["parent_id"].is_null() && parent.is_none()) {
                return Err(ERROR);
            }
        }
    }
    let question_ids: HashSet<_> = array(state, "questions")
        .iter()
        .map(|q| q["question_id"].as_str().unwrap())
        .collect();
    let evidence_ids: HashSet<_> = array(state, "evidence")
        .iter()
        .map(|q| q["evidence_id"].as_str().unwrap())
        .collect();
    let agents = array(state, "agents");
    if agents.iter().filter(|a| a["status"] == "running").count() > 4 {
        return Err(ERROR);
    }
    for q in array(state, "questions") {
        if (q["status"] == "completed") == q["answer"].as_str().unwrap().trim().is_empty()
            || array(q, "evidence_ids")
                .iter()
                .any(|i| !evidence_ids.contains(i.as_str().unwrap()))
        {
            return Err(ERROR);
        }
    }
    for a in agents {
        if (a["kind"] == "question") == a["question_id"].is_null()
            || (!a["question_id"].is_null()
                && !question_ids.contains(a["question_id"].as_str().unwrap()))
        {
            return Err(ERROR);
        }
    }
    for e in array(state, "evidence") {
        if !e["question_id"].is_null() && !question_ids.contains(e["question_id"].as_str().unwrap())
        {
            return Err(ERROR);
        }
    }
    ranked(array(state, "evidence"))?;
    for t in array(state, "tools") {
        let a = agents
            .iter()
            .find(|a| a["agent_id"] == t["agent_id"])
            .ok_or(ERROR)?;
        if t["attempt"].as_u64() > a["attempt"].as_u64()
            || array(t, "evidence_ids")
                .iter()
                .any(|i| !evidence_ids.contains(i.as_str().unwrap()))
            || (!t["question_id"].is_null()
                && !question_ids.contains(t["question_id"].as_str().unwrap()))
        {
            return Err(ERROR);
        }
    }
    if state["status"] == "completed" && state["exchange_id"].is_null() {
        return Err(ERROR);
    }
    if !state["exchange_id"].is_null()
        && (!matches!(
            state["status"].as_str(),
            Some("running" | "interrupted" | "completed")
        ) || array(state, "questions")
            .iter()
            .chain(agents)
            .any(|x| !terminal_work(&x["status"]))
            || !agents
                .iter()
                .any(|a| a["kind"] == "finalizer" && a["status"] == "completed"))
    {
        return Err(ERROR);
    }
    Ok(())
}

pub struct Validator {
    identity: [String; 3],
    pub sequence: u64,
    pub terminal: bool,
    state: Option<Value>,
    started: HashMap<String, Value>,
    completed: HashSet<String>,
    question_retries: HashMap<String, String>,
    count: usize,
    final_text: String,
}

impl Validator {
    pub fn new(session: &str, run: &str, turn: &str) -> Checked<Self> {
        for id in [session, run, turn] {
            validate_type(&json!(id), "id")?;
        }
        Ok(Self {
            identity: [session.into(), run.into(), turn.into()],
            sequence: 0,
            terminal: false,
            state: None,
            started: HashMap::new(),
            completed: HashSet::new(),
            question_retries: HashMap::new(),
            count: 0,
            final_text: String::new(),
        })
    }

    pub fn accept(&mut self, event: &Value) -> Checked<()> {
        validate_event(event)?;
        let seq = event["sequence"].as_u64().unwrap();
        if self.terminal
            || self.count >= 16384
            || seq <= self.sequence
            || ["session_id", "run_id", "turn_id"]
                .iter()
                .zip(&self.identity)
                .any(|(k, v)| event[k] != *v)
        {
            return Err(ERROR);
        }
        let kind = event["event"].as_str().unwrap();
        if kind != "error" && seq >= schema()["max_sequence"].as_u64().unwrap() {
            return Err(ERROR);
        }
        if kind == "snapshot" {
            if self.state.is_some() || self.sequence != 0 {
                return Err(ERROR);
            }
            snapshot(&event["snapshot"])?;
            if seq != event["snapshot"]["checkpoint_sequence"].as_u64().unwrap() * 1024 + 900 {
                return Err(ERROR);
            }
            self.completed = array(&event["snapshot"], "tools")
                .iter()
                .map(|t| t["tool_call_id"].as_str().unwrap().to_owned())
                .collect();
            self.state = Some(event["snapshot"].clone());
        } else if kind != "error" {
            self.transition(event)?;
        }
        self.sequence = seq;
        self.count += 1;
        self.terminal = matches!(kind, "done" | "error");
        Ok(())
    }

    fn transition(&mut self, event: &Value) -> Checked<()> {
        let kind = event["event"].as_str().unwrap();
        let state = self.state.as_mut().ok_or(ERROR)?;
        if kind != "done"
            && (matches!(
                state["status"].as_str(),
                Some("completed" | "failed" | "discarded")
            ) || !state["exchange_id"].is_null())
        {
            return Err(ERROR);
        }
        if kind.starts_with("question_") || kind.starts_with("agent_") {
            let question = kind.starts_with("question_");
            let (name, key, field) = if question {
                ("questions", "question_id", "question")
            } else {
                ("agents", "agent_id", "agent")
            };
            let item = &event[field];
            let items = array(state, name);
            let old = items.iter().find(|i| i[key] == item[key]);
            if question {
                if (kind == "question_added") != old.is_none() {
                    return Err(ERROR);
                }
                if let Some(old) = old {
                    if ["text", "parent_id", "depth"]
                        .iter()
                        .any(|k| old[k] != item[k])
                        || !question_transition(&old["status"], &item["status"])
                        || self
                            .question_retries
                            .contains_key(item["question_id"].as_str().unwrap())
                    {
                        return Err(ERROR);
                    }
                }
                let expected = match kind {
                    "question_added" => Some("queued"),
                    "question_answered" => Some("completed"),
                    "question_failed" => Some("failed"),
                    _ => None,
                };
                if expected.is_some_and(|s| item["status"] != s) {
                    return Err(ERROR);
                }
                if old.is_some_and(|o| {
                    o == item
                        || (kind == "question_status"
                            && !matches!(
                                item["status"].as_str(),
                                Some("queued" | "running" | "waiting" | "cancelled")
                            ))
                }) {
                    return Err(ERROR);
                }
                let handlers: Vec<_> = array(state, "agents")
                    .iter()
                    .filter(|a| a["kind"] == "question" && a["question_id"] == item["question_id"])
                    .collect();
                if matches!(kind, "question_answered" | "question_failed")
                    && !handlers.iter().any(|a| a["status"] == "running")
                {
                    return Err(ERROR);
                }
                if old.is_some_and(|o| o["status"] == "failed") {
                    let eligible: Vec<_> = handlers
                        .iter()
                        .filter(|a| {
                            a["status"] == "failed"
                                && a["retry_count"].as_u64().unwrap()
                                    < schema()["types"]["retry"]["max"].as_u64().unwrap()
                        })
                        .collect();
                    if eligible.len() != 1 {
                        return Err(ERROR);
                    }
                    self.question_retries.insert(
                        item["question_id"].as_str().unwrap().into(),
                        eligible[0]["agent_id"].as_str().unwrap().into(),
                    );
                }
            } else {
                if kind == "agent_started"
                    && (item["status"] != "running" || old.is_some_and(|o| o["status"] != "queued"))
                {
                    return Err(ERROR);
                }
                if kind == "agent_completed"
                    && (old.is_none()
                        || old.unwrap()["status"] != "running"
                        || !terminal_work(&item["status"]))
                {
                    return Err(ERROR);
                }
                if kind == "agent_retried"
                    && (old.is_none()
                        || old.unwrap()["status"] != "failed"
                        || item["attempt"].as_u64().unwrap()
                            != old.unwrap()["attempt"].as_u64().unwrap() + 1
                        || item["retry_count"].as_u64().unwrap()
                            != old.unwrap()["retry_count"].as_u64().unwrap() + 1
                        || item["status"] != "queued")
                {
                    return Err(ERROR);
                }
                if let Some(old) = old {
                    if ["kind", "question_id", "parent_id", "depth"]
                        .iter()
                        .any(|k| old[k] != item[k])
                    {
                        return Err(ERROR);
                    }
                    if kind != "agent_retried"
                        && (item["attempt"] != old["attempt"]
                            || matches!(old["status"].as_str(), Some("completed" | "cancelled")))
                    {
                        return Err(ERROR);
                    }
                }
                if item["kind"] == "question" && matches!(kind, "agent_started" | "agent_retried") {
                    let question = array(state, "questions")
                        .iter()
                        .find(|q| q["question_id"] == item["question_id"])
                        .ok_or(ERROR)?;
                    let expected = if kind == "agent_started" {
                        "running"
                    } else {
                        "queued"
                    };
                    if question["status"] != expected {
                        return Err(ERROR);
                    }
                    let id = item["question_id"].as_str().unwrap();
                    if kind == "agent_retried" {
                        if self.question_retries.get(id).map(String::as_str)
                            != item["agent_id"].as_str()
                        {
                            return Err(ERROR);
                        }
                        self.question_retries.remove(id);
                    } else if self.question_retries.contains_key(id) {
                        return Err(ERROR);
                    }
                }
            }
            let items = state[name].as_array_mut().unwrap();
            items.retain(|i| i[key] != item[key]);
            items.push(item.clone());
        } else if kind.starts_with("tool_") {
            let tool = &event["tool"];
            let id = tool["tool_call_id"].as_str().unwrap();
            let agent = array(state, "agents")
                .iter()
                .find(|a| a["agent_id"] == tool["agent_id"])
                .ok_or(ERROR)?;
            if agent["status"] != "running"
                || tool["attempt"] != agent["attempt"]
                || self.completed.contains(id)
            {
                return Err(ERROR);
            }
            if kind == "tool_started" {
                if self.started.insert(id.into(), tool.clone()).is_some() {
                    return Err(ERROR);
                }
            } else {
                if self.started.remove(id).as_ref() != Some(tool) {
                    return Err(ERROR);
                }
                self.completed.insert(id.into());
                state["tools"].as_array_mut().unwrap().push(tool.clone());
            }
        } else if kind == "evidence" {
            if array(state, "evidence").iter().any(|old| {
                !array(event, "evidence")
                    .iter()
                    .any(|new| old["evidence_id"] == new["evidence_id"])
            }) {
                return Err(ERROR);
            }
            if array(state, "evidence").len() == array(event, "evidence").len() {
                return Err(ERROR);
            }
            for old in array(state, "evidence") {
                let item = array(event, "evidence")
                    .iter()
                    .find(|e| e["evidence_id"] == old["evidence_id"])
                    .ok_or(ERROR)?;
                if item["question_id"] != old["question_id"]
                    || old["source"]
                        .as_object()
                        .unwrap()
                        .iter()
                        .any(|(k, v)| k != "rank" && item["source"][k] != *v)
                {
                    return Err(ERROR);
                }
            }
            state["evidence"] = event["evidence"].clone();
        } else if kind == "final_delta" {
            if !array(state, "agents").iter().any(|a| {
                a["agent_id"] == event["agent_id"]
                    && a["kind"] == "finalizer"
                    && a["status"] == "running"
            }) {
                return Err(ERROR);
            }
            self.final_text.push_str(event["delta"].as_str().unwrap());
            validate_type(&json!(self.final_text), "reply")?;
        } else if kind == "done" {
            if !self.started.is_empty()
                || !self.question_retries.is_empty()
                || !matches!(
                    state["status"].as_str(),
                    Some("running" | "interrupted" | "completed")
                )
                || (!state["exchange_id"].is_null() && state["exchange_id"] != event["exchange_id"])
                || (!self.final_text.is_empty() && event["reply"] != self.final_text)
                || array(state, "questions")
                    .iter()
                    .chain(array(state, "agents"))
                    .any(|q| !terminal_work(&q["status"]))
                || !array(state, "agents")
                    .iter()
                    .any(|a| a["kind"] == "finalizer" && a["status"] == "completed")
            {
                return Err(ERROR);
            }
            if array(event, "sources")
                .iter()
                .enumerate()
                .any(|(i, s)| s["rank"] != json!(i + 1))
            {
                return Err(ERROR);
            }
            let mut keys = HashSet::new();
            for source in array(event, "sources") {
                if !keys.insert((
                    source["paper_id"].as_str().unwrap(),
                    source["chunk_index"].as_u64().unwrap(),
                )) || !array(state, "evidence").iter().any(|e| {
                    source
                        .as_object()
                        .unwrap()
                        .iter()
                        .all(|(k, v)| k == "rank" || e["source"][k] == *v)
                }) {
                    return Err(ERROR);
                }
            }
            state["exchange_id"] = event["exchange_id"].clone();
            state["status"] = json!("completed");
        }
        snapshot(state)
    }

    pub fn error(&self) -> Value {
        json!({"version":1,"session_id":self.identity[0],"run_id":self.identity[1],"turn_id":self.identity[2],"sequence":(self.sequence + 1).min(schema()["max_sequence"].as_u64().unwrap()),"event":"error","code":"invalid_stream","message":ERROR,"resumable":true})
    }
}

#[cfg(test)]
pub fn golden() -> Vec<Value> {
    include_str!("../../../tests/fixtures/orchestration.ndjson")
        .lines()
        .map(|line| parse(line).unwrap())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cases() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/orchestration-wire-cases.json"
        ))
        .unwrap()
    }

    fn update(value: &mut Value, changes: &Value) {
        if let Some(fields) = changes["set"].as_object() {
            for (pointer, replacement) in fields {
                *value.pointer_mut(pointer).unwrap() = replacement.clone();
            }
        }
        if let Some(keys) = changes["remove"].as_array() {
            for key in keys {
                value.as_object_mut().unwrap().remove(key.as_str().unwrap());
            }
        }
    }

    fn stream_case(case: &Value) -> Vec<Value> {
        let base = golden();
        let mut events = base[..case["prefix"].as_u64().unwrap_or(0) as usize].to_vec();
        if let Some(setup) = case.get("snapshot") {
            let after = setup["after"].as_u64().unwrap() as usize;
            let mut validator = Validator::new("session", "run", "turn").unwrap();
            for event in &base[..after] {
                validator.accept(event).unwrap();
            }
            let mut state = validator.state.unwrap();
            update(&mut state, setup);
            let checkpoint = base[after - 1]["sequence"].as_u64().unwrap() / 1024;
            state["checkpoint_sequence"] = json!(checkpoint);
            let mut event = base[0].clone();
            event["snapshot"] = state;
            event["sequence"] = json!(checkpoint * 1024 + 900);
            events = vec![event];
        }
        for step in array(case, "steps") {
            let index = step
                .as_u64()
                .unwrap_or_else(|| step["base"].as_u64().unwrap());
            let mut event = base[index as usize].clone();
            if step.is_object() {
                update(&mut event, step);
            }
            event["sequence"] = json!(events.last().unwrap()["sequence"].as_u64().unwrap() + 1);
            events.push(event);
        }
        events
    }

    fn accepts(events: &[Value]) -> bool {
        let mut validator = Validator::new("session", "run", "turn").unwrap();
        for event in events {
            if let Err(error) = parse(&event.to_string()).and_then(|e| validator.accept(&e)) {
                assert_eq!(error, ERROR);
                if !validator.terminal {
                    validator.accept(&validator.error()).unwrap();
                    assert!(validator.terminal);
                }
                return false;
            }
        }
        true
    }

    #[test]
    fn shared_integer_lexing() {
        let corpus = cases();
        let bindings = array(&corpus, "integer_fields");
        assert_eq!(
            bindings
                .iter()
                .map(|b| b[0].as_str().unwrap().to_owned())
                .collect::<HashSet<_>>(),
            *integer_fields()
        );
        assert_eq!(
            bindings
                .iter()
                .map(|b| b[1].as_str().unwrap())
                .collect::<HashSet<_>>(),
            schema()["types"]
                .as_object()
                .unwrap()
                .iter()
                .filter(|(_, r)| r["kind"] == "int")
                .map(|(n, _)| n.as_str())
                .collect()
        );
        for binding in bindings {
            let key = binding[0].as_str().unwrap();
            let name = binding[1].as_str().unwrap();
            let rule = &schema()["types"][name];
            let min = rule["min"].as_u64().unwrap();
            let max = rule["max"].as_u64().unwrap();
            let mut lexical = array(&corpus, "lexical").clone();
            lexical.extend([min, max, max + 1].map(|n| json!({"raw":n.to_string(),"integer":n})));
            for case in &lexical {
                let expected = case["integer"]
                    .as_u64()
                    .is_some_and(|n| min <= n && n <= max);
                for encoded in [
                    json!(key).to_string(),
                    format!("\"\\u{:04x}{}\"", key.as_bytes()[0], &key[1..]),
                ] {
                    let raw = format!(
                        "{{\"nested\":[{{{encoded}:{}}}]}}",
                        case["raw"].as_str().unwrap()
                    );
                    let result =
                        parse(&raw).and_then(|v| validate_type(&v["nested"][0][key], name));
                    assert_eq!(result.is_ok(), expected, "{key}/{name}: {raw}");
                    if !expected {
                        assert_eq!(result, Err(ERROR));
                    }
                }
            }
        }
        for raw in ["-0", "-1", "0.5", "1e2"] {
            assert!(parse(&format!("{{\"score\":{raw}}}"))
                .and_then(|v| validate_type(&v["score"], "score"))
                .is_ok());
        }
    }

    #[test]
    fn shared_stream_cases_and_question_permutations() {
        let corpus = cases();
        for case in array(&corpus, "streams") {
            assert_eq!(
                accepts(&stream_case(case)),
                case["valid"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
        let table = corpus["question_transitions"].as_object().unwrap();
        for (old, allowed) in table {
            for new in table.keys() {
                assert_eq!(
                    question_transition(&json!(old), &json!(new)),
                    allowed.as_array().unwrap().contains(&json!(new))
                );
            }
        }
        for a in [6, 7, 10] {
            for b in [6, 7, 10] {
                for c in [6, 7, 10] {
                    if a == b || b == c || a == c {
                        continue;
                    }
                    let case = json!({"prefix":6,"steps":[a,b,c]});
                    assert_eq!(accepts(&stream_case(&case)), [a, b, c] == [6, 7, 10]);
                }
            }
        }
    }

    #[test]
    fn shared_type_boundary_corpus() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/orchestration-types.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                validate_type(&case["value"], case["type"].as_str().unwrap()).is_ok(),
                case["valid"].as_bool().unwrap()
            );
        }
        for (name, rule) in schema()["types"].as_object().unwrap() {
            if rule["kind"] == "text" {
                let max = rule["max"].as_u64().unwrap() as usize;
                assert_eq!(validate_type(&json!("é".repeat(max / 2)), name), Ok(()));
                assert_eq!(
                    validate_type(&json!("é".repeat(max / 2 + 1)), name),
                    Err(ERROR)
                );
            } else if rule["kind"] == "array" {
                let values = vec![Value::Null; rule["max"].as_u64().unwrap() as usize + 1];
                assert_eq!(validate_type(&json!(values), name), Err(ERROR));
            }
        }
    }

    #[test]
    fn golden_python_stream_and_replay() {
        let events = golden();
        let mut validator = Validator::new("session", "run", "turn").unwrap();
        for event in &events {
            validator.accept(event).unwrap();
        }
        assert!(validator.terminal);
        assert_eq!(validator.accept(events.last().unwrap()), Err(ERROR));
    }

    #[test]
    fn reject_each_envelope_mutation_and_replay() {
        let events = golden();
        for position in 0..events.len() {
            for (field, value) in [
                ("sequence", json!(0)),
                ("run_id", json!("other")),
                ("version", json!(2)),
                ("unexpected", json!("private")),
            ] {
                let mut validator = Validator::new("session", "run", "turn").unwrap();
                for event in &events[..position] {
                    validator.accept(event).unwrap();
                }
                let mut bad = events[position].clone();
                bad[field] = value;
                assert_eq!(validator.accept(&bad), Err(ERROR));
            }
            let mut validator = Validator::new("session", "run", "turn").unwrap();
            for event in &events[..=position] {
                validator.accept(event).unwrap();
            }
            assert_eq!(validator.accept(&events[position]), Err(ERROR));
        }
    }

    #[test]
    fn reject_duplicate_keys_and_private_or_oversized_text() {
        for line in ["{\"a\":1,\"a\":2}", "{\"a\":{\"x\":1,\"x\":2}}", "{", "NaN"] {
            assert_eq!(parse(line), Err(ERROR));
        }
        assert_eq!(parse(&"x".repeat(MAX_BYTES + 1)), Err(ERROR));
        for text in [
            "chain_of_thought",
            "system prompt",
            "\u{200b}",
            "ｓｙｓｔｅｍ ｐｒｏｍｐｔ",
        ] {
            assert_eq!(validate_type(&json!(text), "reply"), Err(ERROR));
        }
        assert_eq!(validate_type(&json!("x".repeat(8193)), "reply"), Err(ERROR));
        assert_eq!(validate_type(&json!(true), "sequence"), Err(ERROR));
    }

    #[test]
    fn reject_tool_mismatch_and_nonfinalizer_delta() {
        for (position, field, value) in [
            (4, "tool_name", json!("answer_question")),
            (16, "agent_id", json!("intake")),
        ] {
            let events = golden();
            let mut validator = Validator::new("session", "run", "turn").unwrap();
            for event in &events[..position] {
                validator.accept(event).unwrap();
            }
            let mut bad = events[position].clone();
            if position == 4 {
                bad["tool"][field] = value;
            } else {
                bad[field] = value;
            }
            assert_eq!(validator.accept(&bad), Err(ERROR));
        }
    }
}
