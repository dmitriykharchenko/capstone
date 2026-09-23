"""Top-level command routing, including optional graph reads."""

from __future__ import annotations

from typing import Any

from .config import CLIConfig
from .graph_queries import handle_graph_query
from .runtime import run_command


def dispatch(
    command: Any, config: CLIConfig, request: dict[str, Any]
) -> dict[str, Any]:
    """Route one CLI command without changing established command handlers."""

    def execute(command: Any, request: dict[str, Any]) -> dict[str, Any]:
        if command == "graph_query":
            return handle_graph_query(config, request)
        from .commands import dispatch as dispatch_legacy

        return dispatch_legacy(command, config, request)

    return run_command(command, request, execute)


__all__ = ["dispatch"]
