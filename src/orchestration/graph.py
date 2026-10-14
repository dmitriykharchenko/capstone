"""Pure event reducers; SQLite, not LangGraph's transient state, owns recovery.

The graph returns one proposed checkpoint per invocation. Its caller must commit
that checkpoint before executing any admitted work. Replaying a notification is
harmless: all decisions use the latest committed state, never event-carried text.
"""

from __future__ import annotations

from dataclasses import dataclass, replace
from functools import lru_cache
from typing import Literal, TypedDict

from .state import (
    TERMINAL_WORK,
    AgentKind,
    AgentState,
    Checkpoint,
    ErrorCode,
    RunStatus,
    WorkStatus,
    identifier,
    integer,
)
from .tools import Limits, digest


@dataclass(frozen=True)
class QuestionAdded:
    question_id: str

    def __post_init__(self):
        identifier(self.question_id)


@dataclass(frozen=True)
class QuestionAnswered:
    question_id: str

    def __post_init__(self):
        identifier(self.question_id)


@dataclass(frozen=True)
class AgentCompleted:
    agent_id: str
    attempt: int
    failed: bool = False

    def __post_init__(self):
        identifier(self.agent_id)
        integer(self.attempt, 0, 4096)
        if type(self.failed) is not bool:
            raise ValueError("Invalid completion status")


@dataclass(frozen=True)
class Reconcile:
    pass


Signal = QuestionAdded | QuestionAnswered | AgentCompleted | Reconcile


def accepting(snapshot: Checkpoint) -> bool:
    return snapshot.run.status is RunStatus.RUNNING and snapshot.run.exchange_id is None


def _agents(snapshot: Checkpoint, agent: AgentState) -> Checkpoint:
    return replace(
        snapshot,
        agents=tuple(
            agent if a.agent_id == agent.agent_id else a for a in snapshot.agents
        ),
    )


def question_added(snapshot: Checkpoint, event: QuestionAdded) -> Checkpoint:
    """The question must already be durably pending before creating its handler."""
    question = next(
        (q for q in snapshot.questions if q.question_id == event.question_id), None
    )
    if (
        not accepting(snapshot)
        or question is None
        or question.status in TERMINAL_WORK
        or any(a.kind is AgentKind.FINALIZER for a in snapshot.agents)
        or any(a.question_id == question.question_id for a in snapshot.agents)
    ):
        return snapshot
    return replace(
        snapshot,
        agents=snapshot.agents
        + (
            AgentState(
                "handler-" + digest(question.question_id)[:32],
                AgentKind.QUESTION,
                question_id=question.question_id,
            ),
        ),
    )


def question_answered(
    snapshot: Checkpoint, event: QuestionAnswered, limits: Limits = Limits()
) -> Checkpoint:
    """Fan-in includes dispatch acknowledgements, not merely answered questions."""
    if (
        not accepting(snapshot)
        or not snapshot.agents
        or not any(
            a.kind is AgentKind.INTAKE and a.status is WorkStatus.COMPLETED
            for a in snapshot.agents
        )
        or any(a.kind is AgentKind.FINALIZER for a in snapshot.agents)
        or any(a.status not in TERMINAL_WORK for a in snapshot.agents)
        or any(
            a.kind is AgentKind.QUESTION
            and a.status is WorkStatus.FAILED
            and a.retry_count < limits.retries
            for a in snapshot.agents
        )
        or any(q.status not in TERMINAL_WORK for q in snapshot.questions)
    ):
        return snapshot
    return replace(
        snapshot,
        agents=snapshot.agents + (AgentState("finalizer", AgentKind.FINALIZER),),
    )


def agent_completed(snapshot: Checkpoint, event: AgentCompleted) -> Checkpoint:
    """Route a fenced acknowledgement using committed answers, not worker copies.

    Finalizer acknowledgement is supplied only after canonical publication. An
    answer committed before a handler interruption is retained and never retried.
    """
    agent = next((a for a in snapshot.agents if a.agent_id == event.agent_id), None)
    if (
        not accepting(snapshot)
        or agent is None
        or agent.attempt != event.attempt
        or agent.status not in (WorkStatus.RUNNING, WorkStatus.WAITING)
    ):
        return snapshot
    failed = event.failed
    if agent.kind is AgentKind.QUESTION:
        question = next(
            q for q in snapshot.questions if q.question_id == agent.question_id
        )
        failed = question.status is not WorkStatus.COMPLETED
        if failed:
            snapshot = replace(
                snapshot,
                questions=tuple(
                    replace(q, status=WorkStatus.FAILED) if q == question else q
                    for q in snapshot.questions
                ),
            )
    return _agents(
        snapshot,
        replace(
            agent,
            status=WorkStatus.FAILED if failed else WorkStatus.COMPLETED,
            error_code=ErrorCode.AGENT_FAILED if failed else None,
        ),
    )


def reconcile(snapshot: Checkpoint, limits: Limits) -> Checkpoint:
    if not accepting(snapshot):
        return snapshot
    if not snapshot.agents:
        return replace(snapshot, agents=(AgentState("intake", AgentKind.INTAKE),))
    for question in snapshot.questions:
        candidate = question_added(snapshot, QuestionAdded(question.question_id))
        if candidate != snapshot:
            return candidate
    for agent in snapshot.agents:
        if (
            agent.kind is AgentKind.QUESTION
            and agent.status is WorkStatus.FAILED
            and agent.retry_count < limits.retries
        ):
            return replace(
                _agents(
                    snapshot,
                    replace(
                        agent,
                        status=WorkStatus.QUEUED,
                        attempt=agent.attempt + 1,
                        retry_count=agent.retry_count + 1,
                        error_code=None,
                    ),
                ),
                questions=tuple(
                    replace(q, status=WorkStatus.QUEUED)
                    if q.question_id == agent.question_id
                    else q
                    for q in snapshot.questions
                ),
            )
    candidate = question_answered(snapshot, QuestionAnswered("reconcile"), limits)
    if candidate != snapshot:
        return candidate
    running = sum(
        a.status in (WorkStatus.RUNNING, WorkStatus.WAITING) for a in snapshot.agents
    )
    for agent in snapshot.agents:
        if agent.status is WorkStatus.QUEUED and running < limits.concurrency:
            return replace(
                _agents(snapshot, replace(agent, status=WorkStatus.RUNNING)),
                questions=tuple(
                    replace(q, status=WorkStatus.RUNNING)
                    if q.question_id == agent.question_id
                    and q.status is WorkStatus.QUEUED
                    else q
                    for q in snapshot.questions
                ),
            )
    return snapshot


class TransitionState(TypedDict):
    snapshot: Checkpoint
    signal: Signal
    limits: Limits


def _route(
    state: TransitionState,
) -> Literal["question_added", "question_answered", "agent_completed", "reconcile"]:
    return {
        QuestionAdded: "question_added",
        QuestionAnswered: "question_answered",
        AgentCompleted: "agent_completed",
        Reconcile: "reconcile",
    }[type(state["signal"])]


def _added(state: TransitionState) -> dict:
    return {"snapshot": question_added(state["snapshot"], state["signal"])}


def _answered(state: TransitionState) -> dict:
    return {
        "snapshot": question_answered(
            state["snapshot"], state["signal"], state["limits"]
        )
    }


def _completed(state: TransitionState) -> dict:
    return {"snapshot": agent_completed(state["snapshot"], state["signal"])}


def _reconcile(state: TransitionState) -> dict:
    return {"snapshot": reconcile(state["snapshot"], state["limits"])}


@lru_cache(maxsize=1)
def transition_graph():
    """No provider, worker, checkpoint backend, or background task is constructed."""
    from langgraph.graph import END, START, StateGraph

    graph = StateGraph(TransitionState)
    for name, node in (
        ("question_added", _added),
        ("question_answered", _answered),
        ("agent_completed", _completed),
        ("reconcile", _reconcile),
    ):
        graph.add_node(name, node)
        graph.add_edge(name, END)
    graph.add_conditional_edges(START, _route)
    return graph.compile()


def transition(snapshot: Checkpoint, signal: Signal, limits: Limits) -> Checkpoint:
    return transition_graph().invoke(
        {"snapshot": snapshot, "signal": signal, "limits": limits}
    )["snapshot"]
