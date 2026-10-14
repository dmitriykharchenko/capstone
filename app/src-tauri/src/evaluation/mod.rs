use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{ipc::Channel, State};

use crate::error::CommandError;
use crate::library::LibraryState;
use crate::{bridge, project_root, run_blocking};

mod metrics;
use metrics::{AggregateMetrics, EvaluationMetrics, PerformanceBudget, PerformanceChecks};

const MAX_AGGREGATE_CHUNK_LIMIT: usize = 35;
const TRANSPORT_ERROR_MESSAGE: &str =
    "Evaluation transport failed. Check the backend logs and retry.";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    performance_budget: Option<PerformanceBudget>,
}

impl EvaluationRequestCase {
    pub(crate) fn validate(&self) -> bool {
        nonempty(&self.question)
            && nonempty(&self.grading_notes)
            && self.sources.iter().all(|source| nonempty(source))
            && self
                .performance_budget
                .as_ref()
                .map_or(true, PerformanceBudget::validate)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluationCaseInput {
    question: String,
    grading_notes: String,
    sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    performance_budget: Option<PerformanceBudget>,
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
                .all(|rank| (1..=MAX_AGGREGATE_CHUNK_LIMIT as u32).contains(rank))
        }) && self.chunks_used.as_ref().map_or(true, |references| {
            references.iter().all(|reference| match reference {
                ChunkReference::Rank(rank) => (1..=MAX_AGGREGATE_CHUNK_LIMIT as u32).contains(rank),
                ChunkReference::Label(label) => nonempty(label),
            })
        }) && self
            .contradicting_claims
            .as_deref()
            .map_or(true, valid_strings)
            && self.missing_elements.as_deref().map_or(true, valid_strings)
            && self
                .unsupported_claims
                .as_deref()
                .map_or(true, valid_strings)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hop_index: Option<u32>,
}

impl EvaluationCaseError {
    fn reason(category: &str) -> Option<(&'static str, bool)> {
        Some(match category {
            "payment_required" => (
                "Provider payment is required. Check billing before retrying.",
                false,
            ),
            "authentication" => (
                "Provider authentication failed. Check credentials before retrying.",
                false,
            ),
            "authorization" => (
                "Provider access was denied. Check permissions before retrying.",
                false,
            ),
            "rate_limit" => ("Provider rate limit reached. Try again later.", true),
            "timeout" => ("The operation timed out. Try again later.", true),
            "connection" => (
                "The provider could not be reached. Check connectivity and retry.",
                true,
            ),
            "http_server_error" => (
                "The provider reported a server error. Try again later.",
                true,
            ),
            "http_client_error" => (
                "The provider rejected the request. Check configuration before retrying.",
                false,
            ),
            "incomplete_response" => (
                "The provider response was incomplete. Check output limits and retry.",
                true,
            ),
            "refusal" => ("The provider declined to answer this request.", false),
            "invalid_planner_output" => ("The planner returned an invalid plan. Try again.", true),
            "retrieval_failure" => ("Retrieval failed. Check the paper index and retry.", false),
            "invalid_judge_output" => ("The judge returned invalid scores. Try again.", true),
            "cancellation" => ("Evaluation was cancelled.", false),
            "configuration" => (
                "Evaluation configuration is invalid. Check configuration before retrying.",
                false,
            ),
            "unknown" => (
                "Evaluation failed for an unknown reason. Check configuration and retry.",
                false,
            ),
            _ => return None,
        })
    }

    fn status_category(status: u16) -> &'static str {
        match status {
            401 => "authentication",
            402 => "payment_required",
            403 => "authorization",
            408 => "timeout",
            429 => "rate_limit",
            500..=599 => "http_server_error",
            _ => "http_client_error",
        }
    }

    fn validate(&self) -> bool {
        let Some(category) = self.category.as_deref() else {
            return nonempty(&self.stage)
                && nonempty(&self.message)
                && self.http_status.is_none()
                && self.retryable.is_none()
                && self.retry_round.is_none()
                && self.stage_attempt.is_none()
                && self.hop_index.is_none();
        };
        let Some((_, retryable)) = Self::reason(category) else {
            return false;
        };
        let hop_stage = matches!(
            self.stage.as_str(),
            "retrieval" | "subanswer" | "followup_retrieval" | "followup_subanswer"
        );
        let stage_valid = hop_stage
            || matches!(
                self.stage.as_str(),
                "planner" | "coverage" | "composer" | "judge" | "model" | "evaluation"
            );
        let status_valid = match self.http_status {
            Some(status) => {
                (400..=599).contains(&status) && Self::status_category(status) == category
            }
            None => !matches!(
                category,
                "payment_required"
                    | "authentication"
                    | "authorization"
                    | "rate_limit"
                    | "http_server_error"
                    | "http_client_error"
            ),
        };
        stage_valid
            && status_valid
            && self.retryable == Some(retryable || self.http_status == Some(409))
            && self.retry_round.is_some_and(|round| {
                round <= 2
                    && self
                        .stage_attempt
                        .is_some_and(|attempt| attempt > 0 && attempt <= round + 1)
            })
            && if hop_stage {
                self.hop_index.is_some_and(|hop| (1..=7).contains(&hop))
            } else {
                self.hop_index.is_none()
            }
    }

    fn sanitize(mut self) -> Self {
        if let Some((message, _)) = self.category.as_deref().and_then(Self::reason) {
            self.message = message.to_string();
            return self;
        }
        let (stage, message) = match self.stage.as_str() {
            "planner" => (
                "planner",
                "Question planning failed for this case. Check backend configuration and retry.",
            ),
            "retrieval" => (
                "retrieval",
                "Retrieval failed for this case. Check the paper index and retry.",
            ),
            "subanswer" => (
                "subanswer",
                "Subanswer generation failed for this case. Check backend configuration and retry.",
            ),
            "coverage" => (
                "coverage",
                "Coverage review failed for this case. Check backend configuration and retry.",
            ),
            "followup_retrieval" => (
                "followup_retrieval",
                "Follow-up retrieval failed for this case. Check the paper index and retry.",
            ),
            "followup_subanswer" => (
                "followup_subanswer",
                "Follow-up subanswer generation failed for this case. Check backend configuration and retry.",
            ),
            "composer" => (
                "composer",
                "Final answer composition failed for this case. Check backend configuration and retry.",
            ),
            "model" => (
                "model",
                "Answer generation failed for this case. Check backend configuration and retry.",
            ),
            "judge" => (
                "judge",
                "Judge evaluation failed for this case. Check backend configuration and retry.",
            ),
            _ => (
                "evaluation",
                "Evaluation failed for this case. Check backend configuration and retry.",
            ),
        };
        self.stage = stage.to_string();
        self.message = message.to_string();
        self
    }
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attempt_history: Vec<EvaluationCaseError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metrics: Option<EvaluationMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    performance: Option<PerformanceChecks>,
}

impl EvaluationCaseResult {
    fn valid_history(&self) -> bool {
        if self.attempt_history.len() > if self.error.is_some() { 3 } else { 2 } {
            return false;
        }
        for (index, item) in self.attempt_history.iter().enumerate() {
            if item.category.is_none() || !item.validate() || item.retry_round != Some(index as u32)
            {
                return false;
            }
            let count = self.attempt_history[..=index]
                .iter()
                .filter(|prior| prior.stage == item.stage && prior.hop_index == item.hop_index)
                .count() as u32;
            if item.stage_attempt != Some(count) {
                return false;
            }
        }
        match &self.error {
            Some(error) if error.category.is_some() => self
                .attempt_history
                .last()
                .is_some_and(|last| last.clone().sanitize() == error.clone().sanitize()),
            Some(_) => self.attempt_history.is_empty(),
            None => true,
        }
    }

    fn valid_round(&self, round: u32) -> bool {
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.category.is_none())
        {
            return true;
        }
        self.attempt_history.len() == (round + u32::from(self.error.is_some())) as usize
    }

    fn sanitize(&mut self) {
        self.error = self.error.take().map(EvaluationCaseError::sanitize);
        self.attempt_history = std::mem::take(&mut self.attempt_history)
            .into_iter()
            .map(EvaluationCaseError::sanitize)
            .collect();
    }

    fn validate(&self, success: bool) -> bool {
        let input_valid = nonempty(&self.input.question)
            && nonempty(&self.input.grading_notes)
            && self.input.sources.iter().all(|source| nonempty(source))
            && self
                .input
                .performance_budget
                .as_ref()
                .map_or(true, PerformanceBudget::validate);
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
            && self.valid_history()
            && self
                .metrics
                .as_ref()
                .map_or(true, EvaluationMetrics::validate)
            && self
                .performance
                .as_ref()
                .map_or(true, PerformanceChecks::validate)
            && if success {
                self.error.is_none()
                    && (1..=MAX_AGGREGATE_CHUNK_LIMIT).contains(&self.retrieved_chunks.len())
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
                    .is_some_and(EvaluationCaseError::validate)
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
    RetryRoundStart {
        retry_round: u32,
        total: u32,
        pending: u32,
    },
    CaseRetryStart {
        retry_round: u32,
        case_index: u32,
        total: u32,
    },
    CaseRetryResult {
        retry_round: u32,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aggregate_metrics: Option<AggregateMetrics>,
    },
    Error {
        message: String,
    },
}

impl EvaluationStreamEvent {
    fn validate(&self) -> bool {
        match self {
            Self::RunStart { total, .. } => *total > 0,
            Self::RetryRoundStart {
                retry_round,
                total,
                pending,
            } => (1..=2).contains(retry_round) && *pending > 0 && pending <= total,
            Self::CaseRetryStart {
                retry_round,
                case_index,
                total,
            } => (1..=2).contains(retry_round) && *case_index > 0 && case_index <= total,
            Self::CaseRetryResult {
                retry_round,
                completed,
                total,
                result,
            } => {
                (1..=2).contains(retry_round)
                    && *total > 0
                    && completed == total
                    && result.case_index > 0
                    && result.case_index <= *total
                    && result.validate(result.error.is_none())
                    && result.valid_round(*retry_round)
            }
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
                    && result.valid_round(0)
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
                    && result.valid_round(0)
            }
            Self::Complete {
                completed,
                total,
                succeeded,
                failed,
                aggregate_scores,
                aggregate_metrics,
                ..
            } => {
                completed == total
                    && succeeded.checked_add(*failed) == Some(*total)
                    && aggregate_scores.validate()
                    && aggregate_metrics
                        .as_ref()
                        .map_or(true, |metrics| metrics.validate(*total))
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

    fn sanitize(self) -> Self {
        match self {
            Self::CaseSuccess {
                completed,
                total,
                mut result,
            } => {
                result.sanitize();
                Self::CaseSuccess {
                    completed,
                    total,
                    result,
                }
            }
            Self::CaseRetryResult {
                retry_round,
                completed,
                total,
                mut result,
            } => {
                result.sanitize();
                Self::CaseRetryResult {
                    retry_round,
                    completed,
                    total,
                    result,
                }
            }
            Self::CaseFailure {
                completed,
                total,
                mut result,
            } => {
                result.sanitize();
                Self::CaseFailure {
                    completed,
                    total,
                    result,
                }
            }
            Self::Error { .. } => evaluation_transport_error_event(),
            event => event,
        }
    }
}

pub(crate) fn evaluation_stream_event(value: Value) -> EvaluationStreamEvent {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        return match value.get("error").and_then(Value::as_str) {
            Some(message) if nonempty(message) => EvaluationStreamEvent::Error {
                message: message.to_string(),
            },
            _ => evaluation_transport_error_event(),
        };
    }
    match serde_json::from_value::<EvaluationStreamEvent>(value) {
        Ok(event) if event.validate() => event.sanitize(),
        _ => EvaluationStreamEvent::Error {
            message: "invalid evaluation progress event".to_string(),
        },
    }
}

pub(crate) fn evaluation_transport_error_event() -> EvaluationStreamEvent {
    EvaluationStreamEvent::Error {
        message: evaluation_transport_error_message().to_string(),
    }
}

pub(crate) fn evaluation_transport_error_message() -> &'static str {
    TRANSPORT_ERROR_MESSAGE
}

fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}

fn valid_strings(values: &[String]) -> bool {
    values.iter().all(|value| nonempty(value))
}

#[tauri::command]
pub(crate) async fn evaluate_stream(
    retrieval_type: EvaluationRetrievalType,
    test_cases: Vec<EvaluationRequestCase>,
    on_event: Channel<EvaluationStreamEvent>,
    state: State<'_, LibraryState>,
) -> Result<(), CommandError> {
    evaluate_stream_with_state(retrieval_type, test_cases, on_event, &state).await
}

async fn evaluate_stream_with_state(
    retrieval_type: EvaluationRetrievalType,
    test_cases: Vec<EvaluationRequestCase>,
    on_event: Channel<EvaluationStreamEvent>,
    state: &LibraryState,
) -> Result<(), CommandError> {
    if test_cases.is_empty() || test_cases.iter().any(|case| !case.validate()) {
        return Err(CommandError::Bridge(
            "test cases must be a non-empty array of valid cases".to_string(),
        ));
    }
    let operation = state.begin_operation()?;
    let root = project_root();
    run_blocking(move || {
        let request = serde_json::json!({
            "command": "evaluate",
            "stream": true,
            "retrieval_type": retrieval_type,
            "test_cases": test_cases,
        });
        let mut terminal_event_seen = false;
        let result =
            bridge::call_streaming(&root, operation.library_directory(), request, |value| {
                let event = evaluation_stream_event(value);
                terminal_event_seen |= matches!(
                    event,
                    EvaluationStreamEvent::Complete { .. } | EvaluationStreamEvent::Error { .. }
                );
                let _ = on_event.send(event);
            });
        if let Err(error) = result {
            if !terminal_event_seen {
                let _ = on_event.send(EvaluationStreamEvent::Error {
                    message: error.to_string(),
                });
            }
            return Err(error);
        }
        if !terminal_event_seen {
            let message = "evaluation stream ended before terminal completion".to_string();
            let _ = on_event.send(EvaluationStreamEvent::Error {
                message: message.clone(),
            });
            return Err(CommandError::InvalidResponse(message));
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::test_support::{run_with_fake_python, test_library_state};

    #[cfg(unix)]
    #[test]
    fn evaluation_persistence_outcomes_have_one_terminal_channel_frame() {
        let input = serde_json::json!({"question": "q", "grading_notes": "g", "sources": []});
        let progress = [
            serde_json::json!({"event": "run_start", "retrieval_type": "hybrid", "total": 1}),
            serde_json::json!({
                "event": "case_start", "case_index": 1, "total": 1,
                "retrieval_type": "hybrid", "input": input,
            }),
            serde_json::json!({
                "event": "case_failure", "completed": 1, "total": 1,
                "result": {
                    "case_index": 1, "input": input, "retrieval_strategy": "hybrid",
                    "retrieved_chunks": [], "generated_answer": null, "judge_scores": null,
                    "error": {"stage": "judge", "message": "Judge evaluation failed for this case. Check backend configuration and retry."},
                },
            }),
        ];
        let complete = serde_json::json!({
            "event": "complete", "retrieval_type": "hybrid", "completed": 1,
            "total": 1, "succeeded": 0, "failed": 1,
            "aggregate_scores": {
                "context_precision": null, "context_recall": null,
                "relevant_chunk_ranking": null, "context_utilization": null,
                "semantic_similarity": null, "completeness": null, "hallucination_rate": null,
            },
        });
        // Mirror the Python persistence boundary: success completes; a failed
        // write emits only the sanitized error and exits nonzero.
        for (terminal, exit_code, terminal_name) in [
            (complete, 0, "complete"),
            (
                serde_json::json!({"ok": false, "error": evaluation_transport_error_message()}),
                1,
                "error",
            ),
        ] {
            let state = test_library_state();
            let frames = progress.iter().chain(std::iter::once(&terminal));
            let mut contents = String::from("#!/bin/sh\n");
            for frame in frames {
                contents.push_str(&format!("printf '%s\\n' '{}'\n", frame));
            }
            contents.push_str(&format!("exit {exit_code}\n"));
            let (result, events) = run_with_fake_python(&contents, |channel| {
                tauri::async_runtime::block_on(evaluate_stream_with_state(
                    EvaluationRetrievalType::Hybrid,
                    vec![serde_json::from_value(input.clone()).unwrap()],
                    channel,
                    &state,
                ))
            });

            assert_eq!(result.is_ok(), exit_code == 0);
            let names: Vec<_> = events
                .iter()
                .map(|event| event["event"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                ["runStart", "caseStart", "caseFailure", terminal_name]
            );
            assert_eq!(events[1]["input"], input);
            assert_eq!(events[2]["result"], progress[2]["result"]);
            assert_eq!(
                events.last().unwrap(),
                &serde_json::to_value(evaluation_stream_event(terminal)).unwrap()
            );
            assert_eq!(
                names
                    .iter()
                    .filter(|name| matches!(**name, "complete" | "error"))
                    .count(),
                1
            );
            if let Err(error) = result {
                assert_eq!(
                    error.to_string(),
                    format!(
                        "python bridge failed: {}",
                        evaluation_transport_error_message()
                    )
                );
            }
        }
    }

    use serde_json::json;

    fn diagnostic(category: &str, status: Option<u16>, round: u32) -> Value {
        json!({"stage":"planner", "category":category, "http_status":status,
            "retryable":EvaluationCaseError::reason(category).unwrap().1 || status == Some(409),
            "retry_round":round, "stage_attempt":round + 1, "hop_index":null,
            "message":"synthetic-untrusted-diagnostic-canary"})
    }

    fn diagnostic_event(category: &str, status: Option<u16>, round: u32) -> Value {
        let history: Vec<_> = (0..=round)
            .map(|r| diagnostic(category, status, r))
            .collect();
        let mut event = json!({"event":"case_failure", "completed":1,"total":1,
            "result":{"case_index":1,"input":{"question":"q","grading_notes":"g","sources":[]},
                "retrieval_strategy":"hybrid","retrieved_chunks":[],"generated_answer":null,"judge_scores":null,
                "error":history.last(),"attempt_history":history}});
        if round > 0 {
            event["event"] = json!("case_retry_result");
            event["retry_round"] = json!(round);
        }
        event
    }

    #[test]
    fn structured_categories_and_402_history_are_sanitized() {
        for (category, status) in [
            ("payment_required", Some(402)),
            ("authentication", Some(401)),
            ("authorization", Some(403)),
            ("rate_limit", Some(429)),
            ("timeout", None),
            ("timeout", Some(408)),
            ("connection", None),
            ("http_server_error", Some(503)),
            ("http_client_error", Some(400)),
            ("http_client_error", Some(409)),
            ("incomplete_response", None),
            ("refusal", None),
            ("invalid_planner_output", None),
            ("retrieval_failure", None),
            ("invalid_judge_output", None),
            ("cancellation", None),
            ("configuration", None),
            ("unknown", None),
        ] {
            for round in 0..=2 {
                let serialized = serde_json::to_value(evaluation_stream_event(diagnostic_event(
                    category, status, round,
                )))
                .unwrap();
                assert_ne!(serialized["event"], "error", "{category}");
                let result = &serialized["result"];
                assert_eq!(result["error"]["category"], category);
                assert_eq!(result["error"]["http_status"], json!(status));
                assert_eq!(
                    result["attempt_history"].as_array().unwrap().len(),
                    (round + 1) as usize
                );
                assert_eq!(result["error"], result["attempt_history"][round as usize]);
                assert!(!serialized.to_string().contains("synthetic-untrusted"));
            }
        }
    }

    #[test]
    fn malformed_structured_diagnostics_are_rejected_without_raw_details() {
        for (field, value) in [
            ("category", json!("synthetic-untrusted")),
            ("stage", json!("synthetic-untrusted")),
            ("body", json!("synthetic-untrusted")),
            ("http_status", json!(401)),
            ("http_status", json!(true)),
            ("http_status", json!(null)),
            ("retryable", json!(true)),
            ("retry_round", json!(3)),
            ("retry_round", json!(0.5)),
            ("stage_attempt", json!(0)),
            ("stage_attempt", json!(4)),
            ("hop_index", json!(1)),
            ("message", json!({"raw":"synthetic-untrusted"})),
        ] {
            for target in ["error", "attempt_history"] {
                let mut value_event = diagnostic_event("payment_required", Some(402), 2);
                if target == "error" {
                    value_event["result"][target][field] = value.clone();
                } else {
                    value_event["result"][target][0][field] = value.clone();
                }
                let event = serde_json::to_value(evaluation_stream_event(value_event)).unwrap();
                assert_eq!(event["event"], "error", "{target}.{field}");
                assert!(!event.to_string().contains("synthetic-untrusted"));
            }
        }
        for kind in 0..5 {
            let mut value = diagnostic_event("payment_required", Some(402), 2);
            match kind {
                0 => value["result"]["attempt_history"]
                    .as_array_mut()
                    .unwrap()
                    .reverse(),
                1 => {
                    value["result"]["attempt_history"][1]["stage_attempt"] = json!(1);
                }
                2 => {
                    value["result"]["attempt_history"]
                        .as_array_mut()
                        .unwrap()
                        .push(diagnostic("payment_required", Some(402), 2));
                }
                3 => {
                    value["retry_round"] = json!(1);
                }
                _ => {
                    value["result"]["attempt_history"] = json!([]);
                }
            }
            assert!(matches!(
                evaluation_stream_event(value),
                EvaluationStreamEvent::Error { .. }
            ));
        }
    }

    #[test]
    fn moving_stage_and_hop_attempts_are_validated() {
        let mut value = diagnostic_event("payment_required", Some(402), 2);
        value["result"]["attempt_history"][0]["stage"] = json!("retrieval");
        value["result"]["attempt_history"][0]["hop_index"] = json!(1);
        value["result"]["attempt_history"][1]["stage"] = json!("retrieval");
        value["result"]["attempt_history"][1]["hop_index"] = json!(2);
        value["result"]["attempt_history"][1]["stage_attempt"] = json!(1);
        value["result"]["attempt_history"][2]["stage"] = json!("judge");
        value["result"]["attempt_history"][2]["stage_attempt"] = json!(1);
        value["result"]["error"] = value["result"]["attempt_history"][2].clone();
        assert!(matches!(
            evaluation_stream_event(value.clone()),
            EvaluationStreamEvent::CaseRetryResult { .. }
        ));
        value["result"]["attempt_history"][1]["hop_index"] = json!(8);
        assert!(matches!(
            evaluation_stream_event(value),
            EvaluationStreamEvent::Error { .. }
        ));
    }

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
                "error": {"stage": "retrieval", "message": "/private/secret/index unavailable"}
            }
        }));

        let serialized = serde_json::to_value(event).unwrap();
        assert_eq!(serialized["event"], "caseFailure");
        assert_eq!(serialized["result"]["case_index"], 1);
        assert_eq!(serialized["result"]["input"]["grading_notes"], "g");
        assert_eq!(serialized["result"]["retrieval_strategy"], "hybrid");
        assert_eq!(
            serialized["result"]["error"]["message"],
            "Retrieval failed for this case. Check the paper index and retry."
        );
        assert!(!serialized.to_string().contains("/private/secret"));
    }

    #[test]
    fn preserves_and_sanitizes_multihop_failure_stages() {
        for (stage, message) in [
            (
                "planner",
                "Question planning failed for this case. Check backend configuration and retry.",
            ),
            (
                "subanswer",
                "Subanswer generation failed for this case. Check backend configuration and retry.",
            ),
            (
                "coverage",
                "Coverage review failed for this case. Check backend configuration and retry.",
            ),
            (
                "followup_retrieval",
                "Follow-up retrieval failed for this case. Check the paper index and retry.",
            ),
            (
                "followup_subanswer",
                "Follow-up subanswer generation failed for this case. Check backend configuration and retry.",
            ),
            (
                "composer",
                "Final answer composition failed for this case. Check backend configuration and retry.",
            ),
        ] {
            let event = evaluation_stream_event(json!({
                "event": "case_failure",
                "completed": 1,
                "total": 2,
                "result": {
                    "case_index": 1,
                    "input": {"question": "q", "grading_notes": "g", "sources": []},
                    "retrieval_strategy": "hybrid",
                    "retrieved_chunks": [],
                    "generated_answer": null,
                    "judge_scores": null,
                    "error": {"stage": stage, "message": "secret backend detail"}
                }
            }));

            let serialized = serde_json::to_value(event).unwrap();
            assert_eq!(serialized["result"]["error"]["stage"], stage);
            assert_eq!(serialized["result"]["error"]["message"], message);
            assert!(!serialized.to_string().contains("secret"));
        }
    }

    #[test]
    fn forwards_python_errors_to_the_channel() {
        let event = evaluation_stream_event(json!({
            "ok": false,
            "error": "paper search index unavailable"
        }));
        assert_eq!(
            event,
            EvaluationStreamEvent::Error {
                message: "paper search index unavailable".to_string()
            }
        );

        let blank = evaluation_stream_event(json!({ "ok": false, "error": " " }));
        assert_eq!(blank, evaluation_transport_error_event());
    }

    #[test]
    fn accepts_bounded_retry_round_progress() {
        for round in 1..=2 {
            let start = evaluation_stream_event(json!({
                "event": "retry_round_start", "retry_round": round, "total": 3, "pending": 2
            }));
            let serialized = serde_json::to_value(start).unwrap();
            assert_eq!(serialized["event"], "retryRoundStart");
            assert_eq!(serialized["retryRound"], round);
            let case = evaluation_stream_event(json!({
                "event": "case_retry_start", "retry_round": round, "total": 3, "case_index": 1
            }));
            assert_eq!(
                serde_json::to_value(case).unwrap()["event"],
                "caseRetryStart"
            );
        }
    }

    #[test]
    fn retry_result_can_replace_earlier_case_without_increasing_completed() {
        let value = json!({
            "event": "case_retry_result", "retry_round": 2, "completed": 3, "total": 3,
            "result": {
                "case_index": 1,
                "input": {"question": "q", "grading_notes": "g", "sources": []},
                "retrieval_strategy": "hybrid", "retrieved_chunks": [],
                "generated_answer": null, "judge_scores": null,
                "error": {"stage": "followup_subanswer", "message": "private-provider-canary"}
            }
        });
        let event = serde_json::to_value(evaluation_stream_event(value.clone())).unwrap();
        assert_eq!(event["event"], "caseRetryResult");
        assert_eq!(event["completed"], 3);
        assert_eq!(event["result"]["case_index"], 1);
        assert!(!event.to_string().contains("private-provider-canary"));
        for (field, invalid) in [
            ("retry_round", 0),
            ("retry_round", 3),
            ("completed", 4),
            ("completed", 2),
        ] {
            let mut bad = value.clone();
            bad[field] = json!(invalid);
            assert!(matches!(
                evaluation_stream_event(bad),
                EvaluationStreamEvent::Error { .. }
            ));
        }
    }

    #[test]
    fn rejects_invalid_retry_progress_bounds_and_unknown_fields() {
        for value in [
            json!({"event":"retry_round_start","retry_round":0,"total":3,"pending":1}),
            json!({"event":"retry_round_start","retry_round":3,"total":3,"pending":1}),
            json!({"event":"retry_round_start","retry_round":1,"total":3,"pending":0}),
            json!({"event":"retry_round_start","retry_round":1,"total":3,"pending":4}),
            json!({"event":"retry_round_start","retry_round":1,"total":3,"pending":1,"raw":"private"}),
            json!({"event":"case_retry_start","retry_round":1,"total":3,"case_index":0}),
            json!({"event":"case_retry_start","retry_round":1,"total":3,"case_index":4}),
            json!({"event":"case_retry_start","retry_round":3,"total":3,"case_index":1}),
        ] {
            assert!(matches!(
                evaluation_stream_event(value),
                EvaluationStreamEvent::Error { .. }
            ));
        }
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
        let chunks: Vec<Value> = (1..=35)
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
        ranking["relevant_chunk_ranks"] = json!([1, 35]);
        let mut utilization = base.clone();
        utilization["chunks_used"] = json!([35, "chunk-2"]);
        let mut similarity = base.clone();
        similarity["contradicting_claims"] = json!([]);
        let mut completeness = base.clone();
        completeness["missing_elements"] = json!([]);
        let mut hallucination = base.clone();
        hallucination["unsupported_claims"] = json!([]);
        let payload = json!({
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
        });
        let event = evaluation_stream_event(payload.clone());

        let serialized = serde_json::to_value(event).unwrap();
        assert_eq!(serialized["event"], "caseSuccess");
        let mut recovered = payload.clone();
        recovered["event"] = json!("case_retry_result");
        recovered["retry_round"] = json!(1);
        recovered["result"]["attempt_history"] =
            json!([diagnostic("payment_required", Some(402), 0)]);
        let safe = serde_json::to_value(evaluation_stream_event(recovered)).unwrap();
        assert_eq!(safe["event"], "caseRetryResult");
        assert_eq!(safe["result"]["error"], Value::Null);
        assert!(!safe.to_string().contains("synthetic-untrusted"));
        assert_eq!(
            serialized["result"]["retrieved_chunks"][34]["text"],
            "full chunk 35"
        );
        assert_eq!(
            serialized["result"]["judge_scores"]["context_utilization"]["chunks_used"][0],
            35
        );
        assert_eq!(
            serialized["result"]["judge_scores"]["context_utilization"]["chunks_used"][1],
            "chunk-2"
        );

        for invalid_chunks in [
            json!([]),
            json!((1..=36)
                .map(|rank| {
                    json!({
                        "rank": rank, "text": "chunk", "score": 1.0,
                        "paper_id": format!("paper-{rank}"), "chunk_index": rank - 1,
                        "filename": "paper.pdf", "title": "Paper", "page": rank
                    })
                })
                .collect::<Vec<_>>()),
        ] {
            let mut invalid = payload.clone();
            invalid["result"]["retrieved_chunks"] = invalid_chunks;
            assert!(matches!(
                evaluation_stream_event(invalid),
                EvaluationStreamEvent::Error { .. }
            ));
        }

        let mut invalid_reference = payload;
        invalid_reference["result"]["judge_scores"]["relevant_chunk_ranking"]
            ["relevant_chunk_ranks"] = json!([36]);
        assert!(matches!(
            evaluation_stream_event(invalid_reference),
            EvaluationStreamEvent::Error { .. }
        ));
    }
}
