"""Offline counters, accounting, context, and persistence with deterministic clocks."""

import json
from types import SimpleNamespace

import pytest

from src.retrieval import embeddings
from src.evaluation.metrics import (
    EvaluationMetrics,
    EvaluationPricing,
)
from src.evaluation.runner import validate_test_cases
from test_evaluation_runner import (
    EvaluationRunner,
    FakeModel,
    FakeRetriever,
    case,
    chunks,
)
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


def test_cached_input_discount_fallback_unknown_and_provider_precedence():
    usage = {
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "prompt_tokens_details": {"cached_tokens": 6},
        }
    }
    discounted = EvaluationMetrics(EvaluationPricing(2, 6, None, 0.5))
    discounted.call("judge", lambda: usage)
    assert discounted.snapshot()["cost"]["estimated_usd"] == pytest.approx(
        35 / 1_000_000
    )

    fallback = EvaluationMetrics(EvaluationPricing(2, 6))
    fallback.call("judge", lambda: usage)
    assert fallback.snapshot()["cost"]["estimated_usd"] == pytest.approx(44 / 1_000_000)

    unknown = EvaluationMetrics(EvaluationPricing(2, 6, None, 0.5))
    unknown.call(
        "judge", lambda: {"usage": {"prompt_tokens": 10, "completion_tokens": 4}}
    )
    assert unknown.snapshot()["cost"]["estimated_usd"] is None

    provider = EvaluationMetrics(EvaluationPricing(99, 99, None, 0.5))
    provider.call("judge", lambda: {"usage": {**usage["usage"], "cost": 0.002}})
    assert provider.snapshot()["cost"]["estimated_usd"] == 0.002
    assert provider.snapshot()["cost"]["provider_reported_calls"] == 1


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


@pytest.mark.parametrize("bad", ["nan", "inf", "-1", "synthetic-canary"])
def test_invalid_price_config_fails_without_echoing_values(monkeypatch, bad):
    monkeypatch.setenv("CAPSTONE_EVAL_INPUT_USD_PER_MILLION", bad)
    with pytest.raises(ValueError) as error:
        EvaluationPricing.from_env()
    assert "synthetic-canary" not in str(error.value)
