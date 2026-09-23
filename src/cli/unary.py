"""Named unary command nodes; existing handlers retain all domain ownership.

The outer command runtime validates command names first. This graph selects one
handler without splitting its validation, logging, or storage transaction. It
has no retries, checkpoints, cache, or persistence of its own.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from functools import lru_cache
from types import MappingProxyType
from typing import TYPE_CHECKING, NotRequired, TypedDict

from .config import CLIConfig
from .runtime import CommandRuntimeError, Payload

if TYPE_CHECKING:
    from langgraph.graph.state import CompiledStateGraph
    from langgraph.runtime import Runtime

# Whether the established handler takes the request in addition to config.
UNARY_COMMANDS = MappingProxyType(
    {
        "health": False,
        "sessions": False,
        "messages": True,
        "search": True,
        "search_papers": True,
        "graph_query": True,
        "index_papers": False,
        "paper_statuses": False,
        "bm25_status": False,
        "reset_all_indices": False,
    }
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
    command: str, takes_request: bool
) -> Callable[[UnaryState, Runtime[UnaryInvocation]], dict[str, Payload]]:
    def execute(
        state: UnaryState, runtime: Runtime[UnaryInvocation]
    ) -> dict[str, Payload]:
        context = runtime.context
        if command == "graph_query":
            handler = context.graph_query
        else:
            from . import commands

            handler = getattr(commands, f"handle_{command}")
        try:
            result = (
                handler(context.config, state["request"])
                if takes_request
                else handler(context.config)
            )
        except Exception as exc:
            raise _HandlerFailure(exc) from None
        return {"result": result}

    return execute


@lru_cache(maxsize=1)
def get_unary_graph() -> CompiledStateGraph:
    """Lazily compile an allow-listed, single-handler graph without I/O."""

    from langgraph.graph import END, START, StateGraph

    graph = StateGraph(UnaryState, context_schema=UnaryInvocation)
    for command, takes_request in UNARY_COMMANDS.items():
        graph.add_node(command, _handler_node(command, takes_request))
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
