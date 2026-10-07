use super::super::{evaluation_stream_event, EvaluationStreamEvent};
use super::*;
use serde_json::{json, Value};

fn snapshot() -> Value {
    let context = json!({"chunks":0,"unique_chunks":0,"duplicate_chunks":0,"characters":0,"utf8_bytes":0,
        "max_chunks":0,"max_characters":0,"prompt_characters":20,"max_prompt_characters":20});
    let tokens = json!({"input":10,"output":5,"total":15,"known_input":10,
        "known_output":5,"known_total":15,"reported_calls":1});
    let cost = json!({"estimated_usd":0.002,"known_usd":0.002,"priced_calls":1,
        "provider_reported_calls":1,"rate_estimated_calls":0});
    let stage = json!({"latency_ms":4.0,"model_calls":1,"exchanges":1,"tool_calls":0,
        "tokens":tokens,"cost":cost,"context":context});
    let mut embedding = stage.clone();
    embedding["exchanges"] = json!(0);
    embedding["tool_calls"] = json!(1);
    embedding["tokens"] = json!({"input":10,"output":0,"total":10,"known_input":10,
        "known_output":0,"known_total":10,"reported_calls":1});
    embedding["cost"] = json!({"estimated_usd":null,"known_usd":0.0,"priced_calls":0,
        "provider_reported_calls":0,"rate_estimated_calls":0});
    let mut judge = stage.clone();
    judge["tokens"] = json!({"input":null,"output":null,"total":null,"known_input":0,
        "known_output":0,"known_total":0,"reported_calls":0});
    judge["cost"] = embedding["cost"].clone();
    json!({"version":1,"latency_ms":{"total":12.0,"rag":8.0,"judge":4.0},
        "model_calls":{"total":3,"rag":1,"judge":1,"embedding":1},
        "tokens":{"input":null,"output":null,"total":null,"known_input":20,
            "known_output":5,"known_total":25,"reported_calls":2},
        "cost":{"estimated_usd":null,"known_usd":0.002,"priced_calls":1,
            "provider_reported_calls":1,"rate_estimated_calls":0},
        "workflow":{"exchanges":1,"tool_calls":1,"reused_searches":0},"context":context,
        "stages":{"planner":stage,"retrieval":embedding,"judge":judge},
        "pricing":{"input_usd_per_million":1.0,"output_usd_per_million":2.0,
            "embedding_usd_per_million":null}})
}

fn case_event(success: bool, retry: bool) -> Value {
    let mut event = json!({"event":if success {"case_success"} else {"case_failure"},
        "completed":1,"total":1,"result":{"case_index":1,
            "input":{"question":"q","grading_notes":"g","sources":[]},
            "retrieval_strategy":"hybrid","retrieved_chunks":[],"generated_answer":null,
            "judge_scores":null,"error":{"stage":"judge","message":"untrusted error"}}});
    if success {
        event["result"]["error"] = Value::Null;
        event["result"]["generated_answer"] = json!("answer");
        event["result"]["retrieved_chunks"] = json!([{"rank":1,"text":"chunk","score":1.0,
            "paper_id":"paper","chunk_index":0,"filename":"paper.pdf","title":"Paper","page":1}]);
        let mut scores = json!({});
        for (criterion, extra) in [
            ("context_precision", None),
            ("context_recall", None),
            ("relevant_chunk_ranking", Some("relevant_chunk_ranks")),
            ("context_utilization", Some("chunks_used")),
            ("semantic_similarity", Some("contradicting_claims")),
            ("completeness", Some("missing_elements")),
            ("hallucination_rate", Some("unsupported_claims")),
        ] {
            scores[criterion] =
                json!({"score":4,"confidence":"high","reasoning":"supported","flags":[]});
            if let Some(key) = extra {
                scores[criterion][key] = json!([]);
            }
        }
        event["result"]["judge_scores"] = scores;
    }
    if retry {
        event["event"] = json!("case_retry_result");
        event["retry_round"] = json!(1);
        if success {
            event["result"]["attempt_history"] = json!([{"stage":"judge","message":"untrusted error",
                "category":"timeout","retryable":true,"retry_round":0,"stage_attempt":1}]);
        }
    }
    event
}

fn complete_event() -> Value {
    json!({"event":"complete","retrieval_type":"hybrid","completed":2,"total":2,
        "succeeded":0,"failed":2,"aggregate_scores":{"context_precision":null,"context_recall":null,
            "relevant_chunk_ranking":null,"context_utilization":null,"semantic_similarity":null,
            "completeness":null,"hallucination_rate":null}})
}

fn aggregate(measured: bool) -> Value {
    let metrics = snapshot();
    let mut result = json!({"cases_measured":if measured {1} else {0},"cases_total":2,
        "latency_ms":{"mean":null,"p50":null,"p95":null}});
    for key in ["model_calls", "tokens", "cost", "workflow", "context"] {
        result[key] = if measured {
            metrics[key].clone()
        } else {
            json!({})
        };
    }
    if measured {
        result["latency_ms"] =
            json!({"total":12.0,"rag":8.0,"judge":4.0,"mean":12.0,"p50":12.0,"p95":12.0});
    }
    result
}

fn assert_invalid(event: Value) {
    let safe = evaluation_stream_event(event);
    assert_eq!(
        safe,
        EvaluationStreamEvent::Error {
            message: "invalid evaluation progress event".into()
        }
    );
}

#[test]
fn metric_snapshots_survive_success_failure_and_both_retry_outcomes() {
    for (success, retry, name) in [
        (true, false, "caseSuccess"),
        (false, false, "caseFailure"),
        (true, true, "caseRetryResult"),
        (false, true, "caseRetryResult"),
    ] {
        let mut event = case_event(success, retry);
        event["result"]["metrics"] = snapshot();
        let safe = serde_json::to_value(evaluation_stream_event(event)).unwrap();
        assert_eq!(safe["event"], name);
        assert_eq!(safe["result"]["metrics"], snapshot());
        assert!(!safe.to_string().contains("untrusted error"));
    }
}

#[test]
fn legacy_and_null_metrics_remain_optional() {
    for (success, retry) in [(true, false), (false, false), (true, true), (false, true)] {
        let event = case_event(success, retry);
        let safe = serde_json::to_value(evaluation_stream_event(event.clone())).unwrap();
        assert_ne!(safe["event"], "error");
        assert!(safe["result"].get("metrics").is_none());
        let mut event = event;
        event["result"]["metrics"] = Value::Null;
        assert_ne!(
            serde_json::to_value(evaluation_stream_event(event)).unwrap()["event"],
            "error"
        );
    }
    let mut event = complete_event();
    for null in [false, true] {
        if null {
            event["aggregate_metrics"] = Value::Null;
        }
        let safe = serde_json::to_value(evaluation_stream_event(event.clone())).unwrap();
        assert_eq!(safe["event"], "complete");
        assert!(safe.get("aggregateMetrics").is_none());
    }
}

#[test]
fn performance_checks_budgets_and_aggregates_are_strictly_consistent() {
    let checks = json!({"version":1,"status":"fail","checks":{
        "rag_latency_ms":{"value":8.0,"limit":30000.0,"status":"pass"},
        "followup_queries":{"value":1.0,"limit":0.0,"status":"fail"},
        "rag_tokens":{"value":null,"limit":100.0,"status":"unknown"}}});
    let mut event = case_event(true, false);
    event["result"]["input"]["performance_budget"] =
        json!({"max_followup_queries":0,"max_rag_latency_ms":30000.0});
    event["result"]["performance"] = checks.clone();
    let safe = serde_json::to_value(evaluation_stream_event(event.clone())).unwrap();
    assert_eq!(safe["result"]["performance"], checks);
    for (path, value) in [
        ("/result/performance/status", json!("pass")),
        ("/result/performance/status", json!("incomplete")),
        (
            "/result/performance/checks/followup_queries/status",
            json!("pass"),
        ),
        (
            "/result/performance/checks/rag_tokens/status",
            json!("pass"),
        ),
        ("/result/performance/checks/private", json!({})),
        ("/result/performance/version", json!(2)),
        ("/result/input/performance_budget", json!({})),
        ("/result/input/performance_budget/max_retrievals", json!(-1)),
        ("/result/input/performance_budget/private", json!(1)),
    ] {
        let mut bad = event.clone();
        let (parent, key) = path.rsplit_once('/').unwrap();
        bad.pointer_mut(parent).unwrap()[key] = value;
        assert_invalid(bad);
    }
    let mut complete = complete_event();
    complete["aggregate_metrics"] = aggregate(true);
    complete["aggregate_metrics"]["performance"] = json!({"cases_checked":2,"pass":1,
        "fail":1,"incomplete":0,"failed_checks":{"followup_queries":1}});
    let safe = serde_json::to_value(evaluation_stream_event(complete.clone())).unwrap();
    assert_eq!(safe["aggregateMetrics"]["performance"]["fail"], 1);
    complete["aggregate_metrics"]["performance"]["failed_checks"]["retrievals"] = json!(2);
    assert_invalid(complete);
}

#[test]
fn complete_carries_partial_coverage_and_empty_aggregates_in_camel_case() {
    for measured in [true, false] {
        let mut event = complete_event();
        event["aggregate_metrics"] = aggregate(measured);
        let safe = serde_json::to_value(evaluation_stream_event(event)).unwrap();
        assert_eq!(safe["event"], "complete");
        assert_eq!(safe["aggregateMetrics"], aggregate(measured));
        assert!(safe.get("aggregate_metrics").is_none());
    }
}

#[test]
fn accepts_fully_reported_usage_and_provider_or_rate_costs() {
    let mut metrics = snapshot();
    metrics["stages"]["judge"]["tokens"] = metrics["stages"]["planner"]["tokens"].clone();
    for stage in ["retrieval", "judge"] {
        metrics["stages"][stage]["cost"] = json!({"estimated_usd":0.002,"known_usd":0.002,
            "priced_calls":1,"provider_reported_calls":0,"rate_estimated_calls":1});
    }
    metrics["tokens"] = json!({"input":30,"output":10,"total":40,"known_input":30,
        "known_output":10,"known_total":40,"reported_calls":3});
    metrics["cost"] = json!({"estimated_usd":0.006,"known_usd":0.006,"priced_calls":3,
        "provider_reported_calls":1,"rate_estimated_calls":2});
    let mut event = case_event(true, false);
    event["result"]["metrics"] = metrics.clone();
    let safe = serde_json::to_value(evaluation_stream_event(event)).unwrap();
    assert_eq!(safe["result"]["metrics"], metrics);
}

#[test]
fn zero_call_cases_and_all_executed_stage_names_are_valid() {
    let mut metrics = snapshot();
    let (mut numbers, mut objects) = (vec![], vec![]);
    paths(&metrics, "", &mut numbers, &mut objects);
    for path in numbers {
        *metrics.pointer_mut(&path).unwrap() = json!(0);
    }
    metrics["version"] = json!(1);
    metrics["tokens"] = metrics["stages"]["planner"]["tokens"].clone();
    metrics["cost"] = metrics["stages"]["planner"]["cost"].clone();
    let zero_stage = metrics["stages"]["planner"].clone();
    metrics["stages"] = json!({});
    let mut event = case_event(false, false);
    event["result"]["metrics"] = metrics.clone();
    assert!(matches!(
        evaluation_stream_event(event.clone()),
        EvaluationStreamEvent::CaseFailure { .. }
    ));
    for name in [
        "planner",
        "retrieval",
        "subanswer",
        "coverage",
        "followup_retrieval",
        "followup_subanswer",
        "composer",
        "judge",
    ] {
        event["result"]["metrics"]["stages"][name] = zero_stage.clone();
        assert!(matches!(
            evaluation_stream_event(event.clone()),
            EvaluationStreamEvent::CaseFailure { .. }
        ));
    }
    let mut complete = complete_event();
    complete["aggregate_metrics"] = aggregate(true);
    for group in ["model_calls", "tokens", "cost", "workflow", "context"] {
        complete["aggregate_metrics"][group] = metrics[group].clone();
    }
    complete["aggregate_metrics"]["latency_ms"] =
        json!({"total":0,"rag":0,"judge":0,"mean":0,"p50":0,"p95":0});
    assert!(matches!(
        evaluation_stream_event(complete),
        EvaluationStreamEvent::Complete { .. }
    ));
}

fn paths(value: &Value, prefix: &str, numbers: &mut Vec<String>, objects: &mut Vec<String>) {
    if let Some(map) = value.as_object() {
        objects.push(prefix.into());
        for (key, value) in map {
            paths(value, &format!("{prefix}/{key}"), numbers, objects);
        }
    } else if value.is_number() {
        numbers.push(prefix.into());
    }
}

#[test]
fn rejects_text_negative_boolean_and_unknown_metadata_at_every_metrics_level() {
    let mut event = case_event(false, false);
    event["result"]["metrics"] = snapshot();
    let mut complete = complete_event();
    complete["aggregate_metrics"] = aggregate(true);
    for (event, root) in [(event, "/result/metrics"), (complete, "/aggregate_metrics")] {
        let (mut numbers, mut objects) = (vec![], vec![]);
        paths(
            event.pointer(root).unwrap(),
            root,
            &mut numbers,
            &mut objects,
        );
        for path in numbers {
            for invalid in [json!("private-canary"), json!(-1), json!(true), json!({})] {
                let mut bad = event.clone();
                *bad.pointer_mut(&path).unwrap() = invalid;
                assert_invalid(bad);
            }
        }
        for path in objects {
            let mut bad = event.clone();
            bad.pointer_mut(&path).unwrap()["private-canary"] = json!("not forwarded");
            assert_invalid(bad);
        }
    }
}

#[test]
fn rejects_bad_versions_stages_counts_coverage_and_missing_required_fields() {
    for (path, invalid) in [
        ("/version", json!(2)),
        ("/version", json!(1.5)),
        ("/model_calls/total", json!(4)),
        ("/model_calls/rag", json!(u64::MAX)),
        ("/tokens/reported_calls", json!(4)),
        ("/tokens/input", json!(12)),
        ("/tokens/known_total", json!(0.5)),
        ("/cost/priced_calls", json!(4)),
        ("/cost/estimated_usd", json!(0.002)),
        ("/cost/provider_reported_calls", json!(2)),
        ("/workflow/exchanges", json!(2)),
        ("/context/unique_chunks", json!(1)),
        ("/context/max_prompt_characters", json!(21)),
        ("/stages/planner/model_calls", json!(2)),
        ("/stages/judge/exchanges", json!(2)),
        ("/stages/judge/tokens/reported_calls", json!(1)),
        ("/stages/judge/cost/known_usd", json!(1)),
        ("/latency_ms/total", Value::Null),
        ("/pricing/input_usd_per_million", json!("1.0")),
    ] {
        let mut event = case_event(false, false);
        event["result"]["metrics"] = snapshot();
        *event["result"]["metrics"].pointer_mut(path).unwrap() = invalid;
        assert_invalid(event);
    }
    for name in ["done", "model", "evaluation", "embedding", "private-canary"] {
        let mut event = case_event(false, false);
        event["result"]["metrics"] = snapshot();
        event["result"]["metrics"]["stages"][name] = snapshot()["stages"]["planner"].clone();
        assert_invalid(event);
    }
    for group in ["tokens", "cost", "pricing"] {
        let metrics = snapshot();
        for key in metrics[group].as_object().unwrap().keys() {
            let mut event = case_event(false, false);
            event["result"]["metrics"] = metrics.clone();
            event["result"]["metrics"][group]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert_invalid(event);
        }
    }
}

#[test]
fn rejects_inconsistent_aggregate_coverage_and_empty_shapes() {
    for measured in [false, true] {
        let mut event = complete_event();
        event["aggregate_metrics"] = aggregate(measured);
        for (path, value) in [
            ("/cases_total", json!(1)),
            ("/cases_measured", json!(3)),
            ("/cases_measured", json!(if measured { 0 } else { 1 })),
            (
                "/model_calls",
                if measured {
                    json!({})
                } else {
                    snapshot()["model_calls"].clone()
                },
            ),
            (
                "/latency_ms/mean",
                if measured { Value::Null } else { json!(0.0) },
            ),
        ] {
            let mut bad = event.clone();
            *bad["aggregate_metrics"].pointer_mut(path).unwrap() = value;
            assert_invalid(bad);
        }
    }
    let mut event = complete_event();
    event["succeeded"] = json!(u32::MAX);
    event["failed"] = json!(3);
    assert_invalid(event);
}

#[test]
fn nonfinite_numbers_are_rejected_before_serialization() {
    use serde::de::value::{Error, F64Deserializer};
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(Nonnegative::deserialize(F64Deserializer::<Error>::new(value)).is_err());
    }
    for raw in ["1e999", "NaN", "Infinity"] {
        assert!(serde_json::from_str::<Nonnegative>(raw).is_err());
    }
}

#[test]
fn counters_cannot_exceed_javascript_safe_integer_precision() {
    let mut event = case_event(false, false);
    event["result"]["metrics"] = snapshot();
    let mut complete = complete_event();
    complete["aggregate_metrics"] = aggregate(true);
    for (event, root) in [(event, "/result/metrics"), (complete, "/aggregate_metrics")] {
        let (mut numbers, mut objects) = (vec![], vec![]);
        paths(
            event.pointer(root).unwrap(),
            root,
            &mut numbers,
            &mut objects,
        );
        for path in numbers {
            if path.contains("/latency_ms") || path.contains("/pricing/") || path.ends_with("_usd")
            {
                continue;
            }
            let mut bad = event.clone();
            *bad.pointer_mut(&path).unwrap() = json!(MAX_SAFE_COUNT + 1);
            assert_invalid(bad);
        }
    }
    let calls: ModelCalls = serde_json::from_value(json!({"total":MAX_SAFE_COUNT,
        "rag":MAX_SAFE_COUNT,"judge":0,"embedding":0}))
    .unwrap();
    assert!(calls.validate());
    let tokens: Tokens = serde_json::from_value(json!({"input":MAX_SAFE_COUNT,"output":0,
        "total":MAX_SAFE_COUNT,"known_input":MAX_SAFE_COUNT,"known_output":0,
        "known_total":MAX_SAFE_COUNT,"reported_calls":1}))
    .unwrap();
    assert!(tokens.validate(1));
    let mut metrics = snapshot();
    metrics["context"]["prompt_characters"] = json!(MAX_SAFE_COUNT);
    metrics["context"]["max_prompt_characters"] = json!(MAX_SAFE_COUNT);
    metrics["workflow"]["tool_calls"] = json!(MAX_SAFE_COUNT);
    let valid: EvaluationMetrics = serde_json::from_value(metrics).unwrap();
    assert!(valid.validate());
}
