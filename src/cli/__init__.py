"""JSON stdin/stdout CLI bridge invoked by the Rust/Tauri backend.

The bridge exposes the reusable ``src`` RAG modules to the desktop app without
running an HTTP server: ``python -m src.cli`` reads one JSON request and writes
one JSON response; ``commands.dispatch`` routes unary commands. Nothing here
performs work on import.
"""

from __future__ import annotations

from .config import CLIConfig

__all__ = ["CLIConfig"]
