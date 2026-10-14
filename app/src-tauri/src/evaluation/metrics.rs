//! Strict numeric-only wire schema from src/evaluation/metrics.py.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_SAFE_COUNT: u64 = (1 << 53) - 1;

fn safe_counts(values: &[u64]) -> bool {
    values.iter().all(|value| *value <= MAX_SAFE_COUNT)
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "f64")]
struct Nonnegative(f64);

impl TryFrom<f64> for Nonnegative {
    type Error = &'static str;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value.is_finite() && value >= 0.0 {
            Ok(Self(value))
        } else {
            Err("metrics must be finite non-negative numbers")
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Latency {
    total: Nonnegative,
    rag: Nonnegative,
    judge: Nonnegative,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelCalls {
    total: u64,
    rag: u64,
    judge: u64,
    embedding: u64,
}

impl ModelCalls {
    fn validate(&self) -> bool {
        self.total <= MAX_SAFE_COUNT
            && self
                .rag
                .checked_add(self.judge)
                .and_then(|n| n.checked_add(self.embedding))
                == Some(self.total)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Tokens {
    #[serde(deserialize_with = "Option::deserialize")]
    input: Option<u64>,
    #[serde(default, deserialize_with = "Option::deserialize")]
    cached_input: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    output: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    total: Option<u64>,
    known_input: u64,
    known_output: u64,
    known_total: u64,
    reported_calls: u64,
}

impl Tokens {
    fn validate(&self, calls: u64) -> bool {
        self.reported_calls <= calls
            && safe_counts(&[
                self.known_input,
                self.known_output,
                self.known_total,
                self.reported_calls,
            ])
            && self.cached_input.map_or(true, |cached| {
                cached <= MAX_SAFE_COUNT && self.input.map_or(true, |input| cached <= input)
            })
            && [
                (self.input, self.known_input),
                (self.output, self.known_output),
                (self.total, self.known_total),
            ]
            .iter()
            .all(|(total, known)| total.map_or(self.reported_calls < calls, |n| n == *known))
            && ((self.reported_calls == calls)
                == (self.input.is_some() && self.output.is_some() && self.total.is_some()))
            && (calls != 0
                || self.known_input == 0 && self.known_output == 0 && self.known_total == 0)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cost {
    #[serde(deserialize_with = "Option::deserialize")]
    estimated_usd: Option<Nonnegative>,
    known_usd: Nonnegative,
    priced_calls: u64,
    provider_reported_calls: u64,
    rate_estimated_calls: u64,
}

impl Cost {
    fn validate(&self, calls: u64) -> bool {
        self.priced_calls <= calls
            && safe_counts(&[
                self.priced_calls,
                self.provider_reported_calls,
                self.rate_estimated_calls,
            ])
            && self
                .provider_reported_calls
                .checked_add(self.rate_estimated_calls)
                == Some(self.priced_calls)
            && (self.estimated_usd.is_some() == (self.priced_calls == calls))
            && self
                .estimated_usd
                .map_or(true, |total| total == self.known_usd)
            && (self.priced_calls != 0 || self.known_usd.0 == 0.0)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Context {
    chunks: u64,
    unique_chunks: u64,
    duplicate_chunks: u64,
    characters: u64,
    utf8_bytes: u64,
    max_chunks: u64,
    max_characters: u64,
    prompt_characters: u64,
    max_prompt_characters: u64,
}

impl Context {
    fn validate(&self) -> bool {
        safe_counts(&[
            self.chunks,
            self.unique_chunks,
            self.duplicate_chunks,
            self.characters,
            self.utf8_bytes,
            self.max_chunks,
            self.max_characters,
            self.prompt_characters,
            self.max_prompt_characters,
        ]) && self.unique_chunks <= self.chunks
            && self.duplicate_chunks <= self.chunks
            && self.max_chunks <= self.chunks
            && self.max_characters <= self.characters
            && self.characters <= self.utf8_bytes
            && self.max_prompt_characters <= self.prompt_characters
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Workflow {
    exchanges: u64,
    tool_calls: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reused_searches: Option<u64>,
}

impl Workflow {
    fn validate(&self) -> bool {
        safe_counts(&[self.exchanges, self.tool_calls])
            && self
                .reused_searches
                .map_or(true, |count| count <= MAX_SAFE_COUNT)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Planner,
    Retrieval,
    Subanswer,
    Coverage,
    FollowupRetrieval,
    FollowupSubanswer,
    Composer,
    Judge,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StageMetrics {
    latency_ms: Nonnegative,
    model_calls: u64,
    exchanges: u64,
    tool_calls: u64,
    tokens: Tokens,
    cost: Cost,
    context: Context,
}

impl StageMetrics {
    fn validate(&self) -> bool {
        safe_counts(&[self.model_calls, self.exchanges, self.tool_calls])
            && self.exchanges <= self.model_calls
            && self.tokens.validate(self.model_calls)
            && self.cost.validate(self.model_calls)
            && self.context.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pricing {
    #[serde(deserialize_with = "Option::deserialize")]
    input_usd_per_million: Option<Nonnegative>,
    #[serde(deserialize_with = "Option::deserialize")]
    output_usd_per_million: Option<Nonnegative>,
    #[serde(deserialize_with = "Option::deserialize")]
    embedding_usd_per_million: Option<Nonnegative>,
    #[serde(default, deserialize_with = "Option::deserialize")]
    cached_input_usd_per_million: Option<Nonnegative>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvaluationMetrics {
    version: u8,
    latency_ms: Latency,
    model_calls: ModelCalls,
    tokens: Tokens,
    cost: Cost,
    workflow: Workflow,
    context: Context,
    stages: BTreeMap<Stage, StageMetrics>,
    pricing: Pricing,
}

impl EvaluationMetrics {
    pub(super) fn validate(&self) -> bool {
        self.version == 1
            && self.model_calls.validate()
            && self.tokens.validate(self.model_calls.total)
            && self.cost.validate(self.model_calls.total)
            && self.workflow.validate()
            && self.workflow.exchanges <= self.model_calls.rag
            && self.context.validate()
            && self.stages.values().all(StageMetrics::validate)
            && self
                .stages
                .values()
                .try_fold(0u64, |sum, stage| sum.checked_add(stage.model_calls))
                == Some(self.model_calls.total)
    }
}

// Python emits {} for these groups when no final cases have metrics.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
enum AggregateGroup<T> {
    Measured(T),
    Empty(Empty),
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AggregateLatency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total: Option<Nonnegative>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rag: Option<Nonnegative>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    judge: Option<Nonnegative>,
    #[serde(deserialize_with = "Option::deserialize")]
    mean: Option<Nonnegative>,
    #[serde(deserialize_with = "Option::deserialize")]
    p50: Option<Nonnegative>,
    #[serde(deserialize_with = "Option::deserialize")]
    p95: Option<Nonnegative>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PerformanceBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_rag_latency_ms: Option<Nonnegative>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_rag_model_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_retrievals: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_followup_queries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_rag_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_cost_usd: Option<Nonnegative>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_duplicate_chunks: Option<u64>,
}

impl PerformanceBudget {
    pub(super) fn validate(&self) -> bool {
        let counts = [
            self.max_rag_model_calls,
            self.max_retrievals,
            self.max_followup_queries,
            self.max_rag_tokens,
            self.max_duplicate_chunks,
        ];
        (self.max_rag_latency_ms.is_some()
            || self.max_cost_usd.is_some()
            || counts.iter().any(Option::is_some))
            && safe_counts(&counts.iter().flatten().copied().collect::<Vec<_>>())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PerformanceCheckName {
    RagLatencyMs,
    RagModelCalls,
    Retrievals,
    FollowupQueries,
    RagTokens,
    RagCostUsd,
    DuplicateChunks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Pass,
    Fail,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum PerformanceStatus {
    Pass,
    Fail,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PerformanceCheck {
    #[serde(deserialize_with = "Option::deserialize")]
    value: Option<Nonnegative>,
    limit: Nonnegative,
    status: CheckStatus,
}

/// Mirrors check_performance in src/evaluation/metrics.py.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PerformanceChecks {
    version: u8,
    status: PerformanceStatus,
    checks: BTreeMap<PerformanceCheckName, PerformanceCheck>,
}

impl PerformanceChecks {
    pub(super) fn validate(&self) -> bool {
        let consistent = self.checks.values().all(|check| match check.value {
            None => check.status == CheckStatus::Unknown,
            Some(value) => {
                check.status
                    == if value.0 <= check.limit.0 {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    }
            }
        });
        let has = |status| self.checks.values().any(|check| check.status == status);
        self.version == 1
            && consistent
            && match self.status {
                PerformanceStatus::Fail => has(CheckStatus::Fail),
                PerformanceStatus::Incomplete => !has(CheckStatus::Fail),
                PerformanceStatus::Pass => !has(CheckStatus::Fail) && !has(CheckStatus::Unknown),
            }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AggregatePerformance {
    cases_checked: u32,
    pass: u32,
    fail: u32,
    incomplete: u32,
    failed_checks: BTreeMap<PerformanceCheckName, u32>,
}

impl AggregatePerformance {
    fn validate(&self, total: u32) -> bool {
        self.cases_checked <= total
            && self
                .pass
                .checked_add(self.fail)
                .and_then(|n| n.checked_add(self.incomplete))
                == Some(self.cases_checked)
            && self
                .failed_checks
                .values()
                .all(|count| (1..=self.fail).contains(count))
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AggregateMetrics {
    cases_measured: u32,
    cases_total: u32,
    latency_ms: AggregateLatency,
    model_calls: AggregateGroup<ModelCalls>,
    tokens: AggregateGroup<Tokens>,
    cost: AggregateGroup<Cost>,
    workflow: AggregateGroup<Workflow>,
    context: AggregateGroup<Context>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    performance: Option<AggregatePerformance>,
}

impl AggregateMetrics {
    pub(super) fn validate(&self, total: u32) -> bool {
        if self.cases_total != total
            || self.cases_measured > total
            || self
                .performance
                .as_ref()
                .is_some_and(|performance| !performance.validate(total))
        {
            return false;
        }
        let latency = &self.latency_ms;
        let times = [
            latency.total,
            latency.rag,
            latency.judge,
            latency.mean,
            latency.p50,
            latency.p95,
        ];
        match (
            &self.model_calls,
            &self.tokens,
            &self.cost,
            &self.workflow,
            &self.context,
        ) {
            (
                AggregateGroup::Empty(_),
                AggregateGroup::Empty(_),
                AggregateGroup::Empty(_),
                AggregateGroup::Empty(_),
                AggregateGroup::Empty(_),
            ) => self.cases_measured == 0 && times.iter().all(Option::is_none),
            (
                AggregateGroup::Measured(calls),
                AggregateGroup::Measured(tokens),
                AggregateGroup::Measured(cost),
                AggregateGroup::Measured(workflow),
                AggregateGroup::Measured(context),
            ) => {
                self.cases_measured > 0
                    && times.iter().all(Option::is_some)
                    && calls.validate()
                    && tokens.validate(calls.total)
                    && cost.validate(calls.total)
                    && workflow.validate()
                    && workflow.exchanges <= calls.rag
                    && context.validate()
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests;
