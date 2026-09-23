//! No string/data inputs: diagnostics cannot receive conversation text, IDs,
//! fingerprints, paths, credentials, vectors or provider bodies.
use crate::protocol::ErrorCode;
use serde::Serialize;
use std::{io::Write, time::Instant};

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
    Admission,
    History,
    Rag,
    CanonicalCommit,
    DerivedIndex,
    Reconcile,
}
#[derive(Serialize)]
struct Record {
    component: &'static str,
    stage: Stage,
    elapsed_ms: u128,
    outcome: Outcome,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Complete,
    Cancelled,
    Unknown,
    Failed,
}
fn record(stage: Stage, elapsed_ms: u128, error: Option<ErrorCode>) -> Record {
    let outcome = match error {
        None => Outcome::Complete,
        Some(ErrorCode::Cancelled) => Outcome::Cancelled,
        Some(ErrorCode::OutcomeUnknown) => Outcome::Unknown,
        _ => Outcome::Failed,
    };
    Record {
        component: "native_chat",
        stage,
        elapsed_ms,
        outcome,
    }
}
pub(super) struct Diagnostic {
    pub stage: Stage,
    start: Instant,
    pub error: Option<ErrorCode>,
}
impl Diagnostic {
    pub fn new() -> Self {
        Self {
            stage: Stage::Admission,
            start: Instant::now(),
            error: Some(ErrorCode::Internal),
        }
    }
}
impl Drop for Diagnostic {
    fn drop(&mut self) {
        if let Ok(mut line) = serde_json::to_vec(&record(
            self.stage,
            self.start.elapsed().as_millis(),
            self.error,
        )) {
            line.push(b'\n');
            let _ = std::io::stderr().lock().write_all(&line);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_have_only_fixed_labels_and_elapsed_time() {
        let v = serde_json::to_value(record(Stage::DerivedIndex, 12, Some(ErrorCode::Internal)))
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"component":"native_chat","stage":"derived_index","elapsed_ms":12,"outcome":"failed"})
        );
    }
}
