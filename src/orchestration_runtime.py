"""Caller-pulled durable orchestration with injectable, owned execution handles.

One owner calls step/resume/discard serially; recovery requires stopping the old
owner first. No threads, processes, providers, history stores or executors are
created here. Executors may multiplex cooperative work; poll must not block.
They must close/join their resources on close, and mutate state only via tools.

Publication is a *durable linearizable port*, not a best-effort callback: publish
and discard arbitrate on the same run key, lookup survives restarts, and repeated
publish returns the original reply. A commit followed by a lost acknowledgement
is recovered with lookup. Implementing that port in canonical history is a later
wave. Sink delivery is idempotent by (run_id, sequence, kind); the committed
journal, not callback delivery, defines the single terminal outcome.
"""

from __future__ import annotations

from collections.abc import Callable, Iterator
from dataclasses import asdict, dataclass, replace
from time import monotonic
from typing import Literal, Protocol

from .orchestration_agents import (
    FinalizerInput,
    FinalizerResult,
    HandlerInput,
    HistoryTurn,
    IntakeInput,
    SpecialistResult,
    StructuredModel,
    run_finalizer,
    run_handler,
    run_intake,
)
from .orchestration_graph import AgentCompleted, Reconcile, Signal, transition
from .orchestration_state import (
    TERMINAL_RUNS,
    AgentKind,
    AgentState,
    Checkpoint,
    RunStatus,
    WorkStatus,
    identifiers,
    integer,
    text,
)
from .orchestration_store import SQLiteOrchestrationStore, TransitionConflictError
from .orchestration_tools import (
    Limits,
    OrchestrationTools,
    Retriever,
    ToolScope,
    digest,
    final_evidence_ids,
    public_text,
)

WARNINGS = {
    "duplicate_question": "Duplicate questions were omitted.",
    "question_limit": "Some follow-up questions exceeded the work budget.",
    "empty_evidence": "Some retrievals found no supporting evidence.",
    "retrieval_failed": "Some evidence retrievals failed.",
    "question_failed": "Some questions failed; the answer may be incomplete.",
    "handler_retry": "A question handler failed or timed out and will retry once.",
    "derived_index": "The reply was saved, but its search index could not be updated.",
}


@dataclass(frozen=True)
class FinalAnswer:
    assistant_text: str
    evidence_ids: tuple[str, ...] = ()

    def __post_init__(self):
        public_text(self.assistant_text, 8192)
        identifiers(self.evidence_ids, 128)


@dataclass(frozen=True)
class Published:
    exchange_id: int
    answer: FinalAnswer
    index_warning: bool = False

    def __post_init__(self):
        integer(self.exchange_id, 1, 2**63 - 1)
        if type(self.answer) is not FinalAnswer or type(self.index_warning) is not bool:
            raise ValueError("Invalid publication receipt")
        self.answer.__post_init__()


@dataclass(frozen=True)
class Discarded:
    """Durable publication tombstone, distinguishable from an unused key."""


class Publisher(Protocol):
    def lookup(self, key: str) -> Published | Discarded | None: ...

    def publish(self, key: str, answer: FinalAnswer) -> Published: ...

    def discard(self, key: str) -> Published | Discarded:
        """Atomically tombstone an unpublished key, or return its saved reply."""
        ...


@dataclass(frozen=True)
class Output:
    run_id: str
    sequence: int
    kind: Literal["reset", "delta", "warning", "done", "failed", "discarded"]
    text: str = ""
    exchange_id: int | None = None


class Sink(Protocol):
    def emit(self, event: Output) -> None:
        """Deliver once per (run_id, sequence, kind), including after reconnect."""
        ...


class _SinkFailure(RuntimeError):
    pass


@dataclass(frozen=True)
class Delta:
    text: str


@dataclass(frozen=True)
class Completed:
    result: SpecialistResult | FinalizerResult


class Handle(Protocol):
    def poll(self) -> Delta | Completed | None: ...

    def close(self) -> None: ...


@dataclass(frozen=True)
class Dispatch:
    scope: ToolScope
    kind: AgentKind
    request: IntakeInput | HandlerInput | FinalizerInput
    tools: OrchestrationTools


class Executor(Protocol):
    def submit(self, operation_id: str, dispatch: Dispatch) -> Handle:
        """Take ownership once for a committed attempt; close cancels and joins."""
        ...


class IteratorHandle:
    """Adapter for cooperative iterators; final output is an explicit item."""

    def __init__(self, iterator: Iterator[Delta | Completed | None]):
        self.iterator = iterator

    def poll(self):
        return next(self.iterator)

    def close(self):
        close = getattr(self.iterator, "close", None)
        if close is not None:
            close()


def specialist_steps(dispatch: Dispatch, model: StructuredModel):
    """Optional synchronous adapter for Wave 2; callers own the injected model.

    Structured output is validated before chunk delivery. Async/provider-backed
    executors should implement nonblocking poll instead of using this adapter.
    """
    if dispatch.kind is AgentKind.INTAKE:
        result = run_intake(model, dispatch.tools, dispatch.request)
    elif dispatch.kind is AgentKind.QUESTION:
        result = run_handler(model, dispatch.tools, dispatch.request)
    else:
        result = run_finalizer(model, dispatch.tools, dispatch.request)
        for start in range(0, len(result.assistant_text), 256):
            yield Delta(result.assistant_text[start : start + 256])
    yield Completed(result)


@dataclass
class _Active:
    agent: AgentState
    handle: Handle
    deadline: float
    answer: str = ""


class OrchestrationRuntime:
    def __init__(
        self,
        store: SQLiteOrchestrationStore,
        run_id: str,
        session_id: str,
        graph_version: str,
        original_query: str,
        *,
        executor: Executor,
        retriever: Retriever,
        publisher: Publisher,
        sink: Sink,
        history: tuple[HistoryTurn, ...] = (),
        limits: Limits = Limits(),
        clock: Callable[[], float] = monotonic,
        timeout: float = 60.0,
    ):
        public_text(original_query, 2048)
        if type(limits) is not Limits:
            raise ValueError("Invalid runtime limits")
        if type(history) is not tuple or len(history) > 16:
            raise ValueError("Invalid public history")
        for turn in history:
            if type(turn) is not HistoryTurn or turn.role not in ("user", "assistant"):
                raise ValueError("Invalid public history")
            public_text(turn.content, 2048)
        if type(timeout) not in (int, float) or not 0 < timeout <= 3600:
            raise ValueError("Invalid timeout")
        self.store, self.args = store, (run_id, session_id, graph_version)
        self.query, self.history, self.limits = original_query, history, limits
        self.executor, self.retriever = executor, retriever
        self.publisher, self.sink, self.clock = publisher, sink, clock
        self.timeout = timeout
        self.key = "exchange-" + digest(self.args)
        self.active: dict[str, _Active] = {}
        self.closed = False
        self._busy = False
        self._terminal_sent = False
        self.request_id = "graph:request:" + digest(
            {
                "query": original_query,
                "history": [asdict(t) for t in history],
                "limits": asdict(limits),
            }
        )
        self.load()
        for event in self._events():
            if (
                event.operation_id.startswith("graph:request:")
                and event.operation_id != self.request_id
            ):
                raise ValueError("Recovery requires the original request and limits")

    def load(self) -> Checkpoint:
        return self.store.load(*self.args)

    def _emit(self, snapshot: Checkpoint, kind, value="", exchange_id=None):
        self._deliver(Output(self.args[0], snapshot.sequence, kind, value, exchange_id))

    def _deliver(self, event: Output):
        try:
            self.sink.emit(event)
        except Exception:
            raise _SinkFailure("Output delivery interrupted") from None

    def _commit(self, snapshot: Checkpoint, label: str) -> Checkpoint:
        return self.store.commit(
            snapshot, operation_id=f"graph:{snapshot.sequence}:{label}"
        )

    def _warning(self, code: str, agent: AgentState) -> None:
        snapshot = self.load()
        label = f"warning:{code}:{agent.agent_id}:{agent.attempt}"
        for event in self._events():
            if event.operation_id.endswith(label):
                self._deliver(
                    Output(self.args[0], event.sequence, "warning", WARNINGS[code])
                )
                return
        snapshot = self._commit(snapshot, label)
        self._emit(snapshot, "warning", WARNINGS[code])

    def _events(self):
        return self.store.events(*self.args, limit=4096)

    def _warning_codes(self) -> tuple[str, ...]:
        events = self._events()
        return tuple(
            sorted(
                {
                    code
                    for code in (
                        "duplicate_question",
                        "question_limit",
                        "empty_evidence",
                        "retrieval_failed",
                    )
                    if any(f":warning:{code}:" in e.operation_id for e in events)
                }
            )
        )

    def _fenced(self, agent: AgentState, snapshot: Checkpoint) -> bool:
        return (
            snapshot.run.status is not RunStatus.RUNNING
            or snapshot.run.exchange_id is not None
            or not any(
                a.agent_id == agent.agent_id
                and a.attempt == agent.attempt
                and a.status is WorkStatus.RUNNING
                for a in snapshot.agents
            )
        )

    def _close_active(self, agent_id: str):
        active = self.active.pop(agent_id, None)
        if active is not None:
            try:
                active.handle.close()
            except Exception:
                # Continue closing other handles; exception details never escape.
                pass

    def _close_all(self):
        for agent_id in tuple(self.active):
            self._close_active(agent_id)

    def _retire_fenced(self, snapshot: Checkpoint):
        # Durable slots may already be reusable after a lost acknowledgement.
        # Retire their physical owners before any reconciliation or admission.
        for agent_id, active in tuple(self.active.items()):
            if self._fenced(active.agent, snapshot):
                self._close_active(agent_id)

    def _dispatch(self, agent: AgentState, snapshot: Checkpoint):
        if self._fenced(agent, self.load()):
            return
        if agent.agent_id in self.active or len(self.active) >= self.limits.concurrency:
            raise RuntimeError("Retire owned attempts before dispatch")
        if agent.kind is AgentKind.QUESTION:
            question = next(
                q for q in snapshot.questions if q.question_id == agent.question_id
            )
            if question.status is WorkStatus.COMPLETED:
                self.notify(AgentCompleted(agent.agent_id, agent.attempt))
                return
            request = HandlerInput(self.query, agent.question_id)
        elif agent.kind is AgentKind.INTAKE:
            request = IntakeInput(self.query)
        else:
            request = FinalizerInput(self.query, self.history, self._warning_codes())
            self._emit(snapshot, "reset")
        if self._fenced(agent, self.load()):
            return
        scope = ToolScope(*self.args, agent.agent_id, agent.attempt)
        tools = OrchestrationTools(
            self.store, scope, self.retriever, limits=self.limits
        )
        try:
            handle = self.executor.submit(
                f"dispatch:{digest(self.args)[:32]}:{agent.agent_id}:{agent.attempt}",
                Dispatch(scope, agent.kind, request, tools),
            )
        except Exception:
            self._failure(agent)
            return
        self.active[agent.agent_id] = _Active(
            agent, handle, self.clock() + self.timeout
        )

    def notify(self, signal: Signal) -> Checkpoint:
        """Notifications are hints; reload and CAS the latest committed effects."""
        old = self.load()
        if isinstance(signal, AgentCompleted) and any(
            a.agent_id == signal.agent_id and a.kind is AgentKind.FINALIZER
            for a in old.agents
        ):
            return old
        if isinstance(signal, Reconcile):
            self._retire_fenced(old)
        candidate = transition(old, signal, self.limits)
        if candidate == old:
            self._retire_notified(signal, old)
            return old
        try:
            result = self._commit(candidate, type(signal).__name__)
        except BaseException as error:
            # A raised commit may still be durable. A failed reload leaves the
            # handle owned; the next reconciliation must fence it before reuse.
            result = self.load()
            self._retire_notified(signal, result)
            if not isinstance(error, TransitionConflictError):
                raise
        self._retire_notified(signal, result)
        return result

    def _retire_notified(self, signal: Signal, snapshot: Checkpoint):
        if isinstance(signal, AgentCompleted):
            active = self.active.get(signal.agent_id)
            if (
                active is not None
                and active.agent.attempt == signal.attempt
                and any(
                    a.agent_id == signal.agent_id
                    and a.attempt == signal.attempt
                    and a.kind is not AgentKind.FINALIZER
                    and a.status in (WorkStatus.COMPLETED, WorkStatus.FAILED)
                    for a in snapshot.agents
                )
            ):
                self._close_active(signal.agent_id)

    def _failure(self, agent: AgentState):
        if self._fenced(agent, self.load()):
            return
        self._close_active(agent.agent_id)
        if agent.kind is AgentKind.QUESTION:
            snapshot = self.notify(AgentCompleted(agent.agent_id, agent.attempt, True))
            failed = next(a for a in snapshot.agents if a.agent_id == agent.agent_id)
            if failed.status is WorkStatus.FAILED:
                code = (
                    "handler_retry"
                    if agent.retry_count < self.limits.retries
                    else "question_failed"
                )
                self._warning(code, agent)
        else:
            self.store.fail(
                *self.args,
                operation_id=f"graph:failed:{agent.agent_id}:{agent.attempt}",
            )

    def _validate_publication(self, value: Published) -> Published:
        if type(value) is not Published:
            raise ValueError("Invalid publication receipt")
        value.__post_init__()
        return value

    def _finish_publication(self, saved: Published) -> Checkpoint:
        saved = self._validate_publication(saved)
        snapshot = self.load()
        finalizer = next(a for a in snapshot.agents if a.kind is AgentKind.FINALIZER)
        if finalizer.status is WorkStatus.QUEUED:
            snapshot = self._commit(
                replace(
                    snapshot,
                    agents=tuple(
                        replace(a, status=WorkStatus.RUNNING) if a == finalizer else a
                        for a in snapshot.agents
                    ),
                ),
                "recover-publication",
            )
        if finalizer.status is not WorkStatus.COMPLETED:
            snapshot = self._commit(
                transition(
                    snapshot,
                    AgentCompleted(finalizer.agent_id, finalizer.attempt),
                    self.limits,
                ),
                "published-finalizer",
            )
        if saved.index_warning and snapshot.run.exchange_id is None:
            self._warning("derived_index", finalizer)
        self.store.attach_exchange(
            *self.args, saved.exchange_id, operation_id="graph:attach"
        )
        return self.store.complete(*self.args, operation_id="graph:done")

    def _completed(self, active: _Active, value: Completed):
        agent = active.agent
        if agent.kind is not AgentKind.FINALIZER:
            if type(value.result) is not SpecialistResult:
                raise ValueError("Invalid specialist result")
            if type(value.result.warnings) is not tuple or any(
                w not in WARNINGS
                or w in ("derived_index", "question_failed", "handler_retry")
                for w in value.result.warnings
            ):
                raise ValueError("Invalid specialist warnings")
            for code in sorted(set(value.result.warnings)):
                self._warning(code, agent)
            self.notify(AgentCompleted(agent.agent_id, agent.attempt))
            return
        result = value.result
        if type(result) is not FinalizerResult:
            raise ValueError("Invalid finalizer result")
        answer = FinalAnswer(result.assistant_text, result.evidence_ids)
        allowed = final_evidence_ids(self.load())
        if not set(answer.evidence_ids) <= allowed or (
            active.answer and active.answer != answer.assistant_text
        ):
            raise ValueError("Finalizer output changed")
        if not active.answer:
            self._delta(active, Delta(answer.assistant_text))
        if self._fenced(agent, self.load()):
            return
        # Publication failure is ambiguous: leave the agent resumable, never fail
        # or discard a possibly saved canonical answer. Resume consults lookup.
        try:
            saved = self.publisher.publish(self.key, answer)
            self._finish_publication(saved)
        except Exception:
            self._interrupt()

    def _delta(self, active: _Active, value: Delta):
        if active.agent.kind is not AgentKind.FINALIZER:
            raise ValueError("Only finalizer may stream")
        text(value.text, 4096)
        combined = active.answer + value.text
        public_text(combined, 8192)
        snapshot = self._commit(self.load(), "final-delta")
        active.answer = combined
        self._emit(snapshot, "delta", value.text)

    def _poll(self):
        # Rotate insertion order so an idle/slow worker cannot starve peers.
        agent_id = next(iter(self.active))
        active = self.active.pop(agent_id)
        self.active[agent_id] = active
        if self._fenced(active.agent, self.load()):
            self._close_active(agent_id)
            return
        if self.clock() >= active.deadline:
            self._failure(active.agent)
            return
        try:
            value = active.handle.poll()
        except Exception:
            self._failure(active.agent)
            return
        if self._fenced(active.agent, self.load()):
            self._close_active(agent_id)
            return
        if self.clock() >= active.deadline:
            self._failure(active.agent)
            return
        if value is None:
            return
        try:
            if type(value) is Delta:
                self._delta(active, value)
            elif type(value) is Completed:
                self._close_active(agent_id)
                self._completed(active, value)
            else:
                raise ValueError("Invalid worker result")
        except (ValueError, TypeError):
            self._failure(active.agent)

    def _terminal(self, snapshot: Checkpoint):
        self._close_all()
        if not self._terminal_sent:
            kind = (
                "done"
                if snapshot.run.status is RunStatus.COMPLETED
                else snapshot.run.status.value
            )
            answer = ""
            if kind == "done":
                saved = self._validate_publication(self.publisher.lookup(self.key))
                if saved.exchange_id != snapshot.run.exchange_id:
                    raise ValueError("Publication receipt mismatch")
                answer = saved.answer.assistant_text
            self._emit(snapshot, kind, answer, exchange_id=snapshot.run.exchange_id)
            self._terminal_sent = True

    def step(self) -> Checkpoint:
        """Advance one transition or poll; do no work while the caller is paused."""
        if self.closed or self._busy:
            raise RuntimeError("Runtime is closed or already advancing")
        self._busy = True
        try:
            old = self.load()
            if old.run.status in TERMINAL_RUNS:
                self._terminal(old)
                return old
            if old.run.status is RunStatus.INTERRUPTED:
                self._close_all()
                return old
            saved = self.publisher.lookup(self.key)
            if type(saved) is Discarded:
                result = self.store.discard(*self.args, operation_id="graph:discard")
                self._terminal(result)
                return result
            if saved is not None:
                result = self._finish_publication(saved)
                self._terminal(result)
                return result
            if not any(e.operation_id == self.request_id for e in self._events()):
                return self.store.commit(old, operation_id=self.request_id)
            for agent in old.agents:
                if agent.status is WorkStatus.FAILED:
                    if agent.kind is not AgentKind.QUESTION:
                        result = self.store.fail(
                            *self.args,
                            operation_id=f"graph:failed:{agent.agent_id}:{agent.attempt}",
                        )
                        self._terminal(result)
                        return result
                    self._warning(
                        "handler_retry"
                        if agent.retry_count < self.limits.retries
                        else "question_failed",
                        agent,
                    )
            old = self.load()
            self._retire_fenced(old)
            candidate = transition(old, Reconcile(), self.limits)
            new = old
            if candidate != old:
                try:
                    new = self._commit(candidate, "Reconcile")
                except TransitionConflictError:
                    return self.load()
            if new != old:
                for before, after in zip(old.agents, new.agents):
                    if (
                        before.status is WorkStatus.QUEUED
                        and after.status is WorkStatus.RUNNING
                    ):
                        self._dispatch(after, new)
            elif self.active:
                self._poll()
            result = self.load()
            if result.run.status in TERMINAL_RUNS:
                self._terminal(result)
            return result
        except BaseException:
            self._interrupt()
            raise RuntimeError("Orchestration interrupted; resume explicitly") from None
        finally:
            self._busy = False

    def _interrupt(self):
        self._close_all()
        snapshot = self.load()
        if (
            snapshot.run.status is RunStatus.RUNNING
            and snapshot.run.exchange_id is None
        ):
            self.store.interrupt(
                *self.args, operation_id=f"graph:interrupt:{snapshot.sequence}"
            )

    def resume(self, operation_id: str) -> Checkpoint:
        if self._busy or self.active or self.closed:
            raise RuntimeError("Stop the prior owner before resuming")
        snapshot = self.load()
        if snapshot.run.status in TERMINAL_RUNS:
            return snapshot
        if snapshot.run.exchange_id is not None:
            return self.store.complete(*self.args, operation_id="graph:done")
        self.store.resume(*self.args, operation_id=operation_id)
        return self.load()

    def discard(self) -> Checkpoint:
        if self._busy:
            raise RuntimeError("Runtime is already advancing")
        try:
            return self._discard()
        except BaseException:
            self._interrupt()
            raise RuntimeError("Discard interrupted; retry explicitly") from None

    def _discard(self) -> Checkpoint:
        self._close_all()
        snapshot = self.load()
        if snapshot.run.status in TERMINAL_RUNS:
            return snapshot
        saved = self.publisher.discard(self.key)
        if type(saved) is Published:
            if snapshot.run.status is RunStatus.INTERRUPTED:
                self.resume(f"graph:retain:{snapshot.sequence}")
            return self._finish_publication(saved)
        if type(saved) is not Discarded:
            raise ValueError("Invalid discard receipt")
        return self.store.discard(*self.args, operation_id="graph:discard")

    def close(self):
        if not self.closed:
            self._interrupt()
            self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
