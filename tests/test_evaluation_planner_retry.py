import json
from datetime import datetime, timezone
from email.utils import format_datetime
from types import SimpleNamespace

import httpx
import openai
import pytest

from src.chat.agent_loop import (
    AgentIncompleteResponseError,
    ChatOpenAIConfig,
    _create_chat_openai,
)
from src.cli import __main__ as cli_main
from src.cli.commands import handle_evaluation_stream
from src.chat.planner_retry import PLAN_CORRECTION, PlannerRetryPolicy
from test_cli_evaluation_stream import config, published_artifact
from test_evaluation_runner import (
    EvaluationRunner,
    FakeModel,
    FakeRetriever,
    case,
    model_stages,
)

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


@pytest.mark.parametrize(
    "invalid",
    [
        "",
        PRIVATE,
        '{"subqueries":["incomplete",',
        '{"subqueries":["alpha"," ALPHA "]}',
        '{"subqueries":["a","b","c","d","e","f"]}',
        '{"subqueries":[]}',
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


@pytest.mark.parametrize(("offset", "expected"), [(-1, 1), (0, 1), (5, 5), (1000, 8)])
def test_http_date_uses_injected_clock_and_cap(offset, expected):
    now = 1800000000
    date = format_datetime(
        datetime.fromtimestamp(now + offset, timezone.utc), usegmt=True
    )
    assert PlannerRetryPolicy(clock=lambda: now).delay(1, date) == expected


def test_default_evaluation_disables_sdk_retries_without_changing_chat(monkeypatch):
    calls = []

    def create(**kwargs):
        calls.append(kwargs)
        model = FakeModel()
        model.bind = lambda **_kwargs: model
        return model

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
