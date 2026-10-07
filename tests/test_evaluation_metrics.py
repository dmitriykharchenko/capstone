"""Offline counters, accounting, context, and persistence with deterministic clocks."""

import json
from concurrent.futures import CancelledError
from types import SimpleNamespace

import pytest

from src import embeddings
from src.cli import __main__ as cli_main
from src.cli.commands import handle_evaluation_stream
from src.evaluation_metrics import (
    EvaluationMetrics,
    EvaluationPricing,
    PerformanceBudget,
    aggregate_metrics,
    check_performance,
    observe_embedding,
    sanitize_performance,
)
from src.evaluation_runner import EvaluationRunner, validate_test_cases
from src.planner_retry import PlannerRetryPolicy
from test_evaluation_runner import FakeModel, FakeRetriever, case, chunks
from test_evaluation_stages import STAGES, runner as fault_runner


class Clock:
    def __init__(self):
        self.time = 0.0

    def __call__(self):
        return self.time

    def advance(self, seconds):
        self.time += seconds


class UsageModel(FakeModel):
    def __init__(self, clock, **kwargs):
        super().__init__(**kwargs)
        self.clock = clock

    def invoke(self, messages):
        self.clock.advance(0.002)
        response = super().invoke(messages)
        response.usage_metadata = {
            "input_tokens": 10,
            "output_tokens": 4,
            "total_tokens": 14,
        }
        return response


class SameChunks(FakeRetriever):
    def __init__(self, clock):
        super().__init__()
        self.clock = clock

    def retrieve_chunks(self, query, n_results=5):
        self.clock.advance(0.005)
        return chunks("café 📚")


def test_success_measures_actual_calls_context_and_separate_judge():
    clock = Clock()
    model = UsageModel(clock)
    pricing = EvaluationPricing(2, 6)
    result = EvaluationRunner(
        SameChunks(clock), model, clock=clock, pricing=pricing
    ).run([case()], "hybrid")[0]
    assert result["error"] is None
    m = result["metrics"]
    assert m["latency_ms"] == pytest.approx({"total": 22, "rag": 20, "judge": 2})
    assert m["model_calls"] == {"total": 6, "rag": 5, "judge": 1, "embedding": 0}
    assert m["workflow"] == {
        "exchanges": 5,
        "tool_calls": 2,
        "reused_searches": 1,
    }
    assert m["tokens"] == {
        "input": 60,
        "output": 24,
        "total": 84,
        "known_input": 60,
        "known_output": 24,
        "known_total": 84,
        "reported_calls": 6,
    }
    assert m["cost"]["estimated_usd"] == pytest.approx(6 * 44 / 1_000_000)
    assert m["cost"]["rate_estimated_calls"] == 6
    assert m["pricing"]["input_usd_per_million"] == 2
    context = model.calls[1][2].content
    assert m["context"]["chunks"] == 10
    assert m["context"]["unique_chunks"] == 5
    assert m["context"]["duplicate_chunks"] == 0
    assert m["context"]["characters"] == 2 * len(context)
    assert m["context"]["utf8_bytes"] == 2 * len(context.encode("utf-8"))
    assert m["context"]["max_characters"] == len(context)
    assert m["context"]["max_chunks"] == 5
    assert m["context"]["prompt_characters"] == sum(
        len(message.content) for call in model.calls[:-1] for message in call
    )
    assert m["context"]["max_prompt_characters"] == max(
        sum(len(message.content) for message in call) for call in model.calls[:-1]
    )
    assert m["stages"]["composer"]["context"]["chunks"] == 0
    assert m["stages"]["judge"]["context"]["chunks"] == 5
    assert m["stages"]["judge"]["tokens"]["total"] == 14
    assert "café" not in json.dumps(m, ensure_ascii=False)


@pytest.mark.parametrize("stage,hop", STAGES)
@pytest.mark.parametrize("failures", [1, 3])
def test_every_stage_failure_keeps_attempts_without_replaying_metrics(
    stage, hop, failures
):
    evaluation, faults = fault_runner({("q", stage, hop): failures})
    result = evaluation.run([case("q")], "hybrid")[0]
    m = result["metrics"]
    tools = sum("retrieval" in name for _, name, _ in faults.calls)
    model = len(faults.calls) - tools
    subanswers = sum("subanswer" in name for _, name, _ in faults.calls)
    assert m["model_calls"]["total"] == model
    assert m["workflow"]["tool_calls"] == tools
    assert m["context"]["chunks"] == subanswers * 5
    assert sum(bucket["model_calls"] for bucket in m["stages"].values()) == model
    assert (result["error"] is not None) == (failures == 3)
    assert m["tokens"]["total"] is None
    assert m["cost"]["estimated_usd"] is None
    assert m["tokens"]["reported_calls"] == 0
    assert "synthetic-private" not in json.dumps(m)


def test_resume_excludes_waits_and_snapshot_is_immutable():
    clock = Clock()
    model = UsageModel(clock, malformed_judges={"q"})
    evaluation = EvaluationRunner(SameChunks(clock), model, clock=clock)
    state = evaluation.start_case(1, validate_test_cases([case("q")])[0], "hybrid")
    clock.advance(100)
    first = evaluation.resume_case(state, 0)
    first_json = json.dumps(first)
    clock.advance(200)
    model.malformed_judges.clear()
    final = evaluation.resume_case(state, 1)
    assert json.dumps(first) == first_json
    assert final["metrics"]["latency_ms"]["total"] == pytest.approx(24)
    assert final["metrics"]["model_calls"] == {
        "total": 7,
        "rag": 5,
        "judge": 2,
        "embedding": 0,
    }
    assert final["metrics"]["context"] == first["metrics"]["context"]
    assert evaluation.resume_case(state, 2) == final


def test_planner_inner_retries_and_backoff_are_counted():
    from test_evaluation_planner_retry import RetryModel, VALID_PLAN

    clock = Clock()
    retriever = FakeRetriever()
    model = RetryModel([TimeoutError("synthetic-canary"), "{}", VALID_PLAN], retriever)
    policy = PlannerRetryPolicy(sleep=clock.advance)
    result = EvaluationRunner(
        retriever, model, clock=clock, planner_retry_policy=policy
    ).run([case()], "hybrid")[0]
    assert result["error"] is None
    m = result["metrics"]
    assert m["stages"]["planner"]["model_calls"] == 3
    assert m["stages"]["planner"]["exchanges"] == 2
    assert m["latency_ms"]["total"] == 3000
    assert m["model_calls"]["total"] == 8


@pytest.mark.parametrize("shape", ["langchain", "openai", "mapping", "usage_metadata"])
def test_usage_shapes_and_provider_cost_precedence(shape):
    raw = {
        "prompt_tokens": 10,
        "completion_tokens": 4,
        "total_tokens": 14,
        "cost": 0.002,
        "private": "synthetic-canary",
    }
    responses = {
        "langchain": SimpleNamespace(response_metadata={"token_usage": raw}),
        "openai": SimpleNamespace(usage=SimpleNamespace(**raw)),
        "mapping": {"usage": raw},
        "usage_metadata": {
            "usage_metadata": {
                "input_tokens": 10,
                "output_tokens": 4,
                "total_tokens": 14,
            },
            "response_metadata": {"token_usage": raw},
        },
    }
    metrics = EvaluationMetrics(EvaluationPricing(99, 99))
    metrics.call("planner", lambda: responses[shape])
    m = metrics.snapshot()
    assert m["tokens"]["total"] == 14
    assert m["cost"]["estimated_usd"] == 0.002
    assert m["cost"]["provider_reported_calls"] == 1
    assert m["cost"]["rate_estimated_calls"] == 0
    assert "synthetic-canary" not in json.dumps(m)


@pytest.mark.parametrize(
    "bad", [-1, True, 1.5, "2", float("nan"), float("inf"), 10**1000]
)
def test_invalid_token_counts_never_become_zero_or_break_strict_json(bad):
    metrics = EvaluationMetrics()
    metrics.call(
        "planner", lambda: {"usage": {"prompt_tokens": bad, "completion_tokens": 4}}
    )
    m = metrics.snapshot()
    assert m["tokens"]["input"] is None
    assert m["tokens"]["total"] is None
    assert m["tokens"]["output"] == 4
    assert m["cost"]["estimated_usd"] is None
    json.dumps(m, allow_nan=False)


def test_partial_usage_unknown_cost_zero_cost_and_missing_are_distinct():
    metrics = EvaluationMetrics()
    metrics.call(
        "planner",
        lambda: {"usage": {"prompt_tokens": 5, "completion_tokens": 2, "cost": 0}},
    )
    metrics.call("composer", lambda: {"content": "no usage"})
    m = metrics.snapshot()
    assert m["tokens"]["total"] is None
    assert m["tokens"]["known_total"] == 7
    assert m["tokens"]["reported_calls"] == 1
    assert m["cost"]["estimated_usd"] is None
    assert m["cost"]["priced_calls"] == 1
    assert m["cost"]["known_usd"] == 0
    assert m["stages"]["planner"]["cost"]["estimated_usd"] == 0


@pytest.mark.parametrize("batch", [False, True])
def test_embedding_sdk_usage_is_captured_only_in_active_evaluation(monkeypatch, batch):
    response = SimpleNamespace(
        usage=SimpleNamespace(prompt_tokens=12, total_tokens=12),
        data=[SimpleNamespace(index=0, embedding=[0.5])],
    )
    client = SimpleNamespace(
        embeddings=SimpleNamespace(create=lambda **kwargs: response)
    )
    monkeypatch.setattr(embeddings, "_openai_client", lambda *args: client)
    monkeypatch.setattr(embeddings, "_api_key", lambda config: None)
    metrics = EvaluationMetrics(EvaluationPricing(2, 6, 0.1))

    def invoke():
        return (
            embeddings.create_embeddings(["text"])
            if batch
            else embeddings.create_embedding("text")
        )

    with metrics.stage("retrieval"):
        invoke()
    invoke()
    m = metrics.snapshot()
    assert m["model_calls"] == {"total": 1, "rag": 0, "judge": 0, "embedding": 1}
    assert m["tokens"]["input"] == 12
    assert m["tokens"]["output"] == 0
    assert m["cost"]["estimated_usd"] == pytest.approx(0.0000012)
    assert m["workflow"]["exchanges"] == 0


def test_terminal_failure_clears_observer_and_preserves_unknown_attempt():
    metrics = EvaluationMetrics()
    with pytest.raises(CancelledError), metrics.stage("retrieval"):
        observe_embedding(lambda: (_ for _ in ()).throw(CancelledError()))
    observe_embedding(lambda: {"usage": {"prompt_tokens": 5}})
    assert metrics.snapshot()["model_calls"]["embedding"] == 1
    assert metrics.snapshot()["tokens"]["input"] is None


def test_stream_and_artifact_aggregate_final_snapshots_once(tmp_path):
    evaluation, faults = fault_runner(
        {("a", "subanswer", 2): 1, ("b", "judge", None): 3}
    )
    inputs = [case("a"), case("b")]
    events = list(
        cli_main._persisted_evaluation_stream(
            handle_evaluation_stream(
                None, {"test_cases": inputs, "retrieval_type": "hybrid"}, evaluation
            ),
            results_path=tmp_path / "eval_results.json",
        )
    )
    artifact = json.loads(next(tmp_path.glob("eval_results_*.json")).read_text())
    assert artifact["schema_version"] == 3
    final = artifact["aggregate_metrics"]
    assert final == events[-1]["aggregate_metrics"]
    assert final == aggregate_metrics(artifact["results"])
    assert final["cases_measured"] == final["cases_total"] == 2
    assert final["model_calls"]["total"] == sum(
        "retrieval" not in stage for _, stage, _ in faults.calls
    )
    assert final["context"]["max_chunks"] == 5
    assert final["tokens"]["total"] is None
    assert artifact["results"][1]["error"] is not None


def test_aggregate_legacy_coverage_and_latency_percentiles():
    first, second = EvaluationMetrics().snapshot(), EvaluationMetrics().snapshot()
    first["latency_ms"] = {"total": 10, "rag": 8, "judge": 2}
    second["latency_ms"] = {"total": 30, "rag": 24, "judge": 6}
    summary = aggregate_metrics([{"metrics": first}, {"metrics": second}, {}])
    assert summary["cases_measured"] == 2
    assert summary["cases_total"] == 3
    assert summary["latency_ms"] == {
        "total": 40,
        "rag": 32,
        "judge": 8,
        "mean": 20,
        "p50": 20,
        "p95": 29,
    }
    assert aggregate_metrics([{}])["cases_measured"] == 0


@pytest.mark.parametrize("bad", ["nan", "inf", "-1", "synthetic-canary"])
def test_invalid_price_config_fails_without_echoing_values(monkeypatch, bad):
    monkeypatch.setenv("CAPSTONE_EVAL_INPUT_USD_PER_MILLION", bad)
    with pytest.raises(ValueError) as error:
        EvaluationPricing.from_env()
    assert "synthetic-canary" not in str(error.value)


def test_price_config_and_zero_rates(monkeypatch):
    for kind, value in (("INPUT", "0"), ("OUTPUT", "2.5"), ("EMBEDDING", "")):
        monkeypatch.setenv(f"CAPSTONE_EVAL_{kind}_USD_PER_MILLION", value)
    assert EvaluationPricing.from_env() == EvaluationPricing(0, 2.5, None)


@pytest.mark.parametrize(
    "path,value",
    [
        (("version",), True),
        (("version",), 2),
        (("tokens", "input"), "synthetic-canary"),
        (("cost", "estimated_usd"), float("nan")),
        (("model_calls", "rag"), -1),
        (("stages", "synthetic-canary"), {}),
        (("context", "private"), "synthetic-canary"),
    ],
)
def test_publication_rejects_untrusted_metrics_without_changing_results(path, value):
    from src.evaluation_diagnostics import sanitize_result_diagnostics

    metrics = EvaluationMetrics().snapshot()
    target = metrics
    for key in path[:-1]:
        target = target[key]
    target[path[-1]] = value
    result = {"metrics": metrics, "error": None, "generated_answer": "answer"}
    safe = sanitize_result_diagnostics(result)
    assert "metrics" not in safe
    assert safe["generated_answer"] == "answer"
    assert aggregate_metrics([result])["cases_measured"] == 0


def measured_snapshot(latency_seconds=2.0, usage=True):
    clock = Clock()
    metrics = EvaluationMetrics(EvaluationPricing(1, 2, None), clock)
    response = (
        {"usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}
        if usage
        else {}
    )
    with metrics.stage("planner"):
        clock.advance(latency_seconds)
        metrics.call("planner", lambda: response)
    with metrics.stage("retrieval"):
        MeasuredRetrieverStub(metrics).retrieve_chunks("q")
    with metrics.stage("judge"):
        clock.advance(100)
        metrics.call("judge", lambda: {})
    return metrics.snapshot()


class MeasuredRetrieverStub:
    def __init__(self, metrics):
        from src.evaluation_metrics import MeasuredRetriever

        self.inner = MeasuredRetriever(FakeRetriever(), metrics, "retrieval")

    def retrieve_chunks(self, query):
        return self.inner.retrieve_chunks(query)


def test_performance_checks_exclude_judge_and_fail_on_any_breach():
    budget = PerformanceBudget(max_rag_tokens=100, max_cost_usd=1)
    passed = check_performance(
        measured_snapshot(), budget, followup_queries=0, completed=True
    )
    assert passed["status"] == "pass"
    assert passed["checks"]["rag_latency_ms"] == {
        "value": 2000,
        "limit": 60_000,
        "status": "pass",
    }
    assert passed["checks"]["rag_model_calls"]["value"] == 1
    assert passed["checks"]["retrievals"]["value"] == 1
    assert passed["checks"]["rag_tokens"]["value"] == 15
    assert passed["checks"]["rag_cost_usd"]["value"] == pytest.approx(0.00002)
    assert passed["checks"]["duplicate_chunks"] == {
        "value": 0,
        "limit": 0,
        "status": "pass",
    }

    slow = check_performance(
        measured_snapshot(latency_seconds=61),
        budget,
        followup_queries=0,
        completed=True,
    )
    assert slow["status"] == "fail"
    assert slow["checks"]["rag_latency_ms"]["status"] == "fail"
    assert "rag_tokens" not in check_performance(
        measured_snapshot(), PerformanceBudget(), followup_queries=0, completed=True
    )["checks"]


def test_duplicate_chunk_check_counts_only_non_reused_submissions():
    chunks = [SimpleNamespace(paper_id="paper", chunk_index=0, text="same evidence")]
    metrics = EvaluationMetrics()
    metrics.call("subanswer", lambda: {}, chunks=chunks)
    metrics.call("subanswer", lambda: {}, chunks=chunks)
    metrics.call("subanswer", lambda: {}, chunks=chunks, reused=True)
    snapshot = metrics.snapshot()
    assert snapshot["context"]["duplicate_chunks"] == 1
    checks = check_performance(
        snapshot, PerformanceBudget(), followup_queries=0, completed=True
    )
    assert checks["checks"]["duplicate_chunks"]["status"] == "fail"


def test_unreported_usage_or_unfinished_cases_are_never_pass():
    budget = PerformanceBudget(max_rag_tokens=100)
    unknown = check_performance(
        measured_snapshot(usage=False), budget, followup_queries=None, completed=True
    )
    assert unknown["status"] == "incomplete"
    assert unknown["checks"]["rag_tokens"]["status"] == "unknown"
    assert unknown["checks"]["followup_queries"]["status"] == "unknown"
    unfinished = check_performance(
        measured_snapshot(), PerformanceBudget(), followup_queries=0, completed=False
    )
    assert unfinished["status"] == "incomplete"


def test_performance_publication_and_aggregation_reject_untrusted_shapes():
    from src.evaluation_diagnostics import sanitize_result_diagnostics

    snapshot = measured_snapshot()
    good = check_performance(
        snapshot,
        PerformanceBudget(max_followup_queries=0),
        followup_queries=2,
        completed=True,
    )
    assert sanitize_performance(good) == good
    for bad in (
        {**good, "version": 2},
        {**good, "status": "ok"},
        {**good, "checks": {"private": {}}},
        {
            **good,
            "checks": {
                "retrievals": {"value": None, "limit": 1, "status": "pass"}
            },
        },
    ):
        assert sanitize_performance(bad) is None
        assert "performance" not in sanitize_result_diagnostics({"performance": bad})
    passed = check_performance(
        snapshot, PerformanceBudget(), followup_queries=0, completed=True
    )
    summary = aggregate_metrics(
        [
            {"metrics": snapshot, "performance": good},
            {"metrics": snapshot, "performance": passed},
            {"performance": {"status": "pass"}},
        ]
    )["performance"]
    assert summary == {
        "cases_checked": 2,
        "pass": 1,
        "fail": 1,
        "incomplete": 0,
        "failed_checks": {"followup_queries": 1},
    }


def test_inconsistent_total_is_unknown_but_known_components_remain():
    metrics = EvaluationMetrics()
    metrics.call(
        "planner",
        lambda: {
            "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 7}
        },
    )
    assert metrics.snapshot()["tokens"]["total"] is None
    assert metrics.snapshot()["tokens"]["input"] == 10


def test_interleaved_cases_have_independent_usage_and_embedding_observers():
    outer, inner = EvaluationMetrics(), EvaluationMetrics()
    response = {"usage": {"prompt_tokens": 2, "total_tokens": 2}}
    with outer.stage("retrieval"):
        observe_embedding(lambda: response)
        with inner.stage("retrieval"):
            observe_embedding(lambda: response)
        observe_embedding(lambda: response)
    assert outer.snapshot()["tokens"]["total"] == 4
    assert inner.snapshot()["tokens"]["total"] == 2


def test_cli_builder_loads_explicit_evaluation_rates(monkeypatch, tmp_path):
    from src.cli import models
    from test_cli_evaluation_stream import config

    monkeypatch.setattr(
        models, "build_paper_search_index", lambda config: FakeRetriever()
    )
    monkeypatch.setenv("CAPSTONE_EVAL_INPUT_USD_PER_MILLION", "1")
    monkeypatch.setenv("CAPSTONE_EVAL_OUTPUT_USD_PER_MILLION", "3")
    monkeypatch.setenv("CAPSTONE_EVAL_EMBEDDING_USD_PER_MILLION", "0.1")
    evaluation = models.build_evaluation_runner(config(tmp_path))
    state = evaluation.start_case(1, validate_test_cases([case()])[0], "hybrid")
    assert state.metrics.pricing == EvaluationPricing(1, 3, 0.1)
