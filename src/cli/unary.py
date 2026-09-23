"""Named unary command nodes; existing handlers retain all domain ownership.

The outer command runtime validates command names first. This graph selects one
handler without splitting its validation, logging, or storage transaction. It
has no retries, checkpoints, cache, or persistence of its own.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from functools import lru_cache
from typing import TYPE_CHECKING, NotRequired, TypedDict

from .config import CLIConfig
from .runtime import CommandRuntimeError, Payload

if TYPE_CHECKING:
    from langgraph.graph.state import CompiledStateGraph
    from langgraph.runtime import Runtime

UNARY_COMMANDS = (
    "health",
    "sessions",
    "messages",
    "search",
    "search_papers",
    "graph_query",
    "index_papers",
    "paper_statuses",
    "bm25_status",
    "reset_all_indices",
)


class UnaryState(TypedDict):
    command: str
    request: Payload
    result: NotRequired[Payload]


@dataclass(frozen=True)
class UnaryInvocation:
    """Configuration and adapters never enter graph state or a durable store."""

    config: CLIConfig
    graph_query: Callable[[CLIConfig, Payload], Payload]


class _HandlerFailure(Exception):
    def __init__(self, error: Exception) -> None:
        super().__init__("Existing unary handler failed")
        self.error = error


def _route(state: UnaryState) -> str:
    return state["command"]


def _handler_node(
    command: str,
) -> Callable[[UnaryState, Runtime[UnaryInvocation]], dict[str, Payload]]:
    def execute(
        state: UnaryState, runtime: Runtime[UnaryInvocation]
    ) -> dict[str, Payload]:
        context = runtime.context
        try:
            if command == "graph_query":
                result = context.graph_query(context.config, state["request"])
            else:
                from .commands import dispatch

                # Resolve at execution time, including legacy unknown-command
                # behavior when an optional handler is absent from this checkout.
                result = dispatch(command, context.config, state["request"])
        except Exception as exc:
            raise _HandlerFailure(exc) from None
        return {"result": result}

    return execute


@lru_cache(maxsize=1)
def get_unary_graph() -> CompiledStateGraph:
    """Lazily compile an allow-listed, single-handler graph without I/O."""

    from langgraph.graph import END, START, StateGraph

    graph = StateGraph(UnaryState, context_schema=UnaryInvocation)
    for command in UNARY_COMMANDS:
        graph.add_node(command, _handler_node(command))
        graph.add_edge(command, END)
    graph.add_conditional_edges(START, _route, {name: name for name in UNARY_COMMANDS})
    return graph.compile(name="cli_unary")


def run_unary(
    command: str,
    config: CLIConfig,
    request: Payload,
    *,
    graph_query: Callable[[CLIConfig, Payload], Payload],
) -> Payload:
    try:
        from langsmith import tracing_context

        with tracing_context(enabled=False):
            state = get_unary_graph().invoke(
                {"command": command, "request": request},
                context=UnaryInvocation(config, graph_query),
                config={"callbacks": []},
            )
        return state["result"]
    except _HandlerFailure as exc:
        raise exc.error from None
    except Exception:
        raise CommandRuntimeError() from None
