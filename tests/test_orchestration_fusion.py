"""Original-query fusion uses fake providers and disposable durable stores only."""

import json
from dataclasses import replace

import pytest

from src.orchestration.agents import (
    IntakeInput,
    run_intake,
)
from src.orchestration.runtime import specialist_steps
from src.orchestration.state import (
    AgentKind,
    EventKind,
    RunStatus,
    ToolName,
    WorkStatus,
    encode,
)
from src.orchestration.store import SQLiteOrchestrationStore
from src.orchestration.tools import (
    ORIGINAL_QUERY_CALL_ID,
    AuthorizationError,
    LimitError,
    Limits,
    OrchestrationTools,
    ToolError,
    ToolOutcome,
    _receipt_warning,
)
from test_orchestration_graph import Harness
from test_orchestration_specialists import (
    ARGS,
    Model,
    Retriever,
    bind,
    chunk,
    intake_plan,
    settle_agent,
)


@pytest.fixture
def store(tmp_path):
    with SQLiteOrchestrationStore(tmp_path / "fusion.sqlite3") as value:
        value.create_run(ARGS[1], ARGS[2], run_id=ARGS[0])
        yield value


class QueryRetriever:
    def __init__(self, results):
        self.results, self.calls, self.closed = results, [], 0

    def retrieve_chunks(self, query, n_results):
        self.calls.append((query, n_results))
        try:
            value = self.results[query]
            if isinstance(value, Exception):
                raise value
            yield from value
        finally:
            self.closed += 1


def plan(questions):
    return intake_plan(*[(f"Part {i}", q) for i, q in enumerate(questions)])


def response(ids=()):
    return json.dumps(
        {
            "answer": "Supported intermediate answer",
            "evidence_ids": ids,
            "followups": [],
        }
    )


def payload(model, index=0):
    return json.loads(model.messages[index][1].content)


def resume_tools(store, tools):
    store.interrupt(*ARGS, operation_id=f"interrupt-{tools.scope.attempt}")
    snapshot = store.resume(*ARGS, operation_id=f"resume-{tools.scope.attempt}")
    agent = next(a for a in snapshot.agents if a.agent_id == tools.scope.agent_id)
    store.commit(
        replace(
            snapshot,
            agents=tuple(
                replace(a, status=WorkStatus.RUNNING)
                if a.agent_id == agent.agent_id
                else a
                for a in snapshot.agents
            ),
        ),
        operation_id=f"restart-{agent.attempt}",
    )
    return OrchestrationTools(
        store,
        replace(tools.scope, attempt=agent.attempt),
        tools._retriever,
        limits=tools.limits,
    )


@pytest.mark.parametrize("queries", [1, 2])
def test_original_has_one_extra_allowance_and_cannot_be_used_by_handler(store, queries):
    retriever = Retriever()
    tools = bind(
        store,
        name="intake",
        retriever=retriever,
        limits=Limits(queries_per_attempt=queries),
    )
    original = tools.retrieve_context("Original", call_id=ORIGINAL_QUERY_CALL_ID)
    for i in range(queries):
        tools.retrieve_context(f"Optional {i}", call_id=f"optional-{i}")
    with pytest.raises(LimitError):
        tools.retrieve_context("Overflow", call_id="overflow")
    assert (
        tools.retrieve_context("Original", call_id=ORIGINAL_QUERY_CALL_ID) == original
    )
    with pytest.raises(ToolError, match="conflicts"):
        tools.retrieve_context(" Original ", call_id=ORIGINAL_QUERY_CALL_ID)
    question = tools.add_question("Question", call_id="question").question
    settle_agent(store, "intake")
    handler = bind(
        store,
        AgentKind.QUESTION,
        question.question_id,
        name="handler",
        retriever=retriever,
    )
    with pytest.raises(AuthorizationError):
        handler.retrieve_context("Original", call_id=ORIGINAL_QUERY_CALL_ID)
    assert len(retriever.calls) == queries + 1


@pytest.mark.parametrize("resume", [False, True])
@pytest.mark.parametrize("phase", ["before", "after"])
@pytest.mark.parametrize("target", [0, 1])
def test_intake_partial_plan_commit_recovers_one_receipt_per_part(
    store, monkeypatch, resume, phase, target
):
    retriever = Retriever()
    tools = bind(store, retriever=retriever)
    original = "Compare A and B on cost and latency"
    questions = [
        "How do A and B compare on cost?",
        "How do A and B compare on latency?",
    ]
    model = Model(plan(questions))
    commit = store.commit
    seen = 0

    def interrupted(snapshot, **kwargs):
        nonlocal seen
        added = snapshot.tool_results[len(tools.snapshot().tool_results) :]
        hit = bool(
            added and added[0].tool_name is ToolName.ADD_QUESTION and seen == target
        )
        if added and added[0].tool_name is ToolName.ADD_QUESTION:
            seen += 1
        if hit and phase == "before":
            raise RuntimeError("interrupted proposal")
        result = commit(snapshot, **kwargs)
        if hit and phase == "after":
            raise RuntimeError("lost proposal acknowledgement")
        return result

    monkeypatch.setattr(store, "commit", interrupted)
    with pytest.raises(RuntimeError):
        run_intake(model, tools, IntakeInput(original))
    assert len(model.messages) == 1
    partial = tools.snapshot()
    assert len(partial.questions) == target + (phase == "after")
    monkeypatch.setattr(store, "commit", commit)
    if resume:
        tools = resume_tools(store, tools)
    with SQLiteOrchestrationStore(store.database_path) as reopened:
        restored = OrchestrationTools(reopened, tools.scope, retriever)
        replay_model = Model(
            intake_plan(*[(f"Renamed label {i}", q) for i, q in enumerate(questions)])
        )
        result = run_intake(replay_model, restored, IntakeInput(original))
        assert len(replay_model.messages) == 1
        final = restored.snapshot()
        assert [q.text for q in final.questions] == questions
        assert final.questions[: len(partial.questions)] == partial.questions
        receipts = [
            r for r in final.tool_results if r.tool_name is ToolName.ADD_QUESTION
        ]
        assert [r.question_id for r in receipts] == [
            q.question_id for q in final.questions
        ]
        assert len(final.tool_results) == 3
        assert result.warnings == (
            ("duplicate_question",) if resume and partial.questions else ()
        )
    assert retriever.calls == [(original, 5)]


def test_failed_receipt_decoding_requires_exact_event_identity(store):
    tools = bind(store, retriever=QueryRetriever({"Original": RuntimeError("private")}))
    outcome = tools.retrieve_context(
        "Original", call_id=ORIGINAL_QUERY_CALL_ID, best_effort=True
    )
    receipt = tools.snapshot().tool_results[-1]
    event = store.events(*ARGS)[-1]
    assert _receipt_warning(receipt, event) == "retrieval_failed"
    for changed in (
        replace(event, kind=EventKind.TRANSITION),
        replace(event, tool_call_id="unrelated"),
        replace(event, operation_id="tool:retrieval_failed:unrelated"),
        replace(event, operation_id=event.operation_id + "x"),
        replace(event, operation_id="prefix:" + receipt.tool_call_id),
    ):
        with pytest.raises(ToolError, match="outcome event"):
            _receipt_warning(receipt, changed)
    with pytest.raises(ToolError):
        _receipt_warning(replace(receipt, tool_name=ToolName.ADD_QUESTION), event)
    with pytest.raises(ToolError):
        _receipt_warning(replace(receipt, evidence_ids=("invented",)), event)
    with pytest.raises(ToolError):
        ToolOutcome(outcome.event, warning="untrusted")
    with pytest.raises(ToolError, match="retrieval failed"):
        tools.retrieve_context("Original", call_id=ORIGINAL_QUERY_CALL_ID)
    assert len(tools._retriever.calls) == 1


@pytest.mark.parametrize("failed_path", ["Original", "Question", "all", "empty"])
def test_failed_or_empty_query_paths_warn_but_complete_finalizer_only(
    tmp_path, failed_path
):
    h = Harness(tmp_path, count=0)
    h.retriever = QueryRetriever(
        {
            q: RuntimeError("PRIVATE_PROVIDER_DETAIL")
            if failed_path in (q, "all")
            else []
            if failed_path == "empty"
            else [chunk(i, 0.9 - i / 10)]
            for i, q in enumerate(("Explain the result", "Question"))
        }
    )
    if failed_path == "Original":
        h.retriever.results["Explain the result"] = RuntimeError(
            "PRIVATE_PROVIDER_DETAIL"
        )
    models = []

    def work(dispatch):
        if dispatch.kind is AgentKind.INTAKE:
            model = Model(plan(["Question"]))
        elif dispatch.kind is AgentKind.QUESTION:
            model = Model(response())
        else:
            model = Model(
                '{"assistant_text":"Final answer acknowledges missing evidence."}'
            )
        models.append((dispatch.kind, model))
        yield from specialist_steps(dispatch, model)

    h.executor.factory = work
    h.runtime = h.new_runtime()
    try:
        final = h.done()
        assert final.run.status is RunStatus.COMPLETED
        assert h.retriever.calls == [(q, 5) for q in h.retriever.results]
        assert h.retriever.closed == 2
        expected = "empty" if failed_path == "empty" else "failed"
        final_model = next(
            model for kind, model in models if kind is AgentKind.FINALIZER
        )
        assert any(
            expected in w or (expected == "empty" and "found no" in w)
            for w in payload(final_model)["warnings"]
        )
        assert (
            "".join(e.text for e in h.sink.of_kind("delta"))
            == "Final answer acknowledges missing evidence."
        )
        assert len(h.sink.of_kind("done")) == 1 and h.publisher.writes == 1
        assert all(
            "PRIVATE_PROVIDER_DETAIL" not in encode(e)
            for e in h.store.events(*h.runtime.args)
        )
        assert h.sink.of_kind("warning")
    finally:
        h.close()
