"""Ephemeral LangGraph entry path for the existing CLI command handlers.

This first migration wave only selects and invokes existing handlers. Stream
iterators are pulled by the caller, not a graph worker: prefetching could move
terminal side effects ahead of stdout delivery. No checkpoints, retries, cache,
or durable store are configured. Importing this module performs no graph work.
"""

from __future__ import annotations

from collections.abc import Callable, Iterator
from dataclasses import dataclass
from functools import lru_cache
from types import MappingProxyType
from typing import TYPE_CHECKING, Any, Literal, NotRequired, TypedDict, cast

if TYPE_CHECKING:
    from langgraph.graph.state import CompiledStateGraph
    from langgraph.runtime import Runtime

Payload = dict[str, Any]
UnaryHandler = Callable[[Any, Payload], Payload]
StreamFactory = Callable[[], Iterator[Payload]]


@dataclass(frozen=True)
class CommandSpec:
    unary: bool = True
    streaming: bool = False


COMMANDS = MappingProxyType(
    {
        "health": CommandSpec(),
        "chat": CommandSpec(streaming=True),
        "sessions": CommandSpec(),
        "messages": CommandSpec(),
        "search": CommandSpec(),
        "search_papers": CommandSpec(),
        "index_papers": CommandSpec(streaming=True),
        "paper_statuses": CommandSpec(),
        "bm25_status": CommandSpec(),
        "reset_all_indices": CommandSpec(),
        "graph_query": CommandSpec(),
        "evaluate": CommandSpec(unary=False, streaming=True),
    }
)


class CommandState(TypedDict):
    """One invocation only; never a conversation or persistence authority."""

    command: Any
    request: Payload
    streaming: bool
    route: NotRequired[Literal["unary", "stream"]]
    result: NotRequired[Payload]
    events: NotRequired[Iterator[Payload]]


@dataclass(frozen=True)
class Invocation:
    """Adapters (and their captured configuration) stay outside graph state."""

    unary: UnaryHandler | None = None
    stream: StreamFactory | None = None


class CommandRuntimeError(RuntimeError):
    """Fixed public failure for graph infrastructure, not legacy handlers."""

    def __init__(self) -> None:
        super().__init__("Command runtime failed. Check backend logs and retry.")


class _HandlerFailure(Exception):
    def __init__(self, error: Exception) -> None:
        super().__init__("Existing command failed")
        self.error = error


def _validate(state: CommandState) -> dict[str, str]:
    try:
        spec = COMMANDS.get(state["command"])
        if spec is None or not (spec.streaming if state["streaming"] else spec.unary):
            raise ValueError(f"unknown command {state['command']!r}")
    except (ValueError, TypeError) as exc:
        # Preserve legacy unknown-command and unhashable-command failures.
        raise _HandlerFailure(exc) from None
    return {"route": "stream" if state["streaming"] else "unary"}


def _route(state: CommandState) -> str:
    return state["route"]


def _unary(state: CommandState, runtime: Runtime[Invocation]) -> dict[str, Payload]:
    handler = runtime.context.unary
    if handler is None:
        raise CommandRuntimeError()
    try:
        return {"result": handler(state["command"], state["request"])}
    except Exception as exc:
        raise _HandlerFailure(exc) from None


def _stream(
    state: CommandState, runtime: Runtime[Invocation]
) -> dict[str, Iterator[Payload]]:
    factory = runtime.context.stream
    if factory is None:
        raise CommandRuntimeError()
    try:
        return {"events": factory()}
    except Exception as exc:
        raise _HandlerFailure(exc) from None


@lru_cache(maxsize=1)
def get_command_graph() -> CompiledStateGraph:
    """Compile a reusable, deterministic routing graph lazily, without I/O."""

    from langgraph.graph import END, START, StateGraph

    graph = StateGraph(CommandState, context_schema=Invocation)
    graph.add_node("validate", _validate)
    graph.add_node("unary", _unary)
    graph.add_node("stream", _stream)
    graph.add_edge(START, "validate")
    graph.add_conditional_edges(
        "validate", _route, {"unary": "unary", "stream": "stream"}
    )
    graph.add_edge("unary", END)
    graph.add_edge("stream", END)
    return graph.compile(name="cli_command")


def _invoke(
    command: Any, request: Payload, *, streaming: bool, context: Invocation
) -> Payload | Iterator[Payload]:
    try:
        from langsmith import tracing_context

        # A CLI invocation must not acquire an external persistence/tracing
        # authority just because LangGraph is installed or tracing is enabled.
        with tracing_context(enabled=False):
            state = get_command_graph().invoke(
                {"command": command, "request": request, "streaming": streaming},
                context=context,
                config={"callbacks": []},
            )
        return state["events"] if streaming else state["result"]
    except _HandlerFailure as exc:
        raise exc.error from None
    except Exception:
        # Never expose graph internals, configuration, or exception details.
        raise CommandRuntimeError() from None


def run_command(command: Any, request: Payload, handler: UnaryHandler) -> Payload:
    return cast(
        Payload,
        _invoke(command, request, streaming=False, context=Invocation(unary=handler)),
    )


def stream_command(
    command: Any, request: Payload, factory: StreamFactory
) -> Iterator[Payload]:
    """Select a stream through LangGraph, forwarding pulls/close without replay."""

    events = cast(
        Iterator[Payload],
        _invoke(command, request, streaming=True, context=Invocation(stream=factory)),
    )
    yield from events
