"""Original-query fusion uses fake providers and disposable durable stores only."""

import json
from dataclasses import replace
from itertools import permutations

import pytest

from src.orchestration_agents import (
    FinalizerInput,
    HandlerInput,
    IntakeInput,
    run_finalizer,
    run_handler,
    run_intake,
)
from src.orchestration_evidence import (
    EvidenceBudget,
    evidence_payload,
    prepare_evidence,
)
from src.orchestration_runtime import specialist_steps
from src.orchestration_state import (
    AgentKind,
    EventKind,
    EvidenceState,
    QuestionState,
    RunStatus,
    StateValidationError,
    ToolName,
    WorkStatus,
    decode,
    encode,
)
from src.orchestration_store import SQLiteOrchestrationStore
from src.orchestration_tools import (
    ORIGINAL_QUERY_CALL_ID,
    AuthorizationError,
    LimitError,
    Limits,
    OrchestrationTools,
    ToolError,
    ToolOutcome,
    _receipt_warning,
    bounded_json,
    final_evidence_ids,
    rank_evidence,
)
from test_orchestration_graph import Harness
from test_orchestration_specialists import (
    ARGS,
    Model,
    Retriever,
    bind,
    chunk,
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


def plan(questions=(), queries=()):
    return json.dumps({"questions": questions, "queries": queries})


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


def test_exact_original_precedes_optional_and_every_question_query(store):
    original = "  Ｃompare\tfindings?\nExact spacing!  "
    retriever = QueryRetriever(
        {
            q: [chunk(i, 0.9 - i / 10)]
            for i, q in enumerate(
                (
                    original,
                    "Paraphrase one",
                    "Paraphrase two",
                    "Question one",
                    "Question two",
                )
            )
        }
    )
    tools = bind(store, name="intake", retriever=retriever)
    model = Model(
        plan(queries=["Paraphrase one", "Paraphrase two"]),
        plan(["Question one", "Question two"]),
    )
    result = run_intake(model, tools, IntakeInput(original))
    assert payload(model)["retrieval_done"] is False
    assert payload(model)["evidence"][0]["chunk_index"] == 0
    assert payload(model, 1)["retrieval_done"] is True
    assert [e["chunk_index"] for e in payload(model, 1)["evidence"]] == [0, 1, 2]
    assert [e.kind for e in result.events] == ["context_retrieved"] * 3 + [
        "question_added"
    ] * 2
    settle_agent(store, "intake")
    for i, question in enumerate(tools.snapshot().questions):
        handler = bind(
            store,
            AgentKind.QUESTION,
            question.question_id,
            name=f"handler{i}",
            retriever=retriever,
        )
        run_handler(
            Model(response()), handler, HandlerInput(original, question.question_id)
        )
        settle_agent(store, f"handler{i}")
    assert retriever.calls == [(q, 5) for q in retriever.results]
    assert retriever.closed == 5
    assert original not in "".join(encode(e) for e in store.events(*ARGS))


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


@pytest.mark.parametrize("original", ["é" * 1024, " x\t" * 682 + " !"])
def test_exact_query_at_byte_bound_is_not_rewritten(store, original):
    retriever = Retriever([])
    tools = bind(store, retriever=retriever)
    run_intake(Model(plan(["Bounded question"])), tools, IntakeInput(original))
    assert retriever.calls == [(original, 5)]


@pytest.mark.parametrize(
    "original", ["é" * 1025, "x" * 2049, "\x00", "<system>unsafe</system>"]
)
def test_invalid_original_never_retrieves_or_commits(store, original):
    retriever = Retriever([])
    tools = bind(store, retriever=retriever)
    before = tools.snapshot()
    with pytest.raises((ToolError, StateValidationError)):
        run_intake(Model(), tools, IntakeInput(original))
    assert tools.snapshot() == before and retriever.calls == []


@pytest.mark.parametrize("kind", ["success", "empty", "failed"])
@pytest.mark.parametrize("lost_ack", [False, True])
def test_original_receipt_survives_lost_ack_restart_and_multiple_resume(
    store, monkeypatch, kind, lost_ack
):
    original = " Original\tquery "
    value = {
        "success": [chunk()],
        "empty": [],
        "failed": RuntimeError("PRIVATE_PROVIDER_DETAIL"),
    }[kind]
    retriever = QueryRetriever({original: value})
    tools = bind(store, retriever=retriever)
    commit = store.commit
    if lost_ack:

        def ambiguous(snapshot, **kwargs):
            result = commit(snapshot, **kwargs)
            if snapshot.tool_results:
                raise RuntimeError("lost acknowledgement")
            return result

        monkeypatch.setattr(store, "commit", ambiguous)
        with pytest.raises(RuntimeError, match="acknowledgement"):
            tools.retrieve_context(
                original, call_id=ORIGINAL_QUERY_CALL_ID, best_effort=True
            )
        monkeypatch.setattr(store, "commit", commit)
    first = tools.retrieve_context(
        original, call_id=ORIGINAL_QUERY_CALL_ID, best_effort=True
    )
    for _ in range(3):
        previous = tools
        tools = resume_tools(store, tools)
        with pytest.raises(AuthorizationError):
            previous.retrieve_context(
                original, call_id=ORIGINAL_QUERY_CALL_ID, best_effort=True
            )
        with SQLiteOrchestrationStore(store.database_path) as reopened:
            restored = OrchestrationTools(reopened, tools.scope, retriever)
            assert (
                restored.retrieve_context(
                    original, call_id=ORIGINAL_QUERY_CALL_ID, best_effort=True
                )
                == first
            )
            assert (
                restored.retrieval_warnings()
                == {
                    "success": (),
                    "empty": ("empty_evidence",),
                    "failed": ("retrieval_failed",),
                }[kind]
            )
    assert retriever.calls == [(original, 5)] and retriever.closed == 1
    assert len(tools.snapshot().tool_results) == 1
    assert first.warning == ("retrieval_failed" if kind == "failed" else None)
    events = store.events(*ARGS)
    event = next(e for e in events if e.tool_call_id == first.event.tool_call_id)
    assert (
        event.operation_id
        == ("tool:retrieval_failed:" if kind == "failed" else "")
        + first.event.tool_call_id
    )
    assert all("PRIVATE_PROVIDER_DETAIL" not in encode(e) for e in events)
    assert "PRIVATE_PROVIDER_DETAIL" not in encode(tools.snapshot())


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


def test_connected_duplicate_groups_preserve_best_score_and_citation_coverage():
    values = (
        EvidenceState("original", None, "paper", 0, "first text", 9, 7),
        EvidenceState("bridge", "q1", "paper", 0, " Ｓhared  snippet! ", 2, 3),
        EvidenceState("paraphrase", "q2", "other", 1, "shared snippet", 8, 4),
        EvidenceState("tie-b", None, "z", 1, "unique b", 6, 2),
        EvidenceState("tie-a", None, "a", 1, "unique a", 6, 1),
    )
    questions = tuple(
        QuestionState(
            q, q, status=WorkStatus.COMPLETED, answer="Answer", evidence_ids=(e,)
        )
        for q, e in (("q1", "bridge"), ("q2", "paraphrase"))
    )
    for order in permutations(values):
        assert rank_evidence(order) == (values[0], values[4], values[3])
        packet = prepare_evidence(order, answered_questions=questions)
        assert tuple(r.evidence for r in packet.records) == rank_evidence(order)
        assert packet.records[0].question_ids == ("q1", "q2")
        assert packet.records[0].evidence.page == 7
        assert all(not r.question_ids for r in packet.records[1:])


def test_cross_query_variants_are_retained_until_global_fusion(store):
    retriever = QueryRetriever(
        {
            "Original": [chunk(0, 0.9, "winner"), chunk(0, 0.1, "bridge")],
            "Question": [chunk(1, 0.8, " BRIDGE! ", paper="other")],
        }
    )
    tools = bind(store, name="intake", retriever=retriever)
    run_intake(Model(plan(["Question"])), tools, IntakeInput("Original"))
    assert len(tools.snapshot().evidence) == 2
    settle_agent(store, "intake")
    question = tools.snapshot().questions[0]
    handler = bind(
        store,
        AgentKind.QUESTION,
        question.question_id,
        name="handler",
        retriever=retriever,
    )

    def cite(messages):
        evidence = json.loads(messages[1].content)["evidence"]
        assert len(evidence) == 1 and evidence[0]["snippet"] == "winner"
        assert evidence[0]["question_id"] is None
        return response([evidence[0]["evidence_id"]])

    run_handler(Model(cite), handler, HandlerInput("Original", question.question_id))
    snapshot = handler.snapshot()
    assert decode(encode(snapshot)) == snapshot
    assert len(snapshot.evidence) == 3
    settle_agent(store, "handler")
    finalizer = bind(store, AgentKind.FINALIZER, name="finalizer")
    model = Model('{"assistant_text":"Final only"}')
    run_finalizer(model, finalizer, FinalizerInput("Original"))
    evidence = payload(model)["evidence"]
    assert len(evidence) == 1 and evidence[0]["question_ids"] == [question.question_id]


def test_final_pool_includes_original_and_uncited_completed_but_not_failed(store):
    intake = bind(
        store,
        name="intake",
        retriever=Retriever([chunk(9, 0.95, "Original-only support")]),
    )
    run_intake(
        Model(plan(["First", "Second", "Failed"])), intake, IntakeInput("Original")
    )
    settle_agent(store, "intake")
    questions = intake.snapshot().questions
    for i, question in enumerate(questions):
        tools = bind(
            store,
            AgentKind.QUESTION,
            question.question_id,
            name=f"handler{i}",
            retriever=Retriever([chunk(i, 0.8 + i / 100)]),
        )
        if i == 2:
            tools.retrieve_context(question.text, call_id="failed-question-retrieval")
            snapshot = tools.snapshot()
            running = store.commit(
                replace(
                    snapshot,
                    questions=tuple(
                        replace(q, status=WorkStatus.RUNNING) if q == question else q
                        for q in snapshot.questions
                    ),
                ),
                operation_id="running-failed-question",
            )
            store.commit(
                replace(
                    running,
                    questions=tuple(
                        replace(q, status=WorkStatus.FAILED)
                        if q.question_id == question.question_id
                        else q
                        for q in running.questions
                    ),
                ),
                operation_id="failed-question",
            )
            settle_agent(store, f"handler{i}", WorkStatus.FAILED)
        else:

            def cite_original(messages):
                evidence = json.loads(messages[1].content)["evidence"]
                assert len(evidence) == 2
                return response([evidence[0]["evidence_id"]])

            run_handler(
                Model(cite_original),
                tools,
                HandlerInput("Original", question.question_id),
            )
            settle_agent(store, f"handler{i}")
    finalizer = bind(store, AgentKind.FINALIZER, name="finalizer")
    model = Model('{"assistant_text":"Best effort final answer"}')
    result = run_finalizer(model, finalizer, FinalizerInput("Original"))
    evidence = payload(model)["evidence"]
    assert [e["chunk_index"] for e in evidence] == [9, 1, 0]
    assert evidence[0]["question_ids"] == sorted(q.question_id for q in questions[:2])
    assert evidence[1]["question_ids"] == evidence[2]["question_ids"] == []
    assert set(result.evidence_ids) == final_evidence_ids(finalizer.snapshot())
    assert any("failed" in warning for warning in result.warnings)


def test_coverage_reservation_beats_uncited_high_scores_with_all_128_records():
    values = tuple(
        EvidenceState(f"e{i}", None, "paper", i, f"Evidence {i} " + "é" * 490, 128 - i)
        for i in range(128)
    )
    questions = tuple(
        QuestionState(
            f"q{i}",
            f"Question {i}",
            status=WorkStatus.COMPLETED,
            answer="Answer",
            evidence_ids=(f"e{120 + i}",),
        )
        for i in range(8)
    )
    budget = EvidenceBudget(
        final_bytes=8192, compression_input_bytes=8192, summary_bytes=256, max_records=8
    )
    packet = prepare_evidence(values, budget, answered_questions=questions)
    assert packet.compression_required and len(packet.records) == 8
    assert {r.evidence.evidence_id for r in packet.records} == {
        f"e{i}" for i in range(120, 128)
    }
    assert {q for r in packet.records for q in r.question_ids} == {
        q.question_id for q in questions
    }
    assert (
        len(bounded_json({"evidence": evidence_payload(packet.records)}).encode())
        + 1024
        <= budget.compression_input_bytes
    )
    with pytest.raises(ToolError):
        prepare_evidence(values + (values[0],))


@pytest.mark.parametrize("scores", list(permutations((0.3, 0.6, 0.9))))
def test_original_and_two_paraphrase_score_matrix(store, scores):
    queries = ("Original", "Paraphrase one", "Paraphrase two")
    retriever = QueryRetriever(
        {
            query: [
                chunk(0, score, f"Variant {index}"),
                chunk(index + 1, 0.1, f"Unique {index}"),
            ]
            for index, (query, score) in enumerate(zip(queries, scores))
        }
    )
    tools = bind(store, name="intake", retriever=retriever)
    model = Model(plan(queries=queries[1:]), plan(["Question"]))
    run_intake(model, tools, IntakeInput(queries[0]))
    evidence = payload(model, 1)["evidence"]
    assert len(evidence) == 4
    assert evidence[0]["score"] == 0.9
    assert evidence[0]["snippet"] == f"Variant {scores.index(0.9)}"
    assert [e["chunk_index"] for e in evidence] == [0, 1, 2, 3]
    assert retriever.calls == [(q, 5) for q in queries]


def test_later_question_winner_carries_prior_intake_citation_coverage(store):
    retriever = QueryRetriever(
        {
            "Original": [chunk(0, 0.3, "Original evidence")],
            "First": [],
            "Second": [chunk(0, 0.9, "Higher relevance")],
        }
    )
    intake = bind(store, name="intake", retriever=retriever)
    run_intake(Model(plan(["First", "Second"])), intake, IntakeInput("Original"))
    settle_agent(store, "intake")
    for index, question in enumerate(intake.snapshot().questions):
        handler = bind(
            store,
            AgentKind.QUESTION,
            question.question_id,
            name=f"handler{index}",
            retriever=retriever,
        )

        def cite(messages):
            return response(
                [json.loads(messages[1].content)["evidence"][0]["evidence_id"]]
            )

        run_handler(
            Model(cite), handler, HandlerInput("Original", question.question_id)
        )
        settle_agent(store, f"handler{index}")
    finalizer = bind(store, AgentKind.FINALIZER, name="finalizer")
    model = Model('{"assistant_text":"Final supported answer"}')
    run_finalizer(model, finalizer, FinalizerInput("Original"))
    evidence = payload(model)["evidence"]
    assert len(evidence) == 1 and evidence[0]["score"] == 0.9
    assert set(evidence[0]["question_ids"]) == {
        q.question_id for q in intake.snapshot().questions
    }


def test_intake_authorization_does_not_allow_other_question_or_forged_citations(store):
    intake = bind(store, name="intake")
    run_intake(Model(plan(["First", "Second"])), intake, IntakeInput("Original"))
    settle_agent(store, "intake")
    first, second = intake.snapshot().questions
    other = bind(store, AgentKind.QUESTION, first.question_id, name="other")
    retrieved = other.retrieve_context(first.text, call_id="retrieve")
    handler = bind(store, AgentKind.QUESTION, second.question_id, name="handler")
    before = handler.snapshot()
    with pytest.raises(AuthorizationError):
        handler.answer_question(
            second.question_id, "Answer", retrieved.event.evidence_ids, call_id="forged"
        )
    original_id = before.evidence[0].evidence_id
    with pytest.raises(StateValidationError, match="authorized answer receipt"):
        replace(
            before,
            questions=(
                first,
                replace(
                    second,
                    status=WorkStatus.COMPLETED,
                    answer="Answer",
                    evidence_ids=(original_id,),
                ),
            ),
        )
    assert handler.snapshot() == before


def test_raw_durable_records_never_exceed_128_and_replay_works_at_capacity(store):
    retriever = Retriever()
    tools = bind(store, retriever=retriever)
    original = tools.retrieve_context("Original", call_id=ORIGINAL_QUERY_CALL_ID)
    for index in range(26):
        if index and index % 2 == 0:
            tools = resume_tools(store, tools)
        size = 5 if index < 25 else 2
        retriever.chunks = [chunk(index * 5 + j + 1) for j in range(size)]
        tools.retrieve_context(f"Query {index}", call_id=f"retrieve-{index}")
    assert len(tools.snapshot().evidence) == 128
    before = tools.snapshot()
    tools = resume_tools(store, tools)
    at_capacity = tools.snapshot()
    retriever.chunks = [chunk(999)]
    with pytest.raises(LimitError, match="Evidence limit"):
        tools.retrieve_context("Overflow", call_id="overflow", best_effort=True)
    assert tools.snapshot() == at_capacity
    assert (
        tools.retrieve_context("Original", call_id=ORIGINAL_QUERY_CALL_ID) == original
    )
    assert tools.snapshot().evidence == before.evidence
    assert all(n == 5 for _, n in retriever.calls)


@pytest.mark.parametrize(
    "failed_path", ["Original", "Optional", "Question", "all", "empty"]
)
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
            for i, q in enumerate(("Explain the result", "Optional", "Question"))
        }
    )
    if failed_path == "Original":
        h.retriever.results["Explain the result"] = RuntimeError(
            "PRIVATE_PROVIDER_DETAIL"
        )
    models = []

    def work(dispatch):
        if dispatch.kind is AgentKind.INTAKE:
            model = Model(plan(queries=["Optional"]), plan(["Question"]))
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
        assert h.retriever.closed == 3
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
