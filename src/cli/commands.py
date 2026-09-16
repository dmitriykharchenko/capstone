"""Command handlers for the CLI bridge.

Each handler takes the parsed request and returns a JSON-serializable dict.
Chat routes through the agent loop (persisting to SQLite and best-effort
indexing to Chroma); sessions/messages port the read logic from the retired
REST server; search queries the Chroma-backed chat-history index.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Mapping
from typing import Any, Iterator

from ..agent_loop import (
    AgentIncompleteResponseError,
    MultihopCompletedEvent,
    MultihopFinalDeltaEvent,
    MultihopHopCompletedEvent,
    MultihopHopStartedEvent,
    MultihopPlanEvent,
)
from ..chat_logger import log_chat
from ..evaluation_runner import JUDGE_CRITERIA, validate_test_cases
from ..paper_search import EVALUATION_RETRIEVAL_STRATEGIES
from ..retrieval_logger import log_retrieval
from ..sqlite_history import SQLiteHistoryStore
from .config import CLIConfig
from .models import (
    build_agent_loop,
    build_evaluation_runner,
    build_history_store,
    build_index,
    build_paper_indexer,
    build_paper_search_index,
    chromadb_available,
)


def handle_health(config: CLIConfig) -> dict[str, Any]:
    """Report chat mode, search availability, and resolved data locations."""

    return {
        "ok": True,
        "chat_mode": "real" if config.has_api_key else "mock",
        "search_enabled": chromadb_available(),
        "database_path": str(config.database_path),
        "papers_directory": str(config.papers_directory),
        "chroma_directory": str(config.chroma_directory),
    }


def handle_chat(config: CLIConfig, request: dict[str, Any]) -> dict[str, Any]:
    """Run one chat turn for a session and return the assistant reply."""

    session_id = _require_str(request.get("session_id"), "session_id")
    message = _require_str(request.get("message"), "message")
    store = build_history_store(config)
    try:
        index = build_index(config, store)
        paper_retriever = build_paper_search_index(config)
        loop = build_agent_loop(config, store, index, paper_retriever)
        reply = loop.run(session_id, message)
        sources = [_source_payload(source) for source in loop.last_sources]
        context = loop.last_context
    finally:
        store.close()
    chat_mode = "real" if config.has_api_key else "mock"
    log_retrieval(
        config.retrieval_log_path,
        retrieval_type="chat_context",
        query=message,
        results=sources,
        context=context,
    )
    log_chat(
        config.log_path,
        session_id=session_id,
        model=config.chat_model,
        chat_mode=chat_mode,
        message=message,
        response=reply,
        sources=sources,
    )
    return {
        "session_id": session_id,
        "reply": reply,
        "sources": sources,
        "chat_mode": chat_mode,
        "indexed": index is not None,
    }


def handle_chat_stream(
    config: CLIConfig, request: dict[str, Any]
) -> Iterator[dict[str, Any]]:
    """Stream public multihop progress followed by authoritative completion.

    Each yielded dict is emitted as its own JSON line by the bridge entrypoint.
    The terminating event comes from the core's post-persistence completion
    record, so callers must reconcile streamed deltas to its reply and sources.
    """

    store = None
    try:
        session_id = _require_str(request.get("session_id"), "session_id")
        message = _require_str(request.get("message"), "message")
        store = build_history_store(config)
        index = build_index(config, store)
        paper_retriever = build_paper_search_index(config)
        loop = build_agent_loop(config, store, index, paper_retriever)
        completed = None
        for progress in _chat_progress_events(loop, session_id, message):
            if isinstance(progress, MultihopCompletedEvent):
                completed = progress
                break
            yield progress if isinstance(progress, dict) else _chat_progress_payload(progress)
        if completed is None:
            raise RuntimeError("chat stream ended before terminal completion")
        reply = completed.answer
        sources = [_source_payload(source) for source in completed.sources]
        context = loop.last_context
        chat_mode = "real" if config.has_api_key else "mock"
        log_retrieval(
            config.retrieval_log_path,
            retrieval_type="chat_context",
            query=message,
            results=sources,
            context=context,
        )
        log_chat(
            config.log_path,
            session_id=session_id,
            model=config.chat_model,
            chat_mode=chat_mode,
            message=message,
            response=reply,
            sources=sources,
        )
        yield {
            "event": "done",
            "ok": True,
            "done": True,
            "session_id": session_id,
            "reply": reply,
            "sources": sources,
            "chat_mode": chat_mode,
            "indexed": index is not None,
        }
    except AgentIncompleteResponseError:
        yield {
            "event": "error",
            "ok": False,
            "error": "Chat response was incomplete; shorten the request and try again",
        }
    except Exception:  # noqa: BLE001 - transport errors must not expose internals
        yield {
            "event": "error",
            "ok": False,
            "error": "Chat failed. Check backend logs and retry.",
        }
    finally:
        if store is not None:
            store.close()


def _chat_progress_events(loop: Any, session_id: str, message: str) -> Iterator[Any]:
    stream_events = getattr(loop, "stream_events", None)
    if callable(stream_events):
        yield from stream_events(session_id, message)
        return

    parts: list[str] = []
    for delta in loop.stream(session_id, message):
        parts.append(delta)
        yield {"delta": delta}
    yield MultihopCompletedEvent("".join(parts), tuple(loop.last_sources))


def _chat_progress_payload(progress: Any) -> dict[str, Any]:
    if isinstance(progress, MultihopPlanEvent):
        return {
            "event": "plan",
            "subqueries": [
                {"id": _subquery_id(index), "index": index, "query": query}
                for index, query in enumerate(progress.subqueries, start=1)
            ],
        }
    if isinstance(progress, MultihopHopStartedEvent):
        return {
            "event": "subquery",
            "id": _subquery_id(progress.index),
            "index": progress.index,
            "total": progress.total,
            "query": progress.subquery,
        }
    if isinstance(progress, MultihopHopCompletedEvent):
        return {
            "event": "subanswer",
            "id": _subquery_id(progress.index),
            "index": progress.index,
            "total": progress.total,
            "query": progress.subquery,
            "answer": progress.answer,
            "sources": [_source_payload(source) for source in progress.sources],
        }
    if isinstance(progress, MultihopFinalDeltaEvent):
        return {"event": "delta", "delta": progress.text}
    raise TypeError("unsupported chat progress event")


def _subquery_id(index: int) -> str:
    return f"subquery-{index}"


def handle_sessions(config: CLIConfig) -> dict[str, Any]:
    """Return every known session with a title, message and exchange counts.

    The ``title`` is derived from the first few words of the session's first
    user message so the UI can label sessions meaningfully instead of showing
    the opaque session identifier.
    """

    path = config.database_path
    if not path.exists():
        return {"sessions": []}
    connection = sqlite3.connect(str(path))
    try:
        if not _table_exists(connection, "messages"):
            return {"sessions": []}
        message_counts = dict(
            connection.execute(
                "SELECT session_id, COUNT(*) FROM messages GROUP BY session_id"
            ).fetchall()
        )
        first_user_messages = dict(
            connection.execute(
                "SELECT session_id, content FROM messages WHERE id IN ("
                "SELECT MIN(id) FROM messages WHERE role = 'user' GROUP BY session_id"
                ")"
            ).fetchall()
        )
        exchange_counts: dict[str, int] = {}
        if _table_exists(connection, "exchanges"):
            exchange_counts = dict(
                connection.execute(
                    "SELECT session_id, COUNT(*) FROM exchanges GROUP BY session_id"
                ).fetchall()
            )
    finally:
        connection.close()
    sessions = [
        {
            "session_id": session_id,
            "title": _session_title(first_user_messages.get(session_id)),
            "message_count": int(count),
            "exchange_count": int(exchange_counts.get(session_id, 0)),
        }
        for session_id, count in sorted(message_counts.items())
    ]
    return {"sessions": sessions}


def handle_messages(config: CLIConfig, request: dict[str, Any]) -> dict[str, Any]:
    """Return the ordered transcript for one session (empty if unknown)."""

    session_id = _require_str(request.get("session_id"), "session_id")
    path = config.database_path
    if not path.exists():
        return {"session_id": session_id, "messages": []}
    with SQLiteHistoryStore(path) as store:
        records = store.get_messages(session_id)
    return {
        "session_id": session_id,
        "messages": [_message_payload(record) for record in records],
    }


def handle_search(config: CLIConfig, request: dict[str, Any]) -> dict[str, Any]:
    """Return chat-history exchanges ranked by semantic similarity."""

    query = _require_str(request.get("query"), "query")
    limit = request.get("limit", 5)
    if isinstance(limit, bool) or not isinstance(limit, int) or limit < 1:
        limit = 5
    session_id = request.get("session_id")
    if session_id is not None and (
        not isinstance(session_id, str) or not session_id.strip()
    ):
        session_id = None
    store = build_history_store(config)
    try:
        index = build_index(config, store)
        if index is None:
            return {
                "enabled": False,
                "matches": [],
                "reason": "chromadb not installed or index unavailable",
            }
        records = index.search(query, n_results=limit, session_id=session_id)
        matches = [
            {
                "exchange_id": record.exchange_id,
                "session_id": record.session_id,
                "user_message": record.user_message.content,
                "assistant_message": record.assistant_message.content,
            }
            for record in records
        ]
    finally:
        store.close()
    return {"enabled": True, "matches": matches}


def handle_index_papers(config: CLIConfig) -> dict[str, Any]:
    """Index (or replace) every PDF in the papers directory into Chroma."""

    if not chromadb_available():
        return {
            "enabled": False,
            "indexed": 0,
            "papers": [],
            "reason": "chromadb not installed or index unavailable",
        }
    indexer = build_paper_indexer(config)
    if indexer is None:
        return {
            "enabled": False,
            "indexed": 0,
            "papers": [],
            "reason": "paper indexer unavailable",
        }
    try:
        report = indexer.index_directory()
    except NotADirectoryError as exc:
        return {"enabled": True, "indexed": 0, "papers": [], "reason": str(exc)}
    papers = [
        {
            "paper_id": outcome.paper_id,
            "filename": outcome.filename,
            "status": outcome.status,
            "chunk_count": outcome.chunk_count,
            "error": outcome.error,
        }
        for outcome in report.outcomes
    ]
    indexed = sum(1 for outcome in report.outcomes if outcome.status == "indexed")
    return {"enabled": True, "indexed": indexed, "papers": papers}


def handle_index_papers_stream(config: CLIConfig) -> Iterator[dict[str, Any]]:
    """Stream determinate progress while indexing the configured PDF directory."""

    indexer = build_paper_indexer(config)
    if indexer is None:
        raise RuntimeError("paper indexer unavailable")

    counts = {"indexed": 0, "empty": 0, "failed": 0}
    total = 0
    completed = 0
    for progress in indexer.iter_directory():
        total = progress.total
        completed = progress.completed
        outcome = progress.outcome
        if outcome is None:
            yield {"event": "start", "completed": 0, "total": total}
            continue
        counts[outcome.status] += 1
        yield {
            "event": "progress",
            "completed": completed,
            "total": total,
            "paper_id": outcome.paper_id,
            "filename": outcome.filename,
            "status": outcome.status,
            "chunk_count": outcome.chunk_count,
            "error": outcome.error,
        }
    yield {
        "ok": True,
        "event": "done",
        "completed": completed,
        "total": total,
        **counts,
    }


def handle_evaluation_stream(
    config: CLIConfig,
    request: dict[str, Any],
    runner: Any | None = None,
) -> Iterator[dict[str, Any]]:
    """Run validated evaluation cases sequentially and stream ordered events."""

    retrieval_type = _require_str(request.get("retrieval_type"), "retrieval_type")
    if retrieval_type not in EVALUATION_RETRIEVAL_STRATEGIES:
        allowed = ", ".join(EVALUATION_RETRIEVAL_STRATEGIES)
        raise ValueError(f"retrieval_type must be one of: {allowed}")
    cases = validate_test_cases(request.get("test_cases"))
    if runner is not None:
        evaluator = runner
    else:
        try:
            evaluator = build_evaluation_runner(config)
        except Exception as exc:  # noqa: BLE001 - never expose backend details
            raise RuntimeError(
                "Evaluation backend is unavailable. Check backend configuration and retry."
            ) from exc
    total = len(cases)
    succeeded = 0
    failed = 0
    score_totals = {criterion: 0.0 for criterion in JUDGE_CRITERIA}

    yield {
        "event": "run_start",
        "retrieval_type": retrieval_type,
        "total": total,
    }
    for case_index, case in enumerate(cases, start=1):
        case_input = case.as_dict()
        yield {
            "event": "case_start",
            "case_index": case_index,
            "total": total,
            "retrieval_type": retrieval_type,
            "input": case_input,
        }
        try:
            result = evaluator.run([case_input], retrieval_type)[0]
            result = {**result, "case_index": case_index}
        except Exception:  # noqa: BLE001 - one case must not abort the run
            result = {
                "case_index": case_index,
                "input": case_input,
                "retrieval_strategy": retrieval_type,
                "retrieved_chunks": [],
                "generated_answer": None,
                "judge_scores": None,
                "error": {
                    "stage": "evaluation",
                    "message": _evaluation_error_message("evaluation"),
                },
            }
        result = _sanitize_evaluation_result(result)
        completed = case_index
        scores = result.get("judge_scores")
        if result.get("error") is None and isinstance(scores, Mapping):
            succeeded += 1
            for criterion in JUDGE_CRITERIA:
                score_totals[criterion] += float(scores[criterion]["score"])
            event = "case_success"
        else:
            failed += 1
            event = "case_failure"
        yield {
            "event": event,
            "completed": completed,
            "total": total,
            "result": result,
        }

    aggregates = {
        criterion: (score_totals[criterion] / succeeded if succeeded else None)
        for criterion in JUDGE_CRITERIA
    }
    yield {
        "event": "complete",
        "retrieval_type": retrieval_type,
        "completed": total,
        "total": total,
        "succeeded": succeeded,
        "failed": failed,
        "aggregate_scores": aggregates,
    }


def _sanitize_evaluation_result(result: Mapping[str, Any]) -> dict[str, Any]:
    """Replace backend exception details with a fixed stage-aware UI message."""

    sanitized = dict(result)
    error = result.get("error")
    if error is not None:
        stage = error.get("stage") if isinstance(error, Mapping) else None
        safe_stage = stage if stage in {"retrieval", "model", "judge"} else "evaluation"
        sanitized["error"] = {
            "stage": safe_stage,
            "message": _evaluation_error_message(safe_stage),
        }
    return sanitized


def _evaluation_error_message(stage: str) -> str:
    return {
        "retrieval": "Retrieval failed for this case. Check the paper index and retry.",
        "model": "Answer generation failed for this case. Check backend configuration and retry.",
        "judge": "Judge evaluation failed for this case. Check backend configuration and retry.",
        "evaluation": "Evaluation failed for this case. Check backend configuration and retry.",
    }[stage]


def handle_search_papers(config: CLIConfig, request: dict[str, Any]) -> dict[str, Any]:
    """Return indexed papers ranked by semantic similarity to a query.

    When the papers collection is empty (nothing indexed yet), the directory is
    indexed lazily on first use so search works without a manual step.
    """

    query = _require_str(request.get("query"), "query")
    limit = request.get("limit", 5)
    if isinstance(limit, bool) or not isinstance(limit, int) or limit < 1:
        limit = 5
    if not chromadb_available():
        return {
            "enabled": False,
            "matches": [],
            "reason": "chromadb not installed or index unavailable",
        }
    index = build_paper_search_index(config)
    if index is None:
        return {
            "enabled": False,
            "matches": [],
            "reason": "paper search index unavailable",
        }
    if index.count() == 0:
        return {
            "enabled": True,
            "matches": [],
            "reason": "no papers indexed yet; run indexing first",
        }
    try:
        matches = index.search(query, n_results=limit)
    except Exception as exc:  # noqa: BLE001 - surfaced to the caller as a reason
        return {"enabled": True, "matches": [], "reason": str(exc)}
    match_payloads = [
        {
            "paper_id": match.paper_id,
            "title": match.title,
            "filename": match.filename,
            "page": match.page,
            "snippet": match.snippet,
            "score": match.score,
        }
        for match in matches
    ]
    log_retrieval(
        config.retrieval_log_path,
        retrieval_type="paper_search",
        query=query,
        results=match_payloads,
    )
    return {"enabled": True, "matches": match_payloads}


def dispatch(command: Any, config: CLIConfig, request: dict[str, Any]) -> dict[str, Any]:
    """Route ``command`` to its handler, raising ``ValueError`` when unknown."""

    handlers = {
        "health": lambda: handle_health(config),
        "chat": lambda: handle_chat(config, request),
        "sessions": lambda: handle_sessions(config),
        "messages": lambda: handle_messages(config, request),
        "search": lambda: handle_search(config, request),
        "search_papers": lambda: handle_search_papers(config, request),
        "index_papers": lambda: handle_index_papers(config),
    }
    handler = handlers.get(command)
    if handler is None:
        raise ValueError(f"unknown command {command!r}")
    return handler()


def _require_str(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{name} is required and must be a non-empty string")
    return value


def _source_payload(source: Any) -> dict[str, Any]:
    return {
        "rank": source.rank,
        "paper_id": source.paper_id,
        "chunk_index": source.chunk_index,
        "filename": source.filename,
        "title": source.title,
        "page": source.page,
        "score": source.score,
        "snippet": source.snippet,
    }


def _message_payload(record: Any) -> dict[str, Any]:
    payload = {"role": record.role, "content": record.content}
    if record.sources:
        payload["sources"] = [_source_payload(source) for source in record.sources]
    return payload


def _table_exists(connection: sqlite3.Connection, name: str) -> bool:
    row = connection.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", (name,)
    ).fetchone()
    return row is not None


SESSION_TITLE_WORD_LIMIT = 6


def _session_title(content: Any, *, word_limit: int = SESSION_TITLE_WORD_LIMIT) -> str:
    """Derive a short session label from the first few words of ``content``."""

    if not isinstance(content, str):
        return ""
    words = content.split()
    if not words:
        return ""
    title = " ".join(words[:word_limit])
    if len(words) > word_limit:
        title += "\u2026"
    return title


__all__ = [
    "dispatch",
    "handle_chat",
    "handle_chat_stream",
    "handle_evaluation_stream",
    "handle_health",
    "handle_index_papers",
    "handle_index_papers_stream",
    "handle_messages",
    "handle_search",
    "handle_search_papers",
    "handle_sessions",
]
