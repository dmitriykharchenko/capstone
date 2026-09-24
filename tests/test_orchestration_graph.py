"""Disposable stores, deterministic cooperative executors, no network/providers."""

import json
import sqlite3
from dataclasses import asdict
from itertools import permutations

import pytest

from src.orchestration_agents import FinalizerResult, SpecialistResult, run_finalizer
from src.orchestration_graph import (
    AgentCompleted,
    QuestionAdded,
    QuestionAnswered,
    Reconcile,
    agent_completed,
    question_added,
    question_answered,
    reconcile,
    transition,
)
from src.orchestration_runtime import (
    Completed,
    Delta,
    Discarded,
    FinalAnswer,
    IteratorHandle,
    OrchestrationRuntime,
    Published,
    specialist_steps,
)
from src.orchestration_state import (
    TERMINAL_RUNS,
    AgentKind,
    EventKind,
    RunStatus,
    ToolName,
    WorkStatus,
    encode,
)
from src.orchestration_store import SQLiteOrchestrationStore, TransitionConflictError
from src.orchestration_tools import (
    AuthorizationError,
    DuplicateQuestion,
    LimitError,
    Limits,
)

ARGS = ("run", "session", "graph-v3")


class Publisher:
    """A durable test-only port, never the real canonical history schema."""

    def __init__(self, path):
        self.path = path
        self.fail = None
        self.index_warning = False
        self.writes = 0
        with self.connection() as db:
            db.execute(
                "CREATE TABLE IF NOT EXISTS replies (key TEXT PRIMARY KEY, payload TEXT)"
            )

    def connection(self):
        from contextlib import contextmanager

        @contextmanager
        def connection():
            db = sqlite3.connect(self.path)
            try:
                with db:
                    yield db
            finally:
                db.close()

        return connection()

    def lookup(self, key):
        with self.connection() as db:
            row = db.execute(
                "SELECT payload FROM replies WHERE key=?", (key,)
            ).fetchone()
        if not row:
            return None
        if row[0] is None:
            return Discarded()
        data = json.loads(row[0])
        data["answer"]["evidence_ids"] = tuple(data["answer"]["evidence_ids"])
        return Published(
            data["exchange_id"], FinalAnswer(**data["answer"]), data["index_warning"]
        )

    def publish(self, key, answer):
        if self.fail == "before":
            raise RuntimeError("private failure details")
        with self.connection() as db:
            db.execute("BEGIN IMMEDIATE")
            row = db.execute(
                "SELECT payload FROM replies WHERE key=?", (key,)
            ).fetchone()
            if row and row[0] is None:
                raise RuntimeError("Publication discarded")
            if row is None:
                self.writes += 1
                saved = Published(1, answer, self.index_warning)
                db.execute(
                    "INSERT INTO replies VALUES (?,?)", (key, json.dumps(asdict(saved)))
                )
        if self.fail == "after":
            raise RuntimeError("Acknowledgement lost")
        return self.lookup(key)

    def discard(self, key):
        with self.connection() as db:
            db.execute("INSERT OR IGNORE INTO replies VALUES (?,NULL)", (key,))
        return self.lookup(key)


class Sink:
    def __init__(self):
        self.events = {}
        self.text = ""
        self.fail = None

    def emit(self, event):
        if self.fail == event.kind:
            raise RuntimeError("private sink failure")
        key = (event.run_id, event.sequence, event.kind)
        if key in self.events:
            assert self.events[key] == event
            return
        self.events[key] = event
        if event.kind == "reset":
            self.text = ""
        if event.kind == "delta":
            self.text += event.text

    def of_kind(self, kind):
        return [e for e in self.events.values() if e.kind == kind]


class Retriever:
    def retrieve_chunks(self, query, n_results=5):
        return []


class Model:
    def __init__(self, response):
        self.response = response
        self.messages = []

    def invoke(self, messages):
        self.messages.append(messages)
        return json.dumps(self.response)


class Handle(IteratorHandle):
    def __init__(self, iterator):
        super().__init__(iterator)
        self.closed = 0

    def close(self):
        self.closed += 1
        assert self.closed == 1
        super().close()


class Executor:
    def __init__(self, factory):
        self.factory = factory
        self.calls = {}
        self.handles = []
        self.fail_submit = False

    def submit(self, operation_id, dispatch):
        assert operation_id not in self.calls
        self.calls[operation_id] = dispatch
        if self.fail_submit:
            raise RuntimeError("private executor failure")
        handle = Handle(self.factory(dispatch))
        self.handles.append(handle)
        return handle


class Harness:
    def __init__(self, tmp_path, *, count=2, limits=Limits(), followups=False):
        self.store = SQLiteOrchestrationStore(tmp_path / "orchestration.sqlite3")
        self.store.create_run(ARGS[1], ARGS[2], run_id=ARGS[0])
        self.publisher = Publisher(tmp_path / "publication.sqlite3")
        self.sink = Sink()
        self.retriever = Retriever()
        self.time = 0.0
        self.count, self.followups, self.limits = count, followups, limits
        self.failures = set()
        self.hold = set()
        self.final_text = "A grounded final reply."
        self.final_models = []
        self.finished = []
        self.executor = Executor(self.work)
        self.runtime = self.new_runtime()

    def new_runtime(self):
        return OrchestrationRuntime(
            self.store,
            *ARGS,
            "Explain the result",
            executor=self.executor,
            retriever=self.retriever,
            publisher=self.publisher,
            sink=self.sink,
            clock=lambda: self.time,
            timeout=10,
            limits=self.limits,
        )

    def question(self, dispatch):
        return next(
            q
            for q in self.runtime.load().questions
            if q.question_id == dispatch.request.question_id
        )

    def work(self, dispatch):
        tools = dispatch.tools
        if dispatch.kind is AgentKind.INTAKE:
            for index in range(self.count):
                try:
                    tools.add_question(f"Question {index}", call_id=f"add-{index}")
                except (DuplicateQuestion, LimitError):
                    pass
                yield None
            yield Completed(SpecialistResult(()))
        elif dispatch.kind is AgentKind.QUESTION:
            question = self.question(dispatch)
            while question.text in self.hold:
                yield None
            if (question.text, dispatch.scope.attempt) in self.failures:
                raise RuntimeError("private provider exception")
            tools.retrieve_context(question.text, call_id="retrieve")
            yield None
            if self.followups:
                try:
                    tools.add_question(
                        f"Child of {question.text}",
                        parent_id=question.question_id,
                        call_id="child",
                    )
                except (DuplicateQuestion, LimitError):
                    pass
                yield None
            tools.answer_question(
                question.question_id,
                "A concise supported answer.",
                (),
                call_id="answer",
            )
            yield None
            self.finished.append(question.text)
            yield Completed(SpecialistResult(()))
        else:
            model = Model({"assistant_text": self.final_text})
            self.final_models.append(model)
            result = run_finalizer(model, tools, dispatch.request)
            for part in (self.final_text[:5], self.final_text[5:]):
                yield Delta(part)
            yield Completed(result)

    def until(self, predicate, maximum=500):
        for _ in range(maximum):
            snapshot = self.runtime.load()
            if predicate(snapshot):
                return snapshot
            self.runtime.step()
            snapshot = self.runtime.load()
            assert len(snapshot.questions) <= 8
            assert sum(a.status is WorkStatus.RUNNING for a in snapshot.agents) <= 4
            assert max((q.depth for q in snapshot.questions), default=0) <= 2
            for q in snapshot.questions:
                assert sum(a.question_id == q.question_id for a in snapshot.agents) <= 1
        pytest.fail("Runtime did not settle within the deterministic step budget")

    def done(self):
        return self.until(lambda s: s.run.status in TERMINAL_RUNS)

    def restart(self, key="resume"):
        self.runtime.close()
        self.runtime = self.new_runtime()
        self.runtime.resume(key)

    def close(self):
        self.runtime.close()
        assert not self.runtime.active
        assert all(h.closed == 1 for h in self.executor.handles)
        self.store.close()


@pytest.fixture
def harness(tmp_path):
    items = []

    def make(**kwargs):
        directory = tmp_path / str(len(items))
        directory.mkdir()
        result = Harness(directory, **kwargs)
        items.append(result)
        return result

    yield make
    for item in items:
        item.close()


def test_complete_fanout_is_durable_and_only_finalizer_streams(harness):
    h = harness(count=8)
    final = h.done()
    assert final.run.status is RunStatus.COMPLETED
    assert final.run.exchange_id == 1
    assert len(final.questions) == 8
    assert len(h.sink.of_kind("done")) == 1
    assert h.sink.text == h.final_text
    assert h.publisher.writes == 1
    events = h.store.events(*ARGS, limit=4096)
    assert [e.sequence for e in events] == list(range(1, final.sequence + 1))
    assert events[-2].kind is EventKind.EXCHANGE_ATTACHED
    assert events[-1].kind is EventKind.COMPLETED
    for agent in final.agents:
        assert (
            sum(d.scope.agent_id == agent.agent_id for d in h.executor.calls.values())
            == 1
        )
    for _ in range(4):
        h.runtime.step()
    assert h.runtime.load() == final
    assert len(h.sink.of_kind("done")) == 1


def test_pure_handlers_use_committed_state_and_ignore_duplicates(harness):
    h = harness(count=1)
    h.until(lambda s: len(s.questions) == 1)
    snapshot = h.runtime.load()
    q = snapshot.questions[0]
    assert q.status is WorkStatus.QUEUED
    proposed = question_added(snapshot, QuestionAdded(q.question_id))
    assert len(proposed.agents) == len(snapshot.agents) + 1
    assert h.runtime.load() == snapshot
    assert question_added(proposed, QuestionAdded(q.question_id)) == proposed
    assert question_added(snapshot, QuestionAdded("unknown")) == snapshot
    assert question_answered(snapshot, QuestionAnswered(q.question_id)) == snapshot
    for event, function in (
        (QuestionAdded(q.question_id), question_added),
        (QuestionAnswered(q.question_id), question_answered),
        (AgentCompleted("missing", 0), agent_completed),
    ):
        assert transition(snapshot, event, Limits()) == function(snapshot, event)
    assert transition(snapshot, Reconcile(), Limits()) == reconcile(snapshot, Limits())


def test_finalizer_waits_for_completion_ack_even_after_answer(harness):
    h = harness(count=1)
    h.until(lambda s: s.questions and s.questions[0].status is WorkStatus.COMPLETED)
    snapshot = h.runtime.load()
    handler = next(a for a in snapshot.agents if a.kind is AgentKind.QUESTION)
    assert handler.status is WorkStatus.RUNNING
    for _ in range(3):
        assert (
            h.runtime.notify(QuestionAnswered(snapshot.questions[0].question_id))
            == snapshot
        )
    assert not any(a.kind is AgentKind.FINALIZER for a in snapshot.agents)
    h.done()


@pytest.mark.parametrize("order", list(permutations(range(4))))
def test_completion_permutations_and_concurrency_admission(harness, order):
    h = harness(count=4)
    h.hold = {f"Question {i}" for i in range(4)}
    h.until(
        lambda s: (
            len(h.runtime.active) == 4
            and all(
                a.kind is AgentKind.QUESTION
                for a in s.agents
                if a.status is WorkStatus.RUNNING
            )
        )
    )
    for index in order:
        h.hold.remove(f"Question {index}")
        h.until(lambda s: f"Question {index}" in h.finished)
    final = h.done()
    assert final.run.status is RunStatus.COMPLETED
    assert h.finished == [f"Question {i}" for i in order]
    assert h.publisher.writes == 1


@pytest.mark.parametrize("concurrency", [1, 2, 3, 4])
def test_lower_concurrency_and_nested_followups(harness, concurrency):
    h = harness(count=3, followups=True, limits=Limits(concurrency=concurrency))
    final = h.done()
    assert len(final.questions) == 8
    assert max(q.depth for q in final.questions) == 2
    for event in h.store.events(*ARGS, limit=4096):
        snapshot = h.store.load(*ARGS, sequence=event.sequence)
        assert (
            sum(a.status is WorkStatus.RUNNING for a in snapshot.agents) <= concurrency
        )


@pytest.mark.parametrize("failed_attempts", [1, 2])
def test_retry_once_then_best_effort_with_sanitized_warning(harness, failed_attempts):
    h = harness(count=2)
    h.failures = {("Question 0", i) for i in range(failed_attempts)}
    final = h.done()
    failed = next(q for q in final.questions if q.text == "Question 0")
    assert failed.status is (
        WorkStatus.FAILED if failed_attempts == 2 else WorkStatus.COMPLETED
    )
    agent = next(a for a in final.agents if a.question_id == failed.question_id)
    assert agent.retry_count == 1
    assert agent.attempt == 1
    assert final.run.status is RunStatus.COMPLETED
    assert h.sink.of_kind("warning")
    assert "private" not in str(h.sink.events)
    assert "private" not in encode(final)


def test_all_failed_questions_reach_finalizer_with_explicit_warning(harness):
    h = harness(count=3)
    h.failures = {(f"Question {q}", i) for q in range(3) for i in range(2)}
    final = h.done()
    assert all(q.status is WorkStatus.FAILED for q in final.questions)
    payload = json.loads(h.final_models[0].messages[0][1].content)
    assert any("failed" in w for w in payload["warnings"])
    assert final.run.status is RunStatus.COMPLETED


def test_timeouts_close_handles_and_obey_retry_budget(harness):
    h = harness(count=1)
    h.hold.add("Question 0")
    h.until(
        lambda s: any(
            a.kind is AgentKind.INTAKE and a.status is WorkStatus.COMPLETED
            for a in s.agents
        )
    )
    for attempt in range(2):
        h.until(
            lambda s: any(
                a.kind is AgentKind.QUESTION
                and a.attempt == attempt
                and a.status is WorkStatus.RUNNING
                for a in s.agents
            )
        )
        h.time += 11
        h.until(
            lambda s: any(
                a.kind is AgentKind.QUESTION
                and a.attempt == attempt
                and a.status is WorkStatus.FAILED
                for a in s.agents
            )
        )
    final = h.done()
    assert final.questions[0].status is WorkStatus.FAILED
    assert all(handle.closed == 1 for handle in h.executor.handles)


@pytest.mark.parametrize("boundary", range(43))
def test_restart_after_every_caller_step(harness, boundary):
    h = harness(count=1, followups=True)
    for _ in range(boundary):
        if h.runtime.load().run.status in TERMINAL_RUNS:
            break
        h.runtime.step()
    old = h.runtime.load()
    h.restart()
    final = h.done()
    assert final.run.status is RunStatus.COMPLETED
    assert len(final.questions) == 3
    assert h.publisher.writes == 1
    assert len(h.sink.of_kind("done")) == 1
    assert all(q.status is WorkStatus.COMPLETED for q in final.questions)
    for a in old.agents:
        if a.status is WorkStatus.RUNNING:
            h.runtime.notify(AgentCompleted(a.agent_id, a.attempt))
    assert h.runtime.load() == final


def test_final_stream_restart_replaces_uncommitted_text(harness):
    h = harness(count=0)
    h.until(lambda s: len(h.sink.of_kind("delta")) == 1)
    assert h.sink.text == h.final_text[:5]
    h.restart()
    h.final_text = "A fresh complete answer."
    h.done()
    assert len(h.sink.of_kind("reset")) == 2
    assert h.sink.text == h.final_text
    assert h.publisher.lookup(h.runtime.key).answer.assistant_text == h.final_text
    assert all(handle.closed == 1 for handle in h.executor.handles)


@pytest.mark.parametrize("where", ["before", "after"])
def test_publication_failure_and_lost_acknowledgement(harness, where):
    h = harness(count=0)
    h.publisher.fail = where
    h.until(lambda s: s.run.status is RunStatus.INTERRUPTED)
    assert not h.sink.of_kind("done")
    prior_finalizers = len(h.final_models)
    path = h.publisher.path
    h.publisher = Publisher(path)
    h.restart()
    final = h.done()
    assert final.run.exchange_id == 1
    assert len(h.final_models) == prior_finalizers + (where == "before")
    assert len(h.sink.of_kind("done")) == 1
    saved = h.publisher.publish(
        h.runtime.key, FinalAnswer("Different uncommitted text")
    )
    assert saved.answer.assistant_text == h.final_text


def test_derived_index_warning_preserves_saved_reply(harness):
    h = harness(count=0)
    h.publisher.index_warning = True
    h.done()
    assert any("search index" in e.text for e in h.sink.of_kind("warning"))
    assert h.publisher.lookup(h.runtime.key).answer.assistant_text == h.final_text
    assert h.sink.of_kind("done")[0].exchange_id == 1


def test_discard_fences_active_tools_results_and_publication(harness):
    h = harness(count=2)
    h.until(lambda s: bool(h.runtime.active))
    dispatch = next(iter(h.executor.calls.values()))
    discarded = h.runtime.discard()
    assert discarded.run.status is RunStatus.DISCARDED
    assert h.runtime.discard() == discarded
    assert (
        h.runtime.notify(
            AgentCompleted(dispatch.scope.agent_id, dispatch.scope.attempt)
        )
        == discarded
    )
    with pytest.raises(AuthorizationError):
        dispatch.tools.add_question("Late question", call_id="late")
    with pytest.raises(RuntimeError, match="discarded"):
        h.publisher.publish(h.runtime.key, FinalAnswer("Late reply"))
    h.runtime.step()
    h.runtime.step()
    assert len(h.sink.of_kind("discarded")) == 1
    assert h.publisher.writes == 0


def test_discard_after_lost_publication_ack_retains_exchange(harness):
    h = harness(count=0)
    h.publisher.fail = "after"
    h.until(lambda s: s.run.status is RunStatus.INTERRUPTED)
    result = h.runtime.discard()
    assert result.run.status is RunStatus.COMPLETED
    assert result.run.exchange_id == 1
    h.runtime.step()
    assert len(h.sink.of_kind("done")) == 1
    assert not h.sink.of_kind("discarded")


@pytest.mark.parametrize("kind", ["reset", "delta", "done"])
def test_sink_failure_closes_stream_and_reconnect_is_idempotent(harness, kind):
    h = harness(count=0)
    h.sink.fail = kind
    with pytest.raises(RuntimeError, match="resume explicitly"):
        h.done()
    assert not h.runtime.active
    h.sink.fail = None
    h.restart()
    h.done()
    h.runtime.step()
    assert h.publisher.writes == 1
    assert len(h.sink.of_kind("done")) == 1


def test_finalizer_notification_cannot_bypass_publication(harness):
    h = harness(count=0)
    h.until(
        lambda s: any(
            a.kind is AgentKind.FINALIZER and a.status is WorkStatus.RUNNING
            for a in s.agents
        )
    )
    snapshot = h.runtime.load()
    agent = next(a for a in snapshot.agents if a.kind is AgentKind.FINALIZER)
    assert h.runtime.notify(AgentCompleted(agent.agent_id, agent.attempt)) == snapshot
    assert h.publisher.writes == 0
    h.done()


def test_no_automatic_recovery_or_duplicate_dispatch_from_second_owner(harness):
    h = harness(count=1)
    h.until(lambda s: bool(h.runtime.active))
    other = h.new_runtime()
    before = h.runtime.load()
    assert other.step() == before
    assert len(h.executor.calls) == 1
    h.runtime.close()
    assert other.step().run.status is RunStatus.INTERRUPTED
    other.resume("take-over")
    h.runtime = other
    h.done()


def test_handler_cannot_emit_assistant_text(harness):
    h = harness(count=1)
    original = h.executor.factory

    def work(dispatch):
        if dispatch.kind is AgentKind.QUESTION:
            yield Delta("Forbidden intermediate answer")
        else:
            yield from original(dispatch)

    h.executor.factory = work
    final = h.done()
    assert final.questions[0].status is WorkStatus.FAILED
    assert "Forbidden" not in str(h.sink.events)


def test_wave2_specialists_adapter_uses_fake_models_only(harness):
    h = harness(count=0)

    def work(dispatch):
        if dispatch.kind is AgentKind.INTAKE:
            response = {"queries": [], "questions": ["An initial question"]}
        elif dispatch.kind is AgentKind.QUESTION:
            response = {
                "answer": "A concise answer",
                "evidence_ids": [],
                "followups": [],
            }
        else:
            response = {"assistant_text": "A final answer"}
        yield from specialist_steps(dispatch, Model(response))

    h.executor.factory = work
    final = h.done()
    assert final.run.status is RunStatus.COMPLETED
    assert len(final.questions) == 1
    assert h.sink.text == "A final answer"


@pytest.mark.parametrize("phase", ["before", "after"])
@pytest.mark.parametrize("boundary", range(1, 37))
def test_fault_at_each_checkpoint_commit_is_resumable(
    harness, monkeypatch, phase, boundary
):
    h = harness(count=1, followups=True)
    original = h.store.commit
    calls = 0

    def commit(snapshot, **kwargs):
        nonlocal calls
        calls += 1
        if calls == boundary and phase == "before":
            raise KeyboardInterrupt()
        result = original(snapshot, **kwargs)
        if calls == boundary and phase == "after":
            raise KeyboardInterrupt()
        return result

    monkeypatch.setattr(h.store, "commit", commit)
    try:
        h.done()
    except RuntimeError as error:
        assert str(error) == "Orchestration interrupted; resume explicitly"
    monkeypatch.setattr(h.store, "commit", original)
    h.restart()
    final = h.done()
    h.runtime.step()
    assert final.run.status is RunStatus.COMPLETED
    assert len(final.questions) == 3
    assert all(q.status is WorkStatus.COMPLETED for q in final.questions)
    assert h.publisher.writes == 1
    assert len(h.sink.of_kind("done")) == 1


@pytest.mark.parametrize("method", ["attach_exchange", "complete"])
@pytest.mark.parametrize("phase", ["before", "after"])
def test_publication_attachment_terminal_commit_faults(
    harness, monkeypatch, method, phase
):
    h = harness(count=0)
    original = getattr(h.store, method)

    def fault(*args, **kwargs):
        if phase == "before":
            raise KeyboardInterrupt()
        original(*args, **kwargs)
        raise KeyboardInterrupt()

    monkeypatch.setattr(h.store, method, fault)
    with pytest.raises(RuntimeError, match="resume explicitly"):
        h.done()
    monkeypatch.setattr(h.store, method, original)
    h.restart()
    h.done()
    h.runtime.step()
    assert len(h.final_models) == 1
    assert h.publisher.writes == 1
    assert len(h.sink.of_kind("done")) == 1
    assert h.sink.of_kind("done")[0].text == h.final_text


@pytest.mark.parametrize("phase", ["before", "after"])
@pytest.mark.parametrize("target", ["proposal-0", "proposal-1", "answer"])
def test_wave2_followup_answer_barrier_at_every_effect(
    harness, monkeypatch, phase, target
):
    h = harness(count=1)
    h.until(
        lambda s: any(
            a.kind is AgentKind.QUESTION and a.status is WorkStatus.RUNNING
            for a in s.agents
        )
    )
    original_work = h.work

    def work(dispatch):
        if dispatch.kind is AgentKind.QUESTION:
            question = h.question(dispatch)
            proposals = ["First child", "Second child"] if question.depth == 0 else []
            yield from specialist_steps(
                dispatch,
                Model(
                    {
                        "answer": "A validated answer",
                        "evidence_ids": [],
                        "followups": proposals,
                    }
                ),
            )
        else:
            yield from original_work(dispatch)

    h.executor.factory = work
    # The already submitted generator has not been polled; replace only the fake
    # factory behind it by installing a fresh explicit recovery attempt.
    h.restart("use-specialist")
    original = h.store.commit
    seen_adds = 0
    injected = False

    def commit(snapshot, **kwargs):
        nonlocal seen_adds, injected
        old = h.runtime.load()
        additions = snapshot.tool_results[len(old.tool_results) :]
        label = None
        if additions:
            tool = additions[0]
            agent = next(a for a in snapshot.agents if a.agent_id == tool.agent_id)
            question = next(
                (q for q in snapshot.questions if q.question_id == agent.question_id),
                None,
            )
            if question is not None and question.depth == 0:
                if tool.tool_name is ToolName.ADD_QUESTION:
                    label = f"proposal-{seen_adds}"
                    seen_adds += 1
                if tool.tool_name is ToolName.ANSWER_QUESTION:
                    label = "answer"
                    assert len(snapshot.questions) == 3
        if label == target and not injected and phase == "before":
            injected = True
            raise KeyboardInterrupt()
        result = original(snapshot, **kwargs)
        if label == target and not injected and phase == "after":
            injected = True
            raise KeyboardInterrupt()
        return result

    monkeypatch.setattr(h.store, "commit", commit)
    with pytest.raises(RuntimeError, match="resume explicitly"):
        h.done()
    assert injected
    snapshot = h.runtime.load()
    if snapshot.questions[0].status is WorkStatus.COMPLETED:
        assert len(snapshot.questions) == 3
    monkeypatch.setattr(h.store, "commit", original)
    h.restart("recover-effects")
    final = h.done()
    assert len(final.questions) == 3
    assert all(q.status is WorkStatus.COMPLETED for q in final.questions)
    assert h.publisher.writes == 1


def test_discard_tombstone_survives_gap_before_trace_commit(harness, monkeypatch):
    h = harness(count=1)
    h.until(lambda s: bool(h.runtime.active))
    original = h.store.discard

    def crash(*args, **kwargs):
        raise KeyboardInterrupt()

    monkeypatch.setattr(h.store, "discard", crash)
    with pytest.raises(RuntimeError, match="Discard interrupted"):
        h.runtime.discard()
    monkeypatch.setattr(h.store, "discard", original)
    h.restart()
    h.done()
    assert h.runtime.load().run.status is RunStatus.DISCARDED
    assert h.publisher.writes == 0
    assert len(h.sink.of_kind("discarded")) == 1


def test_recovery_rejects_changed_input_or_limits(harness):
    h = harness(count=0)
    h.runtime.step()
    h.runtime.close()
    with pytest.raises(ValueError, match="original request"):
        OrchestrationRuntime(
            h.store,
            *ARGS,
            "Different request",
            executor=h.executor,
            publisher=h.publisher,
            retriever=h.retriever,
            sink=h.sink,
        )
    h.limits = Limits(concurrency=1)
    with pytest.raises(ValueError, match="original request"):
        h.new_runtime()


@pytest.mark.parametrize("kind", [AgentKind.INTAKE, AgentKind.FINALIZER])
def test_nonhandler_failure_has_one_terminal_outcome(harness, kind):
    h = harness(count=0)
    original = h.executor.factory

    def work(dispatch):
        if dispatch.kind is kind:
            raise RuntimeError("private failure")
        yield from original(dispatch)

    h.executor.factory = work
    assert h.done().run.status is RunStatus.FAILED
    h.runtime.step()
    assert len(h.sink.of_kind("failed")) == 1
    assert not h.sink.of_kind("done")
    assert h.publisher.writes == 0


def test_submit_failure_is_sanitized_and_terminal(harness):
    h = harness(count=1)
    h.executor.fail_submit = True
    assert h.done().run.status is RunStatus.FAILED
    assert len(h.sink.of_kind("failed")) == 1
    assert "private" not in str(h.sink.events)


def test_stale_cas_does_not_dispatch_other_owners_claim(harness, monkeypatch):
    h = harness(count=0)
    h.until(lambda s: bool(s.agents))
    original = h.store.commit
    invoked = False

    def competing(snapshot, **kwargs):
        nonlocal invoked
        if any(a.status is WorkStatus.RUNNING for a in snapshot.agents) and not invoked:
            invoked = True
            original(snapshot, operation_id="competing-owner")
            raise TransitionConflictError("Stale orchestration revision")
        return original(snapshot, **kwargs)

    monkeypatch.setattr(h.store, "commit", competing)
    h.runtime.step()
    assert invoked
    assert not h.executor.calls
    monkeypatch.setattr(h.store, "commit", original)
    h.restart()
    h.done()


def test_late_completion_after_resume_cannot_finish_new_attempt(harness):
    h = harness(count=1)
    h.hold.add("Question 0")
    old = h.until(
        lambda s: any(
            a.kind is AgentKind.QUESTION and a.status is WorkStatus.RUNNING
            for a in s.agents
        )
    )
    agent = next(a for a in old.agents if a.kind is AgentKind.QUESTION)
    old_dispatch = next(
        d for d in h.executor.calls.values() if d.scope.agent_id == agent.agent_id
    )
    h.restart()
    h.until(
        lambda s: any(
            a.agent_id == agent.agent_id and a.status is WorkStatus.RUNNING
            for a in s.agents
        )
    )
    before = h.runtime.load()
    assert h.runtime.notify(AgentCompleted(agent.agent_id, agent.attempt)) == before
    with pytest.raises(AuthorizationError):
        old_dispatch.tools.retrieve_context("late retrieval", call_id="late")
    h.hold.clear()
    h.done()


def test_failed_question_cannot_finalize_before_retry_reconciliation(harness):
    h = harness(count=1)
    h.failures.add(("Question 0", 0))
    snapshot = h.until(
        lambda s: any(
            a.kind is AgentKind.QUESTION and a.status is WorkStatus.FAILED
            for a in s.agents
        )
    )
    assert (
        question_answered(snapshot, QuestionAnswered(snapshot.questions[0].question_id))
        == snapshot
    )
    assert (
        h.runtime.notify(QuestionAnswered(snapshot.questions[0].question_id))
        == snapshot
    )
    h.done()


def test_bounded_zero_retries_and_lower_question_depth(harness):
    h = harness(count=4, followups=True, limits=Limits(questions=2, depth=0, retries=0))
    h.failures = {("Question 0", 0)}
    final = h.done()
    assert len(final.questions) == 2
    assert all(q.depth == 0 for q in final.questions)
    assert all(a.retry_count == 0 for a in final.agents)
    assert final.run.status is RunStatus.COMPLETED


@pytest.mark.parametrize(
    "result",
    ["wrong-text", "unknown-evidence", "wrong-type", "unsafe-text", "missing-result"],
)
def test_invalid_finalizer_output_never_publishes(harness, result):
    h = harness(count=0)
    original = h.executor.factory

    def work(dispatch):
        if dispatch.kind is not AgentKind.FINALIZER:
            yield from original(dispatch)
            return
        yield Delta("A streamed answer")
        if result == "missing-result":
            return
        if result == "wrong-type":
            yield Completed(SpecialistResult(()))
        else:
            answer = (
                "Different answer" if result == "wrong-text" else "A streamed answer"
            )
            if result == "unsafe-text":
                answer = "<analysis>Private material</analysis>"
            yield Completed(
                FinalizerResult(
                    answer,
                    ("unknown",) if result == "unknown-evidence" else (),
                    (),
                    False,
                )
            )

    h.executor.factory = work
    assert h.done().run.status is RunStatus.FAILED
    assert not h.sink.of_kind("done")
    assert len(h.sink.of_kind("failed")) == 1
    assert h.publisher.writes == 0


def test_timeout_is_checked_after_worker_poll(harness):
    h = harness(count=1)
    original = h.executor.factory

    def work(dispatch):
        if dispatch.kind is AgentKind.QUESTION:
            h.time += 11
            yield Completed(SpecialistResult(()))
        else:
            yield from original(dispatch)

    h.executor.factory = work
    # Keep Intake settled before timing out a handler.
    h.limits = Limits(concurrency=1)
    h.runtime.close()
    h.runtime = OrchestrationRuntime(
        h.store,
        *ARGS,
        "Explain the result",
        executor=h.executor,
        retriever=h.retriever,
        publisher=h.publisher,
        sink=h.sink,
        limits=h.limits,
        clock=lambda: h.time,
        timeout=10,
    )
    h.runtime.resume("serial")
    final = h.done()
    assert final.questions[0].status is WorkStatus.FAILED
    assert (
        next(a for a in final.agents if a.kind is AgentKind.QUESTION).retry_count == 1
    )


def test_warning_delivery_failure_replays_without_retrying_completed_effect(
    harness, monkeypatch
):
    h = harness(count=1)
    h.failures = {("Question 0", 0), ("Question 0", 1)}
    original = h.sink.emit

    def fail(event):
        if event.kind == "warning":
            raise ValueError("private transport failure")
        original(event)

    monkeypatch.setattr(h.sink, "emit", fail)
    with pytest.raises(RuntimeError, match="resume explicitly"):
        h.done()
    assert not h.runtime.active
    monkeypatch.setattr(h.sink, "emit", original)
    h.restart()
    h.done()
    assert len(h.sink.of_kind("warning")) == 2
    assert h.publisher.writes == 1


def test_cancellation_in_reset_sink_does_not_submit_finalizer(harness, monkeypatch):
    h = harness(count=0)
    original = h.sink.emit

    def cancel(event):
        original(event)
        if event.kind == "reset":
            h.runtime.close()

    monkeypatch.setattr(h.sink, "emit", cancel)
    h.until(lambda s: s.run.status is RunStatus.INTERRUPTED)
    assert not any(d.kind is AgentKind.FINALIZER for d in h.executor.calls.values())
    assert not h.runtime.active
    assert h.publisher.writes == 0


def test_owned_execution_leaves_no_threads(harness):
    import threading

    before = set(threading.enumerate())
    h = harness(count=8)
    h.done()
    h.runtime.close()
    assert set(threading.enumerate()) == before
