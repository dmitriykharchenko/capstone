use std::collections::HashSet;

use serde::Deserialize;

use super::{
    MAX_FOLLOWUP_COUNT, MAX_SUBQUERY_COUNT, MIN_SUBQUERY_COUNT, RagError, RagErrorKind, RagStage,
    normalization::{casefold, compact},
};

// Deserialize directly into structs, not Value: serde rejects duplicate keys,
// unknown keys, invalid types, nonfinite numbers and trailing JSON here.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    subqueries: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Coverage {
    complete: bool,
    follow_up_subqueries: Vec<String>,
}

pub(super) fn plan(text: &str) -> Result<Vec<String>, RagError> {
    let error = RagError::new(RagStage::Planner, RagErrorKind::InvalidPlan);
    require_object(text, error)?;
    let plan: Plan = serde_json::from_str(text).map_err(|_| error)?;
    if !(MIN_SUBQUERY_COUNT..=MAX_SUBQUERY_COUNT).contains(&plan.subqueries.len()) {
        return Err(error);
    }
    questions(plan.subqueries, &[], error)
}

pub(super) fn coverage(text: &str, initial: &[String]) -> Result<Vec<String>, RagError> {
    let error = RagError::new(RagStage::Coverage, RagErrorKind::InvalidCoverage);
    require_object(text, error)?;
    let coverage: Coverage = serde_json::from_str(text).map_err(|_| error)?;
    if coverage.follow_up_subqueries.len() > MAX_FOLLOWUP_COUNT
        || coverage.complete != coverage.follow_up_subqueries.is_empty()
    {
        return Err(error);
    }
    questions(coverage.follow_up_subqueries, initial, error)
}

// Derived struct deserialization also accepts positional arrays. Explicitly
// require an object without first parsing through Value (which loses duplicates).
fn require_object(text: &str, error: RagError) -> Result<(), RagError> {
    if text
        .trim_start_matches([' ', '\n', '\r', '\t'])
        .starts_with('{')
    {
        Ok(())
    } else {
        Err(error)
    }
}

fn questions(raw: Vec<String>, prior: &[String], error: RagError) -> Result<Vec<String>, RagError> {
    let mut seen: HashSet<_> = prior.iter().map(|s| casefold(&compact(s))).collect();
    raw.into_iter()
        .map(|s| {
            let text = compact(&s);
            if text.is_empty() || !seen.insert(casefold(&text)) {
                Err(error)
            } else {
                Ok(text)
            }
        })
        .collect()
}
