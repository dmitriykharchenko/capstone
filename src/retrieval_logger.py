"""Append-only text logging of retrieval operations.

Separate from :mod:`src.chat_logger`.  Each retrieval — a Papers-tab paper
search or the chat-context chunk retrieval that grounds an answer — is written
as a human-readable block to a plain-text file: a timestamp header followed by
the retrieval type, the query, the result count, and one metadata line per
retrieved document.  Logging is best-effort — an I/O failure never interrupts
retrieval.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

_SEPARATOR = "=" * 72


def _format_result(rank: int, result: Mapping[str, Any]) -> str:
    """Render one retrieved-document record as a single metadata line.

    ``rank`` falls back to the enumeration position when the record does not
    carry its own (paper-search matches are pre-ranked but omit the field).
    """

    parts = [
        f"paper_id={result.get('paper_id')}",
        f"file={result.get('filename')}",
        f"page={result.get('page')}",
    ]
    if result.get("chunk_index") is not None:
        parts.append(f"chunk={result.get('chunk_index')}")
    parts.append(f"score={result.get('score')}")
    position = result.get("rank", rank)
    return f"  [{position}] {result.get('title')} ({', '.join(parts)})"


def format_retrieval_entry(
    *,
    retrieval_type: str,
    query: str,
    results: Sequence[Mapping[str, Any]],
    context: str | None = None,
    timestamp: str | None = None,
) -> str:
    """Return the text block logged for one retrieval operation.

    When ``context`` is provided it is appended verbatim as the full text that
    will be handed to the model — the complete retrieved evidence, not just the
    per-document metadata summary above it.
    """

    stamp = timestamp or datetime.now(timezone.utc).isoformat(timespec="seconds")
    lines = [
        _SEPARATOR,
        f"timestamp: {stamp}",
        f"retrieval_type: {retrieval_type}",
        f"query: {query}",
        f"retrieved_documents: {len(results)}",
    ]
    lines.extend(
        _format_result(rank, result) for rank, result in enumerate(results, start=1)
    )
    if context is not None:
        lines.append("context_provided_to_model:")
        lines.append(context)
    lines.append("")
    return "\n".join(lines)


def log_retrieval(
    log_path: Path,
    *,
    retrieval_type: str,
    query: str,
    results: Sequence[Mapping[str, Any]],
    context: str | None = None,
) -> bool:
    """Append one retrieval operation to ``log_path``; return ``True`` on success.

    Best-effort: the parent directory is created if needed and any I/O error is
    swallowed so a logging failure never breaks retrieval itself.
    """

    try:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        entry = format_retrieval_entry(
            retrieval_type=retrieval_type,
            query=query,
            results=results,
            context=context,
        )
        with open(log_path, "a", encoding="utf-8") as handle:
            handle.write(entry + "\n")
        return True
    except OSError:
        return False


__all__ = ["log_retrieval", "format_retrieval_entry"]
