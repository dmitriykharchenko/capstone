"""Offline diagnostics contracts, including provider canaries and all stage cursors."""

import asyncio
import json
from concurrent.futures import CancelledError
from types import SimpleNamespace

import httpx
import openai
import pytest

from src.chat.agent_loop import (
    AgentConfigurationError,
    AgentIncompleteResponseError,
    AgentPlanError,
)
from src.cli import __main__ as cli_main
from src.cli.commands import handle_evaluation_stream
from src.evaluation.diagnostics import (
    EvaluationRefusalError,
    classify_failure,
    failure_record,
    sanitize_result_diagnostics,
    valid_record,
)
from src.evaluation.results import evaluation_results_artifact
from src.evaluation.runner import EvaluationRunner
from src.chat.planner_retry import PlannerRetryPolicy
from test_evaluation_runner import FakeModel, FakeRetriever, case
from test_evaluation_stages import STAGES, runner

CANARY = "synthetic-untrusted-diagnostic-canary"


def http_error(status, sdk=False):
    response = httpx.Response(
        status, request=httpx.Request("POST", "https://offline.invalid"), text=CANARY
    )
    return (
        openai.APIStatusError(CANARY, response=response, body={"raw": CANARY})
        if sdk
        else httpx.HTTPStatusError(CANARY, request=response.request, response=response)
    )


@pytest.mark.parametrize("sdk", [False, True])
@pytest.mark.parametrize(
    ("status", "category"),
    [
        (400, "http_client_error"),
        (401, "authentication"),
        (402, "payment_required"),
        (403, "authorization"),
        (408, "timeout"),
        (409, "http_client_error"),
        (422, "http_client_error"),
        (429, "rate_limit"),
        (500, "http_server_error"),
        (599, "http_server_error"),
    ],
)
def test_typed_http_status_classification(status, category, sdk):
    error = http_error(status, sdk)
    assert classify_failure(error, "planner") == (category, status)
    record = failure_record("planner", classify_failure(error, "planner"), 0, 1)
    assert valid_record(record)
    assert CANARY not in json.dumps(record)
    assert record["retryable"] == (status in {408, 409, 429} or status >= 500)


@pytest.mark.parametrize(
    ("error", "stage", "category"),
    [
        (TimeoutError(CANARY), "planner", "timeout"),
        (httpx.ReadTimeout(CANARY), "judge", "timeout"),
        (ConnectionError(CANARY), "composer", "connection"),
        (httpx.ConnectError(CANARY), "composer", "connection"),
        (AgentIncompleteResponseError(CANARY), "composer", "incomplete_response"),
        (EvaluationRefusalError(CANARY), "planner", "refusal"),
        (AgentPlanError(CANARY), "planner", "invalid_planner_output"),
        (RuntimeError(CANARY), "retrieval", "retrieval_failure"),
        (CancelledError(CANARY), "judge", "cancellation"),
        (asyncio.CancelledError(CANARY), "judge", "cancellation"),
        (AgentConfigurationError(CANARY), "planner", "configuration"),
        (RuntimeError(CANARY), "judge", "unknown"),
    ],
)
def test_type_only_categories(error, stage, category):
    assert classify_failure(error, stage) == (category, None)


@pytest.mark.parametrize(("stage", "hop"), STAGES)
def test_402_at_every_stage_retains_three_ordered_attempts(
    stage, hop, tmp_path, capsys
):
    evaluation, faults = runner(
        {("q", stage, hop): 3}, error_type=lambda _: http_error(402)
    )
    events = list(
        cli_main._persisted_evaluation_stream(
            handle_evaluation_stream(
                None,
                {"test_cases": [case("q")], "retrieval_type": "hybrid"},
                evaluation,
            ),
            results_path=tmp_path / "results.json",
        )
    )
    results = [event["result"] for event in events if "result" in event]
    final = results[-1]
    assert [item["retry_round"] for item in final["attempt_history"]] == [0, 1, 2]
    assert [item["stage_attempt"] for item in final["attempt_history"]] == [1, 2, 3]
    for item in final["attempt_history"]:
        assert item["stage"] == stage and item["hop_index"] == hop
        assert item["category"] == "payment_required" and item["http_status"] == 402
        assert item["retryable"] is False
    assert final["error"] == final["attempt_history"][-1]
    assert len(results[0]["attempt_history"]) == 1
    files = list(tmp_path.glob("results_*.json"))
    assert len(files) == 1
    artifact = json.loads(files[0].read_text())
    assert artifact["schema_version"] == 3
    assert artifact["results"] == [final]
    assert faults.counts[("q", stage, hop)] == 3
    assert sum(e["event"] == "complete" for e in events) == 1
    assert CANARY not in json.dumps(events) + files[0].read_text() + repr(
        evaluation.__dict__
    )
    captured = capsys.readouterr()
    assert CANARY not in captured.out + captured.err


@pytest.mark.parametrize(
    ("kind", "expected", "immediate"),
    [
        ("schema", "invalid_planner_output", 3),
        ("incomplete", "incomplete_response", 3),
        ("refusal", "refusal", 1),
        ("timeout", "timeout", 3),
        ("payment", "payment_required", 1),
    ],
)
def test_planner_last_attempt_reason_preserves_immediate_budgets(
    kind, expected, immediate
):
    class Model(FakeModel):
        count = 0

        def invoke(self, messages):
            self.count += 1
            if kind == "timeout":
                raise TimeoutError(CANARY)
            if kind == "payment":
                raise http_error(402)
            return SimpleNamespace(
                content=CANARY,
                additional_kwargs={"refusal": CANARY} if kind == "refusal" else {},
                response_metadata={"finish_reason": "length"}
                if kind == "incomplete"
                else {},
            )

    model, delays = Model(), []
    result = EvaluationRunner(
        FakeRetriever(),
        model,
        planner_retry_policy=PlannerRetryPolicy(sleep=delays.append),
    ).run([case()], "hybrid")[0]
    assert model.count == immediate * 3
    assert len(delays) == (immediate - 1) * 3
    assert result["error"]["category"] == expected
    assert CANARY not in json.dumps(result)


@pytest.mark.parametrize(
    "kind",
    [
        "reverse",
        "too_long",
        "wrong_attempt",
        "wrong_final",
        "unknown_field",
        "not_array",
    ],
)
def test_artifact_boundary_sanitizes_untrusted_history(kind):
    history = [
        failure_record("planner", ("payment_required", 402), i, i + 1) for i in range(3)
    ]
    result = {"error": dict(history[-1]), "attempt_history": history}
    if kind == "reverse":
        history.reverse()
    if kind == "too_long":
        history.append(history[-1])
    if kind == "wrong_attempt":
        history[1]["stage_attempt"] = 1
    if kind == "wrong_final":
        result["error"]["category"] = "unknown"
    if kind == "unknown_field":
        history[0]["body"] = CANARY
    if kind == "not_array":
        result["attempt_history"] = CANARY
    safe = sanitize_result_diagnostics(result)
    assert safe["attempt_history"] == [] and safe["error"]["category"] == "unknown"
    artifact = evaluation_results_artifact(
        input_cases=[],
        results=[result],
        retrieval_strategy="hybrid",
        aggregate_scores={},
        succeeded=0,
        failed=1,
    )
    assert CANARY not in json.dumps(artifact)
