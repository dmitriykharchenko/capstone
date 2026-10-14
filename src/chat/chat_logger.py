"""Append-only text logging of chat turns.

Only chats are logged.  Each completed turn is written as a human-readable block
to a plain-text file: a timestamp header followed by the session, the model
name, the chat mode, the user message, the assistant response, and metadata for
every retrieved document that grounded the answer.  Logging is best-effort — an
I/O failure never interrupts a chat.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

_SEPARATOR = "=" * 72


def _format_source(source: Mapping[str, Any]) -> str:
    """Render one retrieved-document record as a single metadata line."""

    return (
        f"  [{source.get('rank')}] {source.get('title')} "
        f"(paper_id={source.get('paper_id')}, file={source.get('filename')}, "
        f"page={source.get('page')}, chunk={source.get('chunk_index')}, "
        f"score={source.get('score')})"
    )


def format_chat_entry(
    *,
    session_id: str,
    model: str,
    chat_mode: str,
    message: str,
    response: str,
    sources: Sequence[Mapping[str, Any]],
    timestamp: str | None = None,
) -> str:
    """Return the text block logged for one chat turn."""

    stamp = timestamp or datetime.now(timezone.utc).isoformat(timespec="seconds")
    lines = [
        _SEPARATOR,
        f"timestamp: {stamp}",
        f"session_id: {session_id}",
        f"model: {model}",
        f"chat_mode: {chat_mode}",
        f"message: {message}",
        f"response: {response}",
        f"retrieved_documents: {len(sources)}",
    ]
    lines.extend(_format_source(source) for source in sources)
    lines.append("")
    return "\n".join(lines)


def log_chat(
    log_path: Path,
    *,
    session_id: str,
    model: str,
    chat_mode: str,
    message: str,
    response: str,
    sources: Sequence[Mapping[str, Any]],
) -> bool:
    """Append one chat turn to ``log_path``; return ``True`` on success.

    Best-effort: the parent directory is created if needed and any I/O error is
    swallowed so a logging failure never breaks the chat itself.
    """

    try:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        entry = format_chat_entry(
            session_id=session_id,
            model=model,
            chat_mode=chat_mode,
            message=message,
            response=response,
            sources=sources,
        )
        with open(log_path, "a", encoding="utf-8") as handle:
            handle.write(entry + "\n")
        return True
    except OSError:
        return False


__all__ = ["log_chat", "format_chat_entry"]
