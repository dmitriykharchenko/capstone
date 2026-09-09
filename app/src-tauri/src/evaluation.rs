use serde::{Deserialize, Serialize};
use serde_json::Value;

const CHUNK_LIMIT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EvaluationRetrievalType {
    Vector,
    Bm25,
    Hybrid,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluationRequestCase {
    question: String,
    grading_notes: String,
    sources: Vec<String>,
}

impl EvaluationRequestCase {
    pub(crate) fn validate(&self) -> bool {
        nonempty(&self.question)
            && nonempty(&self.grading_notes)
            && self.sources.iter().all(|source| nonempty(source))
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluationCaseInput {
    question: String,
    grading_notes: String,
    sources: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetrievedChunk {
    rank: u32,
    text: String,
    score: f64,
    paper_id: String,
    chunk_index: u32,
    filename: String,
    title: String,
    page: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum JudgeConfidence {
    High,
    Medium,
    Low,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
enum ChunkReference {
    Rank(u32),
    Label(String),
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JudgeCriterion {
    score: u8,
    confidence: JudgeConfidence,
    reasoning: String,
    flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relevant_chunk_ranks: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunks_used: Option<Vec<ChunkReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contradicting_claims: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    missing_elements: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unsupported_claims: Option<Vec<String>>,
}

#[derive(Clone, Copy)]
enum CriterionExtra {
    None,
    Ranks,
    Used,
    Contradictions,
    Missing,
    Unsupported,
}

impl JudgeCriterion {
    fn validate(&self, expected: CriterionExtra) -> bool {
        if self.score > 5
            || !nonempty(&self.reasoning)
            || !self.flags.iter().all(|flag| nonempty(flag))
        {
            return false;
        }
        let present = [
            self.relevant_chunk_ranks.is_some(),
            self.chunks_used.is_some(),
            self.contradicting_claims.is_some(),
            self.missing_elements.is_some(),
            self.unsupported_claims.is_some(),
        ];
        let expected_index = match expected {
            CriterionExtra::None => None,
            CriterionExtra::Ranks => Some(0),
            CriterionExtra::Used => Some(1),
            CriterionExtra::Contradictions => Some(2),
            CriterionExtra::Missing => Some(3),
            CriterionExtra::Unsupported => Some(4),
        };
        if present
            .iter()
            .enumerate()
            .any(|(index, value)| *value != (expected_index == Some(index)))
        {
            return false;
        }
        self.relevant_chunk_ranks.as_ref().map_or(true, |ranks| {
            ranks
                .iter()
                .all(|rank| (1..=CHUNK_LIMIT as u32).contains(rank))
        }) && self.chunks_used.as_ref().map_or(true, |references| {
            references.iter().all(|reference| match reference {
                ChunkReference::Rank(rank) => (1..=CHUNK_LIMIT as u32).contains(rank),
                ChunkReference::Label(label) => nonempty(label),
            })
        }) && self
            .contradicting_claims
            .as_ref()
            .map_or(true, valid_strings)
            && self.missing_elements.as_ref().map_or(true, valid_strings)
            && self.unsupported_claims.as_ref().map_or(true, valid_strings)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JudgeScores {
    context_precision: JudgeCriterion,
    context_recall: JudgeCriterion,
    relevant_chunk_ranking: JudgeCriterion,
    context_utilization: JudgeCriterion,
    semantic_similarity: JudgeCriterion,
    completeness: JudgeCriterion,
    hallucination_rate: JudgeCriterion,
}

impl JudgeScores {
    fn validate(&self) -> bool {
        self.context_precision.validate(CriterionExtra::None)
            && self.context_recall.validate(CriterionExtra::None)
            && self.relevant_chunk_ranking.validate(CriterionExtra::Ranks)
            && self.context_utilization.validate(CriterionExtra::Used)
            && self
                .semantic_similarity
                .validate(CriterionExtra::Contradictions)
            && self.completeness.validate(CriterionExtra::Missing)
            && self
                .hallucination_rate
                .validate(CriterionExtra::Unsupported)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluationCaseError {
    stage: String,
    message: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluationCaseResult {
    case_index: u32,
    input: EvaluationCaseInput,
    retrieval_strategy: EvaluationRetrievalType,
    retrieved_chunks: Vec<RetrievedChunk>,
    generated_answer: Option<String>,
    judge_scores: Option<JudgeScores>,
    error: Option<EvaluationCaseError>,
}

impl EvaluationCaseResult {
    fn validate(&self, success: bool) -> bool {
        let input_valid = nonempty(&self.input.question)
            && nonempty(&self.input.grading_notes)
            && self.input.sources.iter().all(|source| nonempty(source));
        let chunks_valid = self
            .retrieved_chunks
            .iter()
            .enumerate()
            .all(|(index, chunk)| {
                chunk.rank == (index + 1) as u32
                    && nonempty(&chunk.text)
                    && chunk.score.is_finite()
                    && nonempty(&chunk.paper_id)
                    && nonempty(&chunk.filename)
                    && nonempty(&chunk.title)
                    && chunk.page > 0
            });
        input_valid
            && chunks_valid
            && if success {
                self.error.is_none()
                    && self.retrieved_chunks.len() == CHUNK_LIMIT
                    && self
                        .generated_answer
                        .as_ref()
                        .is_some_and(|answer| nonempty(answer))
                    && self
                        .judge_scores
                        .as_ref()
                        .is_some_and(JudgeScores::validate)
            } else {
                self.error
                    .as_ref()
                    .is_some_and(|error| nonempty(&error.stage) && nonempty(&error.message))
            }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AggregateScores {
    context_precision: Option<f64>,
    context_recall: Option<f64>,
    relevant_chunk_ranking: Option<f64>,
    context_utilization: Option<f64>,
    semantic_similarity: Option<f64>,
    completeness: Option<f64>,
    hallucination_rate: Option<f64>,
}

impl AggregateScores {
    fn validate(&self) -> bool {
        [
            self.context_precision,
            self.context_recall,
            self.relevant_chunk_ranking,
            self.context_utilization,
            self.semantic_similarity,
            self.completeness,
            self.hallucination_rate,
        ]
        .iter()
        .all(|score| {
            score.map_or(true, |value| {
                value.is_finite() && (0.0..=5.0).contains(&value)
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(
    tag = "event",
    rename_all(deserialize = "snake_case", serialize = "camelCase"),
    rename_all_fields(deserialize = "snake_case", serialize = "camelCase"),
    deny_unknown_fields
)]
pub(crate) enum EvaluationStreamEvent {
    RunStart {
        retrieval_type: EvaluationRetrievalType,
        total: u32,
    },
    CaseStart {
        case_index: u32,
        total: u32,
        retrieval_type: EvaluationRetrievalType,
        input: EvaluationCaseInput,
    },
    CaseSuccess {
        completed: u32,
        total: u32,
        result: EvaluationCaseResult,
    },
    CaseFailure {
        completed: u32,
        total: u32,
        result: EvaluationCaseResult,
    },
    Complete {
        retrieval_type: EvaluationRetrievalType,
        completed: u32,
        total: u32,
        succeeded: u32,
        failed: u32,
        aggregate_scores: AggregateScores,
    },
    Error {
        message: String,
    },
}

impl EvaluationStreamEvent {
    fn validate(&self) -> bool {
        match self {
            Self::RunStart { total, .. } => *total > 0,
            Self::CaseStart {
                case_index,
                total,
                input,
                ..
            } => {
                *case_index > 0
                    && case_index <= total
                    && nonempty(&input.question)
                    && nonempty(&input.grading_notes)
                    && input.sources.iter().all(|source| nonempty(source))
            }
            Self::CaseSuccess {
                completed,
                total,
                result,
            } => {
                *completed > 0
                    && completed <= total
                    && result.case_index == *completed
                    && result.validate(true)
            }
            Self::CaseFailure {
                completed,
                total,
                result,
            } => {
                *completed > 0
                    && completed <= total
                    && result.case_index == *completed
                    && result.validate(false)
            }
            Self::Complete {
                completed,
                total,
                succeeded,
                failed,
                aggregate_scores,
                ..
            } => {
                completed == total
                    && succeeded + failed == *total
                    && aggregate_scores.validate()
                    && if *succeeded == 0 {
                        serde_json::to_value(aggregate_scores).is_ok_and(|scores| {
                            scores
                                .as_object()
                                .is_some_and(|values| values.values().all(Value::is_null))
                        })
                    } else {
                        true
                    }
            }
            Self::Error { message } => nonempty(message),
        }
    }
}

pub(crate) fn evaluation_stream_event(value: Value) -> EvaluationStreamEvent {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        return EvaluationStreamEvent::Error {
            message: value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown evaluation error")
                .to_string(),
        };
    }
    match serde_json::from_value::<EvaluationStreamEvent>(value) {
        Ok(event) if event.validate() => event,
        _ => EvaluationStreamEvent::Error {
            message: "invalid evaluation progress event".to_string(),
        },
    }
}

fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}

fn valid_strings(values: &Vec<String>) -> bool {
    values.iter().all(|value| nonempty(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_snake_case_stream_events_to_camel_case_channel_contract() {
        let event = evaluation_stream_event(json!({
            "event": "case_failure",
            "completed": 1,
            "total": 2,
            "result": {
                "case_index": 1,
                "input": {"question": "q", "grading_notes": "g", "sources": ["a.pdf"]},
                "retrieval_strategy": "hybrid",
                "retrieved_chunks": [],
                "generated_answer": null,
                "judge_scores": null,
                "error": {"stage": "retrieval", "message": "no chunks"}
            }
        }));

        let serialized = serde_json::to_value(event).unwrap();
        assert_eq!(serialized["event"], "caseFailure");
        assert_eq!(serialized["result"]["case_index"], 1);
        assert_eq!(serialized["result"]["input"]["grading_notes"], "g");
        assert_eq!(serialized["result"]["retrieval_strategy"], "hybrid");
    }

    #[test]
    fn rejects_unknown_or_incomplete_progress_fields() {
        for value in [
            json!({"event": "run_start", "retrieval_type": "vector", "total": 1, "path": "/secret"}),
            json!({"event": "complete", "completed": 1, "total": 1}),
            json!({"event": "case_failure", "completed": 1, "total": 1, "result": {}}),
        ] {
            assert_eq!(
                evaluation_stream_event(value),
                EvaluationStreamEvent::Error {
                    message: "invalid evaluation progress event".to_string()
                }
            );
        }
    }

    #[test]
    fn request_cases_preserve_the_parsed_python_contract() {
        let request: EvaluationRequestCase = serde_json::from_value(json!({
            "question": "q", "grading_notes": "g", "sources": []
        }))
        .unwrap();
        assert!(request.validate());
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"question": "q", "grading_notes": "g", "sources": []})
        );
    }

    #[test]
    fn accepts_complete_success_with_full_provenance_and_judge_objects() {
        let base = json!({
            "score": 4, "confidence": "high", "reasoning": "supported", "flags": []
        });
        let chunks: Vec<Value> = (1..=5)
            .map(|rank| {
                json!({
                    "rank": rank,
                    "text": format!("full chunk {rank}"),
                    "score": 1.0 / f64::from(rank),
                    "paper_id": format!("paper-{rank}"),
                    "chunk_index": rank - 1,
                    "filename": format!("paper-{rank}.pdf"),
                    "title": format!("Paper {rank}"),
                    "page": rank,
                })
            })
            .collect();
        let mut ranking = base.clone();
        ranking["relevant_chunk_ranks"] = json!([1, 2]);
        let mut utilization = base.clone();
        utilization["chunks_used"] = json!([1, "chunk-2"]);
        let mut similarity = base.clone();
        similarity["contradicting_claims"] = json!([]);
        let mut completeness = base.clone();
        completeness["missing_elements"] = json!([]);
        let mut hallucination = base.clone();
        hallucination["unsupported_claims"] = json!([]);
        let event = evaluation_stream_event(json!({
            "event": "case_success", "completed": 1, "total": 1,
            "result": {
                "case_index": 1,
                "input": {"question": "q", "grading_notes": "g", "sources": ["a.pdf"]},
                "retrieval_strategy": "vector",
                "retrieved_chunks": chunks,
                "generated_answer": "answer",
                "judge_scores": {
                    "context_precision": base,
                    "context_recall": {"score": 4, "confidence": "high", "reasoning": "supported", "flags": []},
                    "relevant_chunk_ranking": ranking,
                    "context_utilization": utilization,
                    "semantic_similarity": similarity,
                    "completeness": completeness,
                    "hallucination_rate": hallucination
                },
                "error": null
            }
        }));

        let serialized = serde_json::to_value(event).unwrap();
        assert_eq!(serialized["event"], "caseSuccess");
        assert_eq!(
            serialized["result"]["retrieved_chunks"][4]["text"],
            "full chunk 5"
        );
        assert_eq!(
            serialized["result"]["judge_scores"]["context_utilization"]["chunks_used"][1],
            "chunk-2"
        );
    }
}
