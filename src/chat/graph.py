"""Ephemeral, caller-pulled LangGraph workflow for one AgentLoop chat turn.

Each invocation ends at the next public event. The caller alone requests the
next step. Final provider output is fully buffered before its first public
delta, and persistence occurs only after all deltas are consumed. Resources
live in invocation context, never in a checkpoint.
"""

from __future__ import annotations

from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from functools import lru_cache
from typing import TYPE_CHECKING, Any, Literal, NotRequired, TypedDict, cast

from .agent_loop import (
    MAX_AGGREGATE_SOURCE_COUNT,
    MAX_TOTAL_HOP_COUNT,
    AgentCoverageError,
    AgentLoop,
    AgentLoopError,
    AgentModelError,
    AgentRetrievalError,
    ChatModel,
    MultihopCompletedEvent,
    MultihopCoverageCompletedEvent,
    MultihopCoverageStartedEvent,
    MultihopFinalDeltaEvent,
    MultihopHopCompletedEvent,
    MultihopHopStartedEvent,
    MultihopPlanEvent,
    MultihopProgress,
    _aggregate_sources,
    _RoundAnswerFailure,
    _answer_multihop_round,
    _chunk_text,
    _raise_if_output_limited,
    _response_text,
    _retrieve_multihop_round,
    _review_multihop_coverage,
)
from ..storage.sqlite_history import SourceRecord

if TYPE_CHECKING:
    from langgraph.graph.state import CompiledStateGraph
    from langgraph.runtime import Runtime

Phase = Literal[
    "prepare",
    "hop_started",
    "hop_completed",
    "retrieve",
    "coverage_started",
    "coverage",
    "compose",
    "final_delta",
    "done",
]


class ChatState(TypedDict):
    """Transient evidence and progress for exactly one caller-owned turn."""

    session_id: str
    user_text: str
    streaming: bool
    phase: Phase
    subqueries: NotRequired[tuple[str, ...]]
    initial_count: NotRequired[int]
    subanswers: NotRequired[tuple[str, ...]]
    contexts: NotRequired[tuple[str, ...]]
    hop_sources: NotRequired[tuple[tuple[SourceRecord, ...], ...]]
    hop_chunks: NotRequired[tuple[tuple[Any, ...], ...]]
    started: NotRequired[int]
    round_start: NotRequired[int]
    round_contexts: NotRequired[tuple[str, ...]]
    round_sources: NotRequired[tuple[tuple[SourceRecord, ...], ...]]
    round_chunks: NotRequired[tuple[tuple[Any, ...], ...]]
    round_answers: NotRequired[tuple[str, ...]]
    chunks: NotRequired[tuple[Any, ...]]
    context: NotRequired[str]
    sources: NotRequired[tuple[SourceRecord, ...]]
    reviewed: NotRequired[bool]
    parts: NotRequired[tuple[str, ...]]
    answer: NotRequired[str]
    event: NotRequired[MultihopProgress]


@dataclass
class ChatInvocation:
    """Model, history adapter, and open provider iterator stay outside state."""

    loop: AgentLoop
    model: ChatModel = field(init=False)
    types: tuple[Any, Any, Any, Any] = field(init=False)
    history: tuple[Any, ...] = field(init=False)
    deltas: Iterator[str] | None = None


class _ChatFailure(Exception):
    def __init__(self, error: Exception) -> None:
        super().__init__("Chat operation failed")
        self.error = error


def _guard_node(
    node: Callable[[ChatState, Runtime[ChatInvocation]], dict[str, Any]],
) -> Callable[[ChatState, Runtime[ChatInvocation]], dict[str, Any]]:
    def guarded(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
        try:
            return node(state, runtime)
        except Exception as exc:
            # Keep established error types/stages, without graph exception notes.
            raise _ChatFailure(exc) from None

    return guarded


def _prepare(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    call = runtime.context
    call.model, call.types, history = call.loop._start_turn(
        state["session_id"], state["user_text"]
    )
    call.history = tuple(history)
    return {
        "subanswers": (),
        "contexts": (),
        "hop_sources": (),
        "hop_chunks": (),
        "started": 0,
        "reviewed": False,
        "parts": (),
    }


def _plan(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    call = runtime.context
    queries = call.loop._plan(call.model, call.types, call.history, state["user_text"])
    return {
        "subqueries": queries,
        "initial_count": len(queries),
        "phase": "hop_started",
        "event": MultihopPlanEvent(queries),
    }


def _hop_started(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    index = state["started"]
    return {
        "started": index + 1,
        "phase": "hop_started" if index + 1 < len(state["subqueries"]) else "retrieve",
        "event": MultihopHopStartedEvent(
            index + 1, len(state["subqueries"]), state["subqueries"][index]
        ),
    }


def _retrieve(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    start = len(state["subanswers"])
    try:
        contexts, sources, chunks = _retrieve_multihop_round(
            runtime.context.loop._paper_retriever,
            state["subqueries"][start:],
        )
    except AgentRetrievalError as exc:
        if state["reviewed"]:
            setattr(exc, "stage", "followup_retrieval")
        raise
    return {
        "round_start": start,
        "round_contexts": contexts,
        "round_sources": sources,
        "round_chunks": chunks,
    }


def _subanswer(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    call = runtime.context
    start = state["round_start"]
    try:
        answers = _answer_multihop_round(
            [call.model] * len(state["round_contexts"]),
            call.types,
            call.loop._system_prompt,
            state["user_text"],
            state["subqueries"][start:],
            state["round_contexts"],
            start_index=start,
        )
    except _RoundAnswerFailure as failure:
        if isinstance(failure.error, AgentModelError) and state["reviewed"]:
            setattr(failure.error, "stage", "followup_subanswer")
        raise failure.error from None
    except AgentModelError as exc:
        if state["reviewed"]:
            setattr(exc, "stage", "followup_subanswer")
        raise
    return {
        "round_answers": answers,
        "phase": "hop_completed",
    }


def _hop_completed(
    state: ChatState, runtime: Runtime[ChatInvocation]
) -> dict[str, Any]:
    index = len(state["subanswers"])
    offset = index - state["round_start"]
    query = state["subqueries"][index]
    chunks = state["round_chunks"][offset]
    if index + 1 < len(state["subqueries"]):
        phase = "hop_completed"
    else:
        phase = "compose" if state["reviewed"] else "coverage_started"
    return {
        "subanswers": (*state["subanswers"], state["round_answers"][offset]),
        "contexts": (*state["contexts"], state["round_contexts"][offset]),
        "hop_sources": (*state["hop_sources"], state["round_sources"][offset]),
        "hop_chunks": (*state["hop_chunks"], chunks),
        "phase": phase,
        "event": MultihopHopCompletedEvent(
            index + 1,
            len(state["subqueries"]),
            query,
            state["round_answers"][offset],
            state["round_sources"][offset],
        ),
    }


def _coverage_started(
    state: ChatState, runtime: Runtime[ChatInvocation]
) -> dict[str, Any]:
    return {
        "phase": "coverage",
        "event": MultihopCoverageStartedEvent(state["initial_count"]),
    }


def _coverage(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    call = runtime.context
    followups = _review_multihop_coverage(
        call.model,
        call.types,
        state["user_text"],
        state["subqueries"],
        state["subanswers"],
    )
    queries = (*state["subqueries"], *followups)
    return {
        "subqueries": queries,
        "reviewed": True,
        "phase": "hop_started" if followups else "compose",
        "event": MultihopCoverageCompletedEvent(len(queries), followups),
    }


def _stream_composition(model: ChatModel, messages: list[Any]) -> tuple[str, ...]:
    stream_method = getattr(model, "stream", None)
    chunks = None
    parts: list[str] = []
    try:
        if callable(stream_method):
            chunks = iter(stream_method(messages))
            for chunk in chunks:
                _raise_if_output_limited(chunk)
                delta = _chunk_text(chunk)
                if delta:
                    parts.append(delta)
        else:
            response = model.invoke(messages)
            _raise_if_output_limited(response)
            parts.append(_response_text(response))
    except AgentLoopError:
        raise
    except Exception as exc:
        raise AgentModelError(f"Composer model streaming failed: {exc}") from exc
    finally:
        close = getattr(chunks, "close", None)
        if callable(close):
            close()
    answer = "".join(parts)
    if not answer.strip():
        raise AgentModelError("Chat model returned empty response text")
    return tuple(parts)


def _compose(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    if (
        len(state["subqueries"]) > MAX_TOTAL_HOP_COUNT
        or sum(map(len, state["hop_sources"])) > MAX_AGGREGATE_SOURCE_COUNT
    ):
        raise AgentCoverageError("adaptive multihop bounds were exceeded")
    call = runtime.context
    messages = call.loop._compose_messages(
        call.types,
        call.history,
        state["user_text"],
        state["subqueries"],
        state["subanswers"],
    )
    if state["streaming"]:
        call.deltas = iter(_stream_composition(call.model, messages))
        return {}
    try:
        answer = _response_text(call.loop._invoke(call.model, messages, "Composer"))
    except AgentModelError as exc:
        setattr(exc, "stage", "composer")
        raise
    return {"answer": answer}


def _final_delta(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    deltas = runtime.context.deltas
    if deltas is None:
        raise AgentLoopError("Chat ended before terminal completion")
    try:
        delta = next(deltas)
    except StopIteration:
        answer = "".join(state["parts"])
        if not answer.strip():
            raise AgentModelError("Chat model returned empty response text")
        return {"answer": answer}
    return {
        "parts": (*state["parts"], delta),
        "phase": "final_delta",
        "event": MultihopFinalDeltaEvent(delta),
    }


def _persist(state: ChatState, runtime: Runtime[ChatInvocation]) -> dict[str, Any]:
    sources = _aggregate_sources(state["hop_sources"])
    runtime.context.loop._persist(
        state["session_id"],
        state["user_text"],
        state["answer"],
        sources,
        state["contexts"],
    )
    return {"phase": "done", "event": MultihopCompletedEvent(state["answer"], sources)}


@lru_cache(maxsize=1)
def get_chat_graph() -> CompiledStateGraph:
    """Compile only topology; no retries, caching, tracing, stores, or checkpoints."""

    from langgraph.graph import END, START, StateGraph

    graph = StateGraph(ChatState, context_schema=ChatInvocation)
    for name, node in (
        ("prepare", _prepare),
        ("plan", _plan),
        ("hop_started", _hop_started),
        ("retrieve", _retrieve),
        ("subanswer", _subanswer),
        ("hop_completed", _hop_completed),
        ("coverage_started", _coverage_started),
        ("coverage", _coverage),
        ("compose", _compose),
        ("final_delta", _final_delta),
        ("persist", _persist),
    ):
        graph.add_node(name, _guard_node(node))
    graph.add_conditional_edges(
        START,
        lambda state: state["phase"],
        {
            name: name
            for name in (
                "prepare",
                "hop_started",
                "hop_completed",
                "retrieve",
                "coverage_started",
                "coverage",
                "compose",
                "final_delta",
            )
        },
    )
    graph.add_edge("prepare", "plan")
    graph.add_edge("retrieve", "subanswer")
    graph.add_edge("subanswer", "hop_completed")
    graph.add_conditional_edges(
        "compose",
        lambda state: "final_delta" if state["streaming"] else "persist",
        {"final_delta": "final_delta", "persist": "persist"},
    )
    graph.add_conditional_edges(
        "final_delta",
        lambda state: "persist" if "answer" in state else "yield",
        {"persist": "persist", "yield": END},
    )
    for name in (
        "plan",
        "hop_started",
        "hop_completed",
        "coverage_started",
        "coverage",
        "persist",
    ):
        graph.add_edge(name, END)
    return graph.compile(name="chat_turn")


def chat_events(
    loop: AgentLoop, session_id: str, user_text: str, *, streaming: bool
) -> Iterator[MultihopProgress]:
    """Pull one graph event at a time, propagating legacy errors and close."""

    state: ChatState = {
        "session_id": session_id,
        "user_text": user_text,
        "streaming": streaming,
        "phase": "prepare",
    }
    call = ChatInvocation(loop)
    try:
        while state["phase"] != "done":
            try:
                from langsmith import tracing_context

                # Do not hold a context manager open across a public yield.
                with tracing_context(enabled=False):
                    state = cast(
                        ChatState,
                        get_chat_graph().invoke(
                            state, context=call, config={"callbacks": []}
                        ),
                    )
            except _ChatFailure as exc:
                raise exc.error from None
            except Exception:
                raise AgentLoopError(
                    "Chat workflow failed. Check backend logs and retry."
                ) from None
            yield state["event"]
    finally:
        close = getattr(call.deltas, "close", None)
        if callable(close):
            close()
