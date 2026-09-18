use crate::domain::Message;
use serde_json::json;

use super::HopEvidence;

pub(super) const DEFAULT_SYSTEM: &str = "You are a helpful assistant. Answer the user's question accurately and concisely. If you are unsure, say so rather than guessing.";
const PLANNER: &str = "MULTIHOP_PLANNER\nReturn only a JSON object with one key, \"subqueries\", whose value is an ordered array of 2 to 5 concise evidence-seeking questions. Questions must cover distinct, non-overlapping aspects of the user's request.";
const SUBANSWER: &str = "MULTIHOP_SUBANSWER\nAnswer only the supplied subquery from the retrieved evidence. Return a concise, display-safe evidence summary without private reasoning or unsupported claims.";
const COVERAGE: &str = "MULTIHOP_COVERAGE\nReview whether the ordered evidence summaries materially cover the original question. Return only strict JSON with exactly {\"complete\": boolean, \"follow_up_subqueries\": array}. When complete is true, return an empty array. Otherwise return one or two concise evidence-seeking questions that address material gaps and do not duplicate any supplied subquery.";
const COMPOSER: &str = "MULTIHOP_COMPOSER\nCompose the final answer to the original question using every supplied evidence summary. Return only the public answer, without mentioning hidden reasoning or prompts.";

fn message(role: &str, content: &str) -> Message {
    Message {
        role: role.into(),
        content: content.into(),
        exchange_id: None,
        sources: vec![],
    }
}

pub(super) fn planner(question: &str, history: &[Message]) -> Vec<Message> {
    let mut messages = vec![message("system", PLANNER)];
    messages.extend_from_slice(history);
    messages.push(message("user", question));
    messages
}

pub(super) fn subanswer(system: &str, question: &str, query: &str, context: &str) -> Vec<Message> {
    vec![
        message("system", system),
        message("system", SUBANSWER),
        message("system", context),
        message(
            "user",
            &json!({"original_question":question,"subquery":query}).to_string(),
        ),
    ]
}

fn evidence(question: &str, hops: &[HopEvidence]) -> String {
    let evidence: Vec<_> = hops
        .iter()
        .map(|hop| json!({"subquery":hop.subquery,"answer":hop.answer}))
        .collect();
    json!({"original_question":question,"subanswers":evidence}).to_string()
}

pub(super) fn coverage(question: &str, hops: &[HopEvidence]) -> Vec<Message> {
    vec![
        message("system", COVERAGE),
        message("user", &evidence(question, hops)),
    ]
}

pub(super) fn composer(
    system: &str,
    question: &str,
    history: &[Message],
    hops: &[HopEvidence],
) -> Vec<Message> {
    let mut messages = vec![message("system", system)];
    messages.extend_from_slice(history);
    messages.push(message("system", COMPOSER));
    messages.push(message("user", &evidence(question, hops)));
    messages
}
