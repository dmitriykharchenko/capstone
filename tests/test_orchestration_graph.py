"""Disposable stores, deterministic cooperative executors, no network/providers."""

import json
import sqlite3
import threading
from dataclasses import asdict

import pytest


from src.orchestration.agents import FinalizerResult, SpecialistResult, run_finalizer
from src.orchestration.graph import (
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
from src.orchestration.runtime import (
    Completed,
    Delta,
    Discarded,
    FinalAnswer,
    IteratorHandle,
    OrchestrationRuntime,
    Published,
)
from src.orchestration.state import (
    TERMINAL_RUNS,
    AgentKind,
    RunStatus,
    WorkStatus,
)
from src.orchestration.store import SQLiteOrchestrationStore
from src.orchestration.tools import (
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


class OwnedHandle(Handle):
    """Nonblocking iterator with a real worker reclaimed by synchronous close."""

    def __init__(self, iterator, close_error):
        self.iterator_live = False
        self.joined = 0
        self.stop = threading.Event()
        self.close_error = close_error
        super().__init__(self.work(iterator))
        next(self.iterator)
        self.thread = threading.Thread(target=self.stop.wait)
        self.thread.start()

    def work(self, iterator):
        self.iterator_live = True
        try:
            yield None
            yield from iterator
        finally:
            self.iterator_live = False
            if self.close_error == "iterator":
                raise RuntimeError("private iterator close failure")

    def close(self):
        try:
            super().close()
        finally:
            # Even an executor reporting a cleanup error must reclaim its work.
            self.stop.set()
            self.joined += 1
            self.thread.join(timeout=5)
            assert not self.thread.is_alive()
            if self.close_error == "join":
                raise RuntimeError("private join acknowledgement failure")


class OwnedExecutor(Executor):
    def __init__(self, h, close_error):
        super().__init__(h.work)
        self.h = h
        self.close_error = close_error
        self.peak = 0

    def submit(self, operation_id, dispatch):
        assert operation_id not in self.calls
        self.calls[operation_id] = dispatch
        handle = OwnedHandle(self.factory(dispatch), self.close_error)
        self.handles.append(handle)
        live = sum(h.thread.is_alive() for h in self.handles)
        self.peak = max(self.peak, live)
        assert live <= self.h.limits.concurrency
        assert sum(h.iterator_live for h in self.handles) <= self.h.limits.concurrency
        return handle

    def assert_owned(self):
        active = {a.handle for a in self.h.runtime.active.values()}
        assert len(active) <= self.h.limits.concurrency
        assert {h for h in self.handles if h.thread.is_alive()} == active
        assert {h for h in self.handles if h.iterator_live} == active
        for handle in self.handles:
            assert handle.closed == handle.joined == int(handle not in active)


@pytest.fixture
def owned_execution(monkeypatch):
    executors = []
    before = set(threading.enumerate())

    def install(h, close_error=None):
        executor = OwnedExecutor(h, close_error)
        executors.append(executor)
        h.executor = h.runtime.executor = executor
        step = h.runtime.step

        def checked_step():
            try:
                return step()
            finally:
                executor.assert_owned()

        monkeypatch.setattr(h.runtime, "step", checked_step)
        return executor

    yield install
    try:
        for executor in executors:
            try:
                executor.h.runtime.close()
                executor.assert_owned()
            finally:
                # A failing ownership assertion must never leave test workers alive.
                for handle in executor.handles:
                    try:
                        if not handle.closed:
                            handle.close()
                    except Exception:
                        pass
                    finally:
                        handle.stop.set()
                        handle.thread.join(timeout=5)
                        assert not handle.thread.is_alive()
    finally:
        assert set(threading.enumerate()) == before


class Harness:
    def __init__(
        self,
        tmp_path,
        *,
        count=2,
        limits=Limits(),
        followups=False,
    ):
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
            assert len(self.runtime.active) <= self.limits.concurrency
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


def assert_one_terminal(h, kind="done"):
    snapshot = h.runtime.load()
    for _ in range(2):
        assert h.runtime.step() == snapshot
    assert len(h.sink.of_kind(kind)) == 1
    assert sum(len(h.sink.of_kind(k)) for k in ("done", "failed", "discarded")) == 1
    assert (
        sum(
            event.kind.value in ("completed", "failed", "discarded")
            for event in h.store.events(*ARGS, limit=4096)
        )
        == 1
    )


@pytest.mark.parametrize("failed", [False, True])
def test_intake_notification_retires_owned_handle(harness, owned_execution, failed):
    h = harness(count=0)
    executor = owned_execution(h)
    h.until(lambda s: bool(h.runtime.active))
    active = next(iter(h.runtime.active.values()))
    signal = AgentCompleted(active.agent.agent_id, active.agent.attempt, failed)
    snapshot = h.runtime.notify(signal)
    executor.assert_owned()
    assert active.handle.closed == active.handle.joined == 1
    assert h.runtime.notify(signal) == snapshot
    h.done()
    assert_one_terminal(h, "failed" if failed else "done")


def owned_notification(harness, owned_execution, failed, close_error=None):
    h = harness(count=8)
    executor = owned_execution(h, close_error)
    h.hold = {f"Question {i}" for i in range(8)}
    h.until(
        lambda s: (
            len(s.questions) == 8
            and len(h.runtime.active) == 4
            and all(
                a.agent.kind is AgentKind.QUESTION for a in h.runtime.active.values()
            )
        )
    )
    active = next(iter(h.runtime.active.values()))
    agent = active.agent
    if not failed:
        dispatch = next(
            d for d in executor.calls.values() if d.scope.agent_id == agent.agent_id
        )
        dispatch.tools.answer_question(
            agent.question_id, "Durable answer", (), call_id="lost-ack-answer"
        )
    return h, executor, active, AgentCompleted(agent.agent_id, agent.attempt, failed)


@pytest.mark.parametrize("committed", [False, True])
@pytest.mark.parametrize("failed", [False, True])
@pytest.mark.parametrize("close_error", [None, "iterator", "join"])
@pytest.mark.parametrize(
    "action", ["duplicate", "step", "timeout", "discard", "cancel", "close"]
)
def test_ambiguous_notification_owns_resources_until_durable_retirement(
    harness, owned_execution, monkeypatch, committed, failed, close_error, action
):
    h, executor, active, signal = owned_notification(
        harness, owned_execution, failed, close_error
    )
    original = tuple(h.runtime.active.values())
    before = h.runtime.load()
    commit = h.store.commit

    def uncertain_commit(*args, **kwargs):
        if committed:
            commit(*args, **kwargs)
        raise RuntimeError("Synthetic commit acknowledgement failure")

    with monkeypatch.context() as patch:
        patch.setattr(h.store, "commit", uncertain_commit)
        with pytest.raises(RuntimeError, match="acknowledgement"):
            h.runtime.notify(signal)
    durable = h.runtime.load()
    if committed:
        expected = transition(before, signal, h.limits)
        assert durable.agents == expected.agents
        assert durable.questions == expected.questions
        assert active.agent.agent_id not in h.runtime.active
    else:
        assert durable == before
        assert h.runtime.active[active.agent.agent_id] is active
    assert active.handle.closed == active.handle.joined == int(committed)
    executor.assert_owned()

    if action in ("duplicate", "step"):
        if action == "duplicate" or not committed:
            snapshot = h.runtime.notify(signal)
            assert h.runtime.notify(signal) == snapshot
        count = len(executor.handles)
        h.until(lambda s: len(executor.handles) > count)
        assert active.handle.closed == active.handle.joined == 1
        if failed:
            replacement = h.runtime.active[active.agent.agent_id]
            assert replacement.agent.attempt == active.agent.attempt + 1
            assert replacement.handle is not active.handle
        snapshot = h.runtime.load()
        for hint in (signal, AgentCompleted(signal.agent_id, signal.attempt + 2)):
            assert h.runtime.notify(hint) == snapshot
            executor.assert_owned()
        h.hold.clear()
        h.done()
        assert_one_terminal(h)
    elif action == "timeout":
        h.time += 11
        h.until(lambda s: all(a.handle.closed for a in original))
        h.hold.clear()
        h.done()
        assert_one_terminal(h)
    elif action == "discard":
        discarded = h.runtime.discard()
        assert h.runtime.discard() == discarded
        assert h.runtime.notify(signal) == discarded
        assert_one_terminal(h, "discarded")
        assert h.publisher.writes == 0
    else:
        if action == "cancel":

            def cancel(*args):
                raise KeyboardInterrupt("Synthetic cancellation")

            with monkeypatch.context() as patch:
                patch.setattr(h.publisher, "lookup", cancel)
                with pytest.raises(RuntimeError, match="resume explicitly"):
                    h.runtime.step()
        h.runtime.close()
        h.runtime.close()
        assert h.runtime.load().run.status is RunStatus.INTERRUPTED
        assert h.publisher.writes == 0
    executor.assert_owned()
    assert not h.runtime.active
    assert executor.peak == 4
    assert all(a.handle.closed == a.handle.joined == 1 for a in original)


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
