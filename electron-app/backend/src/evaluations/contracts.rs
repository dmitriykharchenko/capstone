use super::{
    JUDGE_CRITERIA, JudgeScores,
    strict_json::{self, nonempty},
};
use crate::{
    domain::{Chunk, EvaluationCase, RetrievalStrategy},
    protocol::{ErrorCode, MAX_FRAME_BYTES, Output, OutputBody},
    registry::BackendResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_CASES: usize = 1000;
pub const MAX_ARTIFACT_BYTES: usize = 64 * MAX_FRAME_BYTES;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRequest {
    pub retrieval_type: RetrievalStrategy,
    pub test_cases: Vec<EvaluationCase>,
}
impl EvaluationRequest {
    /// Later handlers should use this instead of deserializing foundation DTOs.
    pub fn from_value(value: Value) -> BackendResult<Self> {
        let request: Self = strict_json::exact(value)?;
        request.validate()?;
        Ok(request)
    }
    pub fn parse(text: &str) -> BackendResult<Self> {
        Self::from_value(strict_json::value(text, MAX_FRAME_BYTES)?)
    }
    pub fn validate(&self) -> BackendResult<()> {
        if self.retrieval_type != RetrievalStrategy::Hybrid {
            return Err(ErrorCode::InvalidParams);
        }
        validate_test_cases(&self.test_cases)
    }
}
pub fn parse_test_cases(text: &str) -> BackendResult<Vec<EvaluationCase>> {
    let cases: Vec<EvaluationCase> =
        strict_json::exact(strict_json::value(text, MAX_FRAME_BYTES)?)?;
    validate_test_cases(&cases)?;
    Ok(cases)
}
pub fn load_default_cases() -> BackendResult<Vec<EvaluationCase>> {
    parse_test_cases(include_str!(
        "../../resources/evaluations/eval_questions.json"
    ))
}
pub fn validate_test_cases(cases: &[EvaluationCase]) -> BackendResult<()> {
    if cases.is_empty()
        || cases.len() > MAX_CASES
        || cases.iter().any(|c| {
            !nonempty(&c.question)
                || !nonempty(&c.grading_notes)
                || c.sources.len() > 1000
                || c.sources.iter().any(|s| !nonempty(s))
        })
    {
        return Err(ErrorCode::InvalidParams);
    }
    if serde_json::to_vec(cases)
        .map_err(|_| ErrorCode::Internal)?
        .len()
        > MAX_FRAME_BYTES
    {
        return Err(ErrorCode::ResponseTooLarge);
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievedChunk {
    pub rank: u32,
    pub text: String,
    pub score: f64,
    pub paper_id: String,
    pub chunk_index: u32,
    pub filename: String,
    pub title: String,
    pub page: u32,
}
impl From<Chunk> for RetrievedChunk {
    fn from(c: Chunk) -> Self {
        Self {
            rank: c.source.rank,
            text: c.text,
            score: c.source.score,
            paper_id: c.source.paper_id,
            chunk_index: c.source.chunk_index,
            filename: c.source.filename,
            title: c.source.title,
            page: c.source.page,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseError {
    pub stage: String,
    pub message: String,
}
impl CaseError {
    pub(super) fn new(stage: &str, truncated: bool) -> Self {
        let message = if truncated {
            "Model output was incomplete for this case. Check the output limit and retry."
        } else {
            match stage {
                "planner" => {
                    "Question planning failed for this case. Check backend configuration and retry."
                }
                "retrieval" => "Retrieval failed for this case. Check the paper index and retry.",
                "subanswer" => {
                    "Subanswer generation failed for this case. Check backend configuration and retry."
                }
                "coverage" => {
                    "Coverage review failed for this case. Check backend configuration and retry."
                }
                "followup_retrieval" => {
                    "Follow-up retrieval failed for this case. Check the paper index and retry."
                }
                "followup_subanswer" => {
                    "Follow-up subanswer generation failed for this case. Check backend configuration and retry."
                }
                "composer" => {
                    "Final answer composition failed for this case. Check backend configuration and retry."
                }
                "judge" => {
                    "Judge evaluation failed for this case. Check backend configuration and retry."
                }
                _ => "Evaluation failed for this case. Check backend configuration and retry.",
            }
        };
        Self {
            stage: stage.into(),
            message: message.into(),
        }
    }
    fn valid(&self) -> bool {
        [
            "planner",
            "retrieval",
            "subanswer",
            "coverage",
            "followup_retrieval",
            "followup_subanswer",
            "composer",
            "judge",
            "evaluation",
        ]
        .contains(&self.stage.as_str())
            && (self.message == Self::new(&self.stage, false).message
                || self.message == Self::new(&self.stage, true).message)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseResult {
    pub case_index: usize,
    pub input: EvaluationCase,
    pub retrieval_strategy: RetrievalStrategy,
    pub retrieved_chunks: Vec<RetrievedChunk>,
    pub generated_answer: Option<String>,
    pub judge_scores: Option<JudgeScores>,
    pub error: Option<CaseError>,
}
impl CaseResult {
    pub fn validate(&self, total: usize) -> BackendResult<()> {
        validate_test_cases(std::slice::from_ref(&self.input))?;
        if !(1..=total).contains(&self.case_index)
            || self.retrieval_strategy != RetrievalStrategy::Hybrid
            || self.retrieved_chunks.len() > 35
            || self.retrieved_chunks.iter().enumerate().any(|(i, c)| {
                c.rank as usize != i + 1
                    || !nonempty(&c.text)
                    || !c.score.is_finite()
                    || !nonempty(&c.paper_id)
                    || !nonempty(&c.filename)
                    || !nonempty(&c.title)
                    || c.page == 0
            })
            || self.generated_answer.as_ref().is_some_and(|s| !nonempty(s))
            || self.error.as_ref().is_some_and(|e| !e.valid())
        {
            return Err(ErrorCode::InvalidParams);
        }
        match (&self.error, &self.judge_scores, &self.generated_answer) {
            (None, Some(scores), Some(_)) if !self.retrieved_chunks.is_empty() => scores.validate(),
            (Some(error), None, answer)
                if (error.stage == "judge"
                    && answer.is_some()
                    && !self.retrieved_chunks.is_empty())
                    || (error.stage != "judge"
                        && answer.is_none()
                        && self.retrieved_chunks.is_empty()) =>
            {
                Ok(())
            }
            _ => Err(ErrorCode::InvalidParams),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct AggregateScores(BTreeMap<String, Option<f64>>);
impl AggregateScores {
    fn calculate(results: &[CaseResult]) -> Self {
        let mut totals = [0u64; 7];
        let mut count = 0;
        for r in results.iter().filter(|r| r.error.is_none()) {
            if let Some(scores) = &r.judge_scores {
                count += 1;
                for (sum, value) in totals.iter_mut().zip(scores.scores()) {
                    *sum += u64::from(value);
                }
            }
        }
        // Legacy divides by successful cases, without intermediate/display rounding.
        Self(
            JUDGE_CRITERIA
                .into_iter()
                .zip(totals)
                .map(|(name, sum)| {
                    (
                        name.into(),
                        (count > 0).then(|| sum as f64 / f64::from(count)),
                    )
                })
                .collect(),
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Complete {
    pub retrieval_type: RetrievalStrategy,
    pub completed: usize,
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub aggregate_scores: AggregateScores,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    retrieval_identifier: String,
    retrieval_strategy: RetrievalStrategy,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Summary {
    total: usize,
    succeeded: usize,
    failed: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationArtifact {
    schema: String,
    schema_version: u32,
    metadata: Metadata,
    pub input_cases: Vec<EvaluationCase>,
    pub results: Vec<CaseResult>,
    pub aggregate_scores: AggregateScores,
    summary: Summary,
}
impl EvaluationArtifact {
    pub fn new(input_cases: Vec<EvaluationCase>, results: Vec<CaseResult>) -> BackendResult<Self> {
        let total = input_cases.len();
        let succeeded = results.iter().filter(|r| r.error.is_none()).count();
        let artifact = Self {
            schema: "rag-evaluation-results".into(),
            schema_version: 1,
            metadata: Metadata {
                retrieval_identifier: "multihop-hybrid".into(),
                retrieval_strategy: RetrievalStrategy::Hybrid,
            },
            aggregate_scores: AggregateScores::calculate(&results),
            input_cases,
            results,
            summary: Summary {
                total,
                succeeded,
                failed: total.saturating_sub(succeeded),
            },
        };
        artifact.validate()?;
        Ok(artifact)
    }
    pub fn parse(text: &str) -> BackendResult<Self> {
        let artifact: Self = strict_json::exact(strict_json::value(text, MAX_ARTIFACT_BYTES)?)?;
        artifact.validate()?;
        Ok(artifact)
    }
    pub fn complete(&self) -> Complete {
        Complete {
            retrieval_type: RetrievalStrategy::Hybrid,
            completed: self.summary.total,
            total: self.summary.total,
            succeeded: self.summary.succeeded,
            failed: self.summary.failed,
            aggregate_scores: self.aggregate_scores.clone(),
        }
    }
    pub fn validate(&self) -> BackendResult<()> {
        validate_test_cases(&self.input_cases)?;
        if self.schema != "rag-evaluation-results"
            || self.schema_version != 1
            || self.metadata.retrieval_identifier != "multihop-hybrid"
            || self.metadata.retrieval_strategy != RetrievalStrategy::Hybrid
            || self.results.len() != self.input_cases.len()
            || self.summary.total != self.results.len()
            || self.summary.succeeded != self.results.iter().filter(|r| r.error.is_none()).count()
            || self.summary.failed != self.results.len() - self.summary.succeeded
            || self.aggregate_scores != AggregateScores::calculate(&self.results)
        {
            return Err(ErrorCode::InvalidParams);
        }
        for (i, (case, result)) in self.input_cases.iter().zip(&self.results).enumerate() {
            result.validate(self.results.len())?;
            if result.case_index != i + 1
                || case.question != result.input.question
                || case.grading_notes != result.input.grading_notes
                || case.sources != result.input.sources
            {
                return Err(ErrorCode::InvalidParams);
            }
        }
        Ok(())
    }
    pub fn serialize(&self) -> BackendResult<Vec<u8>> {
        self.validate()?;
        // Value uses sorted maps: deterministic recursively, UTF-8, no timestamps.
        let mut bytes = serde_json::to_vec_pretty(
            &serde_json::to_value(self).map_err(|_| ErrorCode::Internal)?,
        )
        .map_err(|_| ErrorCode::Internal)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(ErrorCode::ResponseTooLarge);
        }
        Ok(bytes)
    }
}

pub(super) fn bounded_result(complete: &Complete) -> BackendResult<Value> {
    let data = serde_json::to_value(complete).map_err(|_| ErrorCode::Internal)?;
    // Serialize the actual public terminal shape with the maximum valid id/seq.
    // Also respect runtime's retained-result bound before any irreversible write.
    let output = Output::request(
        &"x".repeat(64),
        u64::MAX,
        OutputBody::Result { data: data.clone() },
    );
    if serde_json::to_vec(&data)
        .map_err(|_| ErrorCode::Internal)?
        .len()
        > MAX_FRAME_BYTES - 256
        || serde_json::to_vec(&output)
            .map_err(|_| ErrorCode::Internal)?
            .len()
            > MAX_FRAME_BYTES
    {
        return Err(ErrorCode::ResponseTooLarge);
    }
    Ok(data)
}
