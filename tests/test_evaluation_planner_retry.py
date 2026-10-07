import asyncio
import json
import traceback
from concurrent.futures import CancelledError
from datetime import datetime, timezone
from email.utils import format_datetime
from types import SimpleNamespace

import httpx
import openai
import pytest

from src.agent_loop import (
    AgentConfigurationError,
    AgentIncompleteResponseError,
    AgentLoop,
    AgentModelError,
    ChatOpenAIConfig,
    _create_chat_openai,
    run_multihop,
)
from src.cli import __main__ as cli_main
from src.cli.commands import handle_evaluation_stream
from src.evaluation_runner import EvaluationRunner
from src.planner_retry import PLAN_CORRECTION, PlannerRetryPolicy
from test_cli_evaluation_stream import config, published_artifact
from test_evaluation_runner import FakeModel, FakeRetriever, case, model_stages

PRIVATE = "synthetic-private-provider-canary"
VALID_PLAN = '{"subqueries":["alpha evidence","beta evidence"]}'


def status_error(status, retry_after=None, *, transport=False):
    request = httpx.Request("POST", f"https://example.invalid/{PRIVATE}")
    response = httpx.Response(
        status,
        request=request,
        headers={} if retry_after is None else {"Retry-After": retry_after},
    )
    if transport:
        return httpx.HTTPStatusError(PRIVATE, request=request, response=response)
    return openai.APIStatusError(PRIVATE, response=response, body={"error": PRIVATE})


class RetryModel(FakeModel):
    def __init__(self, attempts, retriever):
        super().__init__()
        self.attempts = iter(attempts)
        self.retriever = retriever
        self.planner_retrieval_counts = []

    def invoke(self, messages):
        if "MULTIHOP_PLANNER" in messages[0].content:
            self.calls.append(messages)
            self.planner_retrieval_counts.append(len(self.retriever.calls))
            result = next(self.attempts)
            if isinstance(result, BaseException):
                raise result
            return (
                SimpleNamespace(content=result) if isinstance(result, str) else result
            )
        return super().invoke(messages)


def runner(attempts, **policy_options):
    retriever = FakeRetriever()
    model = RetryModel(attempts, retriever)
    delays = []
    policy = PlannerRetryPolicy(sleep=delays.append, **policy_options)
    evaluation = EvaluationRunner(retriever, model, planner_retry_policy=policy)
    return evaluation, model, retriever, delays


@pytest.mark.parametrize("status", [408, 409, 429, 500, 502, 503, 504, 599])
@pytest.mark.parametrize("transport", [False, True])
def test_transient_status_recovers_before_retrieving_each_hop_once(status, transport):
    evaluation, model, retriever, delays = runner(
        [status_error(status, transport=transport), VALID_PLAN]
    )
    result = evaluation.run([case()], "hybrid")[0]
    assert result["error"] is None
    assert model.planner_retrieval_counts == [0, 0]
    assert retriever.calls == [("alpha evidence", 5), ("beta evidence", 10)]
    assert model_stages(model) == [
        "planner",
        "planner",
        "subanswer",
        "subanswer",
        "coverage",
        "composer",
        "judge",
    ]
    assert delays == [1.0]
    assert model.calls[0] == model.calls[1]


@pytest.mark.parametrize(
    "error",
    [
        TimeoutError(PRIVATE),
        ConnectionError(PRIVATE),
        httpx.ConnectTimeout(PRIVATE),
        httpx.ReadTimeout(PRIVATE),
        httpx.ConnectError(PRIVATE),
        httpx.ReadError(PRIVATE),
        openai.APITimeoutError(httpx.Request("POST", "https://example.invalid")),
        openai.APIConnectionError(
            message=PRIVATE, request=httpx.Request("POST", "https://example.invalid")
        ),
    ],
)
def test_typed_timeouts_and_connection_failures_recover(error):
    evaluation, model, _, delays = runner([error, VALID_PLAN])
    assert evaluation.run([case()], "hybrid")[0]["error"] is None
    assert model.planner_retrieval_counts == [0, 0]
    assert delays == [1.0]


@pytest.mark.parametrize(
    "invalid",
    [
        "",
        PRIVATE,
        '{"subqueries":["incomplete",',
        '{"subqueries":["alpha"," ALPHA "]}',
        '{"subqueries":["a","b","c","d","e","f"]}',
        '{"subqueries":["a"]}',
        '{"subqueries":["a",null]}',
        '{"subqueries":["a","b"],"extra":true}',
        '{"subqueries":["a","b"],"subqueries":["c","d"]}',
        '{"subqueries":["a",NaN]}',
        "```json\n" + VALID_PLAN + "\n```",
        SimpleNamespace(content=PRIVATE, response_metadata={"finish_reason": "length"}),
        AgentIncompleteResponseError(PRIVATE),
        openai.LengthFinishReasonError(completion=SimpleNamespace(usage=None)),
    ],
)
def test_invalid_or_incomplete_plan_gets_fixed_correction_without_rejected_output(
    invalid,
):
    evaluation, model, retriever, delays = runner([invalid, invalid, VALID_PLAN])
    result = evaluation.run([case()], "hybrid")[0]
    assert result["error"] is None
    assert model.planner_retrieval_counts == [0, 0, 0]
    assert len(retriever.calls) == 2
    assert delays == [1.0, 2.0]
    for messages in model.calls[1:3]:
        assert [m.content for m in messages].count(PLAN_CORRECTION) == 1
        assert PRIVATE not in "\n".join(m.content for m in messages)
        assert messages[-1].content == case()["question"]


@pytest.mark.parametrize("status", [400, 401, 402, 403, 404, 422])
def test_permanent_status_is_one_attempt_per_round_even_with_retry_after(status):
    evaluation, model, retriever, delays = runner(
        [status_error(status, "999")] * 3 + [VALID_PLAN]
    )
    result = evaluation.run([case()], "hybrid")[0]
    assert result["error"]["stage"] == "planner"
    assert PRIVATE not in json.dumps(result)
    assert model_stages(model) == ["planner"] * 3
    assert retriever.calls == delays == []


@pytest.mark.parametrize(
    "failure",
    [
        ValueError(PRIVATE),
        AgentConfigurationError(PRIVATE),
        RuntimeError("429 timeout " + PRIVATE),
        PermissionError(PRIVATE),
        openai.ContentFilterFinishReasonError(),
        SimpleNamespace(content=PRIVATE, additional_kwargs={"refusal": PRIVATE}),
        SimpleNamespace(
            content=VALID_PLAN, response_metadata={"finish_reason": "content_filter"}
        ),
        SimpleNamespace(content=[{"type": "refusal", "refusal": PRIVATE}]),
    ],
)
def test_other_permanent_failures_and_refusals_have_no_immediate_retry(failure):
    evaluation, model, retriever, delays = runner([failure] * 3 + [VALID_PLAN])
    result = evaluation.run([case()], "hybrid")[0]
    assert result["error"]["stage"] == "planner"
    assert PRIVATE not in json.dumps(result)
    assert model_stages(model) == ["planner"] * 3
    assert retriever.calls == delays == []


@pytest.mark.parametrize("error_type", [asyncio.CancelledError, CancelledError])
def test_cancellation_propagates_without_retry(error_type):
    evaluation, model, retriever, delays = runner([error_type(), VALID_PLAN])
    with pytest.raises(error_type):
        evaluation.run([case()], "hybrid")
    assert model_stages(model) == ["planner"]
    assert retriever.calls == delays == []


@pytest.mark.parametrize("maximum", [1, 2, 3])
@pytest.mark.parametrize("invalid", [False, True])
def test_exhaustion_is_sanitized_and_next_case_continues(
    maximum, invalid, capsys, caplog
):
    failure = PRIVATE if invalid else status_error(429)
    evaluation, model, retriever, delays = runner(
        [*[failure] * maximum, VALID_PLAN, *[failure] * (maximum * 2)],
        max_attempts=maximum,
    )
    results = evaluation.run([case("fails"), case("succeeds")], "hybrid")
    assert results[0]["error"]["stage"] == "planner"
    assert results[0]["retrieved_chunks"] == []
    assert results[0]["generated_answer"] is None
    assert results[0]["judge_scores"] is None
    assert results[1]["error"] is None
    assert model.planner_retrieval_counts == [0] * (maximum + 1) + [2] * (maximum * 2)
    assert len(retriever.calls) == 2
    assert delays == [1.0, 2.0][: maximum - 1] * 3
    assert PRIVATE not in json.dumps(results) + caplog.text + str(capsys.readouterr())


@pytest.mark.parametrize("failure", [PRIVATE, status_error(503)])
def test_public_planner_error_has_no_provider_chain(failure):
    _, model, retriever, delays = runner([failure] * 3)
    with pytest.raises(AgentModelError) as raised:
        run_multihop(
            model,
            retriever,
            "question",
            planner_retry_policy=PlannerRetryPolicy(sleep=delays.append),
        )
    error = raised.value
    assert error.stage == "planner"
    assert error.__cause__ is error.__context__ is None
    assert PRIVATE not in "".join(traceback.format_exception(error))
    assert retriever.calls == []


@pytest.mark.parametrize("caller", ["stateless", "chat"])
@pytest.mark.parametrize("failure", [status_error(429), PRIVATE])
def test_non_evaluation_callers_keep_one_attempt(caller, failure, tmp_path):
    _, model, retriever, delays = runner([failure, VALID_PLAN])
    with pytest.raises(AgentModelError):
        if caller == "stateless":
            run_multihop(model, retriever, "question")
        else:
            with AgentLoop(
                tmp_path / "chat.db", model, paper_retriever=retriever
            ) as chat:
                chat.run("session", "question")
    assert model_stages(model) == ["planner"]
    assert retriever.calls == delays == []


@pytest.mark.parametrize("stage", ["subanswer", "coverage", "composer", "judge"])
def test_downstream_provider_failure_never_replays_case(stage):
    evaluation, model, retriever, delays = runner([VALID_PLAN])
    original = model.invoke

    def invoke(messages):
        is_stage = (
            messages[0].content.startswith("You are an evaluation judge")
            if stage == "judge"
            else f"MULTIHOP_{stage.upper()}" in "\n".join(m.content for m in messages)
        )
        if is_stage:
            model.calls.append(messages)
            raise status_error(429)
        return original(messages)

    model.invoke = invoke
    result = evaluation.run([case()], "hybrid")[0]
    assert result["error"]["stage"] == stage
    assert model_stages(model).count("planner") == 1
    assert model_stages(model).count(stage) == 3
    assert len(retriever.calls) == (1 if stage == "subanswer" else 2)
    assert delays == []
    assert PRIVATE not in json.dumps(result)


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        ("0", 1),
        ("5", 5),
        ("999999", 8),
        ("-1", 1),
        ("NaN", 1),
        ("inf", 1),
        ("1.5", 1),
        (PRIVATE, 1),
        ("9" * 129, 1),
        (None, 1),
    ],
)
def test_retry_after_seconds_validation_and_cap(value, expected):
    assert PlannerRetryPolicy().provider_delay(status_error(429, value), 1) == expected


@pytest.mark.parametrize(("offset", "expected"), [(-1, 1), (0, 1), (5, 5), (1000, 8)])
def test_http_date_uses_injected_clock_and_cap(offset, expected):
    now = 1800000000
    date = format_datetime(
        datetime.fromtimestamp(now + offset, timezone.utc), usegmt=True
    )
    assert PlannerRetryPolicy(clock=lambda: now).delay(1, date) == expected


def test_backoff_is_deterministic_and_bounded():
    policy = PlannerRetryPolicy()
    assert [policy.delay(n) for n in range(1, 8)] == [1, 2, 4, 8, 8, 8, 8]


def test_transient_status_attribute_or_cause_does_not_make_unknown_error_retryable():
    error = RuntimeError(PRIVATE)
    error.status_code = 429
    error.__cause__ = TimeoutError(PRIVATE)
    evaluation, model, retriever, delays = runner([error] * 3 + [VALID_PLAN])
    assert evaluation.run([case()], "hybrid")[0]["error"]["stage"] == "planner"
    assert model_stages(model) == ["planner"] * 3
    assert retriever.calls == delays == []


def test_transient_retrieval_failure_never_restarts_planning():
    evaluation, model, retriever, delays = runner([VALID_PLAN])

    def retrieve(query, n_results=5):
        retriever.calls.append((query, n_results))
        raise status_error(503)

    retriever.retrieve_chunks = retrieve
    result = evaluation.run([case()], "hybrid")[0]
    assert result["error"]["stage"] == "retrieval"
    assert model_stages(model) == ["planner"]
    assert len(retriever.calls) == 3
    assert delays == []
    assert PRIVATE not in json.dumps(result)


def test_configuration_failure_does_not_enter_planner_retry_loop(monkeypatch):
    calls = []

    def create(*args, **kwargs):
        calls.append(1)
        raise AgentConfigurationError(PRIVATE)

    monkeypatch.setattr("src.evaluation_runner._create_chat_openai", create)
    retriever = FakeRetriever()
    result = EvaluationRunner(retriever).run([case()], "hybrid")[0]
    assert result["error"]["stage"] == "planner"
    assert calls == [1, 1, 1]
    assert retriever.calls == []
    assert PRIVATE not in json.dumps(result)


def test_mixed_schema_and_provider_failures_use_one_shared_budget():
    evaluation, model, retriever, delays = runner(
        [PRIVATE, status_error(429), VALID_PLAN]
    )
    assert evaluation.run([case()], "hybrid")[0]["error"] is None
    assert model.planner_retrieval_counts == [0, 0, 0]
    assert len(retriever.calls) == 2
    assert delays == [1, 2]
    assert model.calls[1] == model.calls[2]


def test_permanent_failure_stops_remaining_retries():
    evaluation, model, retriever, delays = runner(
        [status_error(503), status_error(401)] * 3 + [VALID_PLAN]
    )
    assert evaluation.run([case()], "hybrid")[0]["error"]["stage"] == "planner"
    assert model_stages(model) == ["planner", "planner"] * 3
    assert delays == [1] * 3
    assert retriever.calls == []


@pytest.mark.parametrize("maximum", [0, 4, -1, True, 1.5])
def test_attempt_limit_cannot_exceed_three(maximum):
    with pytest.raises(ValueError):
        PlannerRetryPolicy(max_attempts=maximum)


def test_default_evaluation_disables_sdk_retries_without_changing_chat(monkeypatch):
    calls = []

    def create(**kwargs):
        calls.append(kwargs)
        return FakeModel()

    monkeypatch.setattr("langchain_openai.ChatOpenAI", create)
    model_config = ChatOpenAIConfig(api_key="synthetic-test-key")
    evaluation = EvaluationRunner(FakeRetriever(), config=model_config)
    assert evaluation.run([case()], "hybrid")[0]["error"] is None
    _create_chat_openai(model_config)
    assert calls[0]["max_retries"] == 0
    assert "max_retries" not in calls[1]


def test_retry_stream_publishes_once_only_after_all_cases(tmp_path, caplog, capsys):
    evaluation, model, retriever, delays = runner(
        [
            status_error(429, "999"),
            VALID_PLAN,
            *[status_error(503)] * 3,
            PRIVATE,
            VALID_PLAN,
            *[status_error(503)] * 6,
        ]
    )
    cases = [case("recovered"), case("exhausted"), case("later")]
    template = tmp_path / "eval_results.json"
    stream = cli_main._persisted_evaluation_stream(
        handle_evaluation_stream(
            config(tmp_path),
            {"retrieval_type": "hybrid", "test_cases": cases},
            evaluation,
        ),
        results_path=template,
    )
    events = []
    for event in stream:
        events.append(event)
        assert len(list(tmp_path.glob("eval_results_*.json"))) == (
            1 if event["event"] == "complete" else 0
        )
    assert [e["event"] for e in events] == [
        "run_start",
        "case_start",
        "case_success",
        "case_start",
        "case_failure",
        "case_start",
        "case_success",
        "retry_round_start",
        "case_retry_start",
        "case_retry_result",
        "retry_round_start",
        "case_retry_start",
        "case_retry_result",
        "complete",
    ]
    assert model.planner_retrieval_counts == [0, 0, 2, 2, 2, 2, 2] + [4] * 6
    assert len(retriever.calls) == 4
    assert (
        model_stages(model).count("composer") == model_stages(model).count("judge") == 2
    )
    assert delays == [8, 1, 2, 1, 1, 2, 1, 2]
    assert (events[-1]["succeeded"], events[-1]["failed"]) == (2, 1)
    assert set(events[-1]["aggregate_scores"].values()) == {4.0}
    artifact = published_artifact(template)
    assert len(artifact["results"]) == 3
    assert PRIVATE not in json.dumps(events) + json.dumps(artifact) + caplog.text + str(
        capsys.readouterr()
    )


def test_all_exhausted_cases_still_publish_one_artifact_without_scores(tmp_path):
    evaluation, model, retriever, delays = runner([status_error(503)] * 18)
    template = tmp_path / "eval_results.json"
    events = list(
        cli_main._persisted_evaluation_stream(
            handle_evaluation_stream(
                config(tmp_path),
                {
                    "retrieval_type": "hybrid",
                    "test_cases": [case("first"), case("second")],
                },
                evaluation,
            ),
            results_path=template,
        )
    )
    assert [e["event"] for e in events] == [
        "run_start",
        "case_start",
        "case_failure",
        "case_start",
        "case_failure",
        *(
            [
                "retry_round_start",
                "case_retry_start",
                "case_retry_result",
                "case_retry_start",
                "case_retry_result",
            ]
            * 2
        ),
        "complete",
    ]
    assert len(model.calls) == 18
    assert retriever.calls == []
    assert delays == [1, 2, 1, 2] * 3
    artifact = published_artifact(template)
    assert artifact["summary"] == {"total": 2, "succeeded": 0, "failed": 2}
    assert set(artifact["aggregate_scores"].values()) == {None}
