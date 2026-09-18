use super::{
    RetrievedChunk,
    strict_json::{self, nonempty},
};
use crate::{
    domain::{EvaluationCase, Message},
    protocol::{ErrorCode, MAX_FRAME_BYTES},
    registry::BackendResult,
};
use serde::{Deserialize, Serialize};

pub const JUDGE_CRITERIA: [&str; 7] = [
    "context_precision",
    "context_recall",
    "relevant_chunk_ranking",
    "context_utilization",
    "semantic_similarity",
    "completeness",
    "hallucination_rate",
];
pub const DEFAULT_JUDGE: &str = include_str!("../../resources/evaluations/judge.txt");
const CHUNKS: &str = "1. {{chunk_1}}\n2. {{chunk_2}}\n...";
const KEYS: [&str; 5] = [
    "{{query}}",
    "{{ground_truth_or_\"NOT PROVIDED\"}}",
    "{{generated_answer}}",
    "{{reference_answer}}",
    CHUNKS,
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

macro_rules! criterion {
    ($name:ident $(, $field:ident: $ty:ty)?) => {
        #[derive(Clone, Debug, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            pub score: u8,
            pub confidence: Confidence,
            /// Public evidence-based rubric explanation, NOT provider-private reasoning.
            pub reasoning: String,
            pub flags: Vec<String>,
            $(pub $field: $ty,)?
        }
    }
}
criterion!(Criterion);
criterion!(Ranking, relevant_chunk_ranks: Vec<u32>);
criterion!(Utilization, chunks_used: Vec<ChunkReference>);
criterion!(Similarity, contradicting_claims: Vec<String>);
criterion!(Completeness, missing_elements: Vec<String>);
criterion!(Hallucination, unsupported_claims: Vec<String>);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChunkReference {
    Rank(u32),
    Description(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeScores {
    pub context_precision: Criterion,
    pub context_recall: Criterion,
    pub relevant_chunk_ranking: Ranking,
    pub context_utilization: Utilization,
    pub semantic_similarity: Similarity,
    pub completeness: Completeness,
    pub hallucination_rate: Hallucination,
}

impl JudgeScores {
    pub fn scores(&self) -> [u8; 7] {
        [
            self.context_precision.score,
            self.context_recall.score,
            self.relevant_chunk_ranking.score,
            self.context_utilization.score,
            self.semantic_similarity.score,
            self.completeness.score,
            self.hallucination_rate.score,
        ]
    }
    pub fn validate(&self) -> BackendResult<()> {
        let fields = [
            (
                &self.context_precision.reasoning,
                &self.context_precision.flags,
            ),
            (&self.context_recall.reasoning, &self.context_recall.flags),
            (
                &self.relevant_chunk_ranking.reasoning,
                &self.relevant_chunk_ranking.flags,
            ),
            (
                &self.context_utilization.reasoning,
                &self.context_utilization.flags,
            ),
            (
                &self.semantic_similarity.reasoning,
                &self.semantic_similarity.flags,
            ),
            (&self.completeness.reasoning, &self.completeness.flags),
            (
                &self.hallucination_rate.reasoning,
                &self.hallucination_rate.flags,
            ),
        ];
        if self.scores().iter().any(|v| *v > 5)
            || fields
                .iter()
                .any(|(reason, flags)| !nonempty(reason) || !strings(flags))
            || self.relevant_chunk_ranking.relevant_chunk_ranks.len() > 35
            || self
                .relevant_chunk_ranking
                .relevant_chunk_ranks
                .iter()
                .any(|v| !(1..=35).contains(v))
            || self.context_utilization.chunks_used.len() > 35
            || self
                .context_utilization
                .chunks_used
                .iter()
                .any(|v| match v {
                    ChunkReference::Rank(v) => !(1..=35).contains(v),
                    ChunkReference::Description(v) => !nonempty(v),
                })
            || !strings(&self.semantic_similarity.contradicting_claims)
            || !strings(&self.completeness.missing_elements)
            || !strings(&self.hallucination_rate.unsupported_claims)
        {
            return Err(ErrorCode::InvalidParams);
        }
        Ok(())
    }
}
fn strings(values: &[String]) -> bool {
    values.len() <= 10_000 && values.iter().all(|s| nonempty(s))
}

pub fn parse_judge_output(text: &str) -> BackendResult<JudgeScores> {
    let scores: JudgeScores = strict_json::exact(strict_json::value(text, MAX_FRAME_BYTES)?)?;
    scores.validate()?;
    Ok(scores)
}

#[derive(Clone)]
pub struct JudgeTemplate {
    system: String,
    user: String,
}
impl JudgeTemplate {
    pub fn embedded() -> BackendResult<Self> {
        Self::parse(DEFAULT_JUDGE)
    }
    pub fn parse(text: &str) -> BackendResult<Self> {
        if text.len() > MAX_FRAME_BYTES {
            return Err(ErrorCode::ResponseTooLarge);
        }
        let (system, user) = text
            .strip_prefix("SYSTEM PROMPT:\n")
            .and_then(|t| t.split_once("\nUSER PROMPT (fill per example):\n"))
            .ok_or(ErrorCode::InvalidParams)?;
        if !nonempty(system) || KEYS.iter().any(|k| !user.contains(k)) {
            return Err(ErrorCode::InvalidParams);
        }
        Ok(Self {
            system: system.trim_matches('\n').into(),
            user: user.trim_matches('\n').into(),
        })
    }
    pub fn render(
        &self,
        case: &EvaluationCase,
        chunks: &[RetrievedChunk],
        answer: &str,
    ) -> BackendResult<Vec<Message>> {
        let ground = if case.sources.is_empty() {
            "NOT PROVIDED".into()
        } else {
            // Match Python ensure_ascii=False list spacing without altering source text.
            format!(
                "[{}]",
                case.sources
                    .iter()
                    .map(serde_json::to_string)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| ErrorCode::Internal)?
                    .join(", ")
            )
        };
        let ranked = chunks
            .iter()
            .map(|c| format!("{}. {}", c.rank, c.text))
            .collect::<Vec<_>>()
            .join("\n");
        let replacements = [
            &case.question,
            &ground,
            &answer.to_owned(),
            &case.grading_notes,
            &ranked,
        ];
        // Single template scan: inserted untrusted text is never reinterpreted as a placeholder.
        let mut rest = self.user.as_str();
        let mut user = String::new();
        while let Some((at, index)) = KEYS
            .iter()
            .enumerate()
            .filter_map(|(i, k)| rest.find(k).map(|at| (at, i)))
            .min()
        {
            user.push_str(&rest[..at]);
            user.push_str(replacements[index]);
            rest = &rest[at + KEYS[index].len()..];
        }
        user.push_str(rest);
        let messages = vec![
            message("system", self.system.clone()),
            message("user", user),
        ];
        if serde_json::to_vec(&messages)
            .map_err(|_| ErrorCode::Internal)?
            .len()
            > MAX_FRAME_BYTES
        {
            return Err(ErrorCode::ResponseTooLarge);
        }
        Ok(messages)
    }
}
fn message(role: &str, content: String) -> Message {
    Message {
        role: role.into(),
        content,
        exchange_id: None,
        sources: vec![],
    }
}
