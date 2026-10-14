"""Command handlers for the CLI bridge.

Each handler takes the parsed request and returns a JSON-serializable dict.
Chat routes through the agent loop (persisting to SQLite and best-effort
indexing to Chroma); sessions/messages port the read logic from the retired
REST server; search queries the Chroma-backed chat-history index.
"""

from __future__ import annotations

import json
import sqlite3
from collections.abc import Mapping
from contextlib import closing
from typing import Any, Iterator

from ..chat.agent_loop import (
    AgentIncompleteResponseError,
    MultihopCompletedEvent,
    MultihopCoverageCompletedEvent,
    MultihopCoverageStartedEvent,
    MultihopFinalDeltaEvent,
    MultihopHopCompletedEvent,
    MultihopHopStartedEvent,
    MultihopPlanEvent,
)
from ..retrieval.bm25_index import BM25Index
from ..chat.chat_logger import log_chat
from ..storage.chroma_persistence import DEFAULT_COLLECTION_NAME, create_client
from ..retrieval.paper_search import DEFAULT_BM25_INDEX_FILENAME
from ..chat.retrieval_logger import log_retrieval
from ..storage.sqlite_history import SQLiteHistoryStore
from .config import CLIConfig
from .models import (
    build_agent_loop,
    build_evaluation_runner,
    build_history_store,
    build_index,
    build_paper_indexer,
    build_paper_search_index,
    chromadb_available,
    stop_graph_store_owner,
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
            yield (
                progress
                if isinstance(progress, dict)
                else _chat_progress_payload(progress)
            )
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
    if isinstance(progress, MultihopCoverageStartedEvent):
        return {
            "event": "coverage",
            "status": "reviewing",
            "total": progress.total,
            "subqueries": [],
        }
    if isinstance(progress, MultihopCoverageCompletedEvent):
        first_index = progress.total - len(progress.followup_subqueries) + 1
        return {
            "event": "coverage",
            "status": "followup" if progress.followup_subqueries else "complete",
            "total": progress.total,
            "subqueries": [
                {"id": _subquery_id(index), "index": index, "query": query}
                for index, query in enumerate(
                    progress.followup_subqueries, start=first_index
                )
            ],
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
        session_rows = connection.execute(
            "SELECT session_id, COUNT(*), MAX(id) FROM messages "
            "GROUP BY session_id ORDER BY MAX(id) DESC, session_id ASC"
        ).fetchall()
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
        for session_id, count, _last_message_id in session_rows
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
    store = _open_status_store(config)
    try:
        report = indexer.index_directory(_already_indexed_ids(store))
    except NotADirectoryError as exc:
        return {"enabled": True, "indexed": 0, "papers": [], "reason": str(exc)}
    finally:
        if store is not None:
            store.close()
    _refresh_bm25_index(config)
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
    """Stream determinate progress while indexing the configured PDF directory.

    Each paper's terminal outcome is persisted to SQLite as it completes so the
    frontend can restore per-paper indicators after an app restart. Papers that
    are already indexed are skipped (not re-extracted or re-embedded); they emit
    a ``"skipped"`` outcome and keep their existing persisted record.
    """

    from .stream_workflows import index_events

    stop_graph_store_owner(config)
    indexer = build_paper_indexer(config)
    if indexer is None:
        raise RuntimeError("paper indexer unavailable")

    store = _open_status_store(config)
    skip_paper_ids = _already_indexed_ids(store)
    counts = {"indexed": 0, "empty": 0, "failed": 0, "skipped": 0}
    try:
        yield from index_events(
            indexer.iter_directory(skip_paper_ids),
            counts=counts,
            started_events=True,
            persist=lambda outcome: _persist_paper_status(store, outcome),
            refresh=lambda: _refresh_bm25_index(config),
        )
    finally:
        if store is not None:
            store.close()


def handle_paper_statuses(config: CLIConfig) -> dict[str, Any]:
    """Return persisted per-paper indexing outcomes for frontend hydration."""

    path = config.database_path
    if not path.exists():
        return {"statuses": []}
    with SQLiteHistoryStore(path) as store:
        records = store.get_paper_statuses()
    return {
        "statuses": [
            {
                "paper_id": record.paper_id,
                "filename": record.filename,
                "status": record.status,
                "chunk_count": record.chunk_count,
                "error": record.error,
                "updated_at": record.updated_at,
            }
            for record in records
        ]
    }


def handle_bm25_status(config: CLIConfig) -> dict[str, Any]:
    """Report which papers the persisted BM25 lexical index currently covers.

    The frontend renders a per-paper BM25 indicator from ``paper_ids`` next to
    the vector-index dot. Degrades to an empty, unbuilt result when the index
    file is absent or unreadable so the indicators simply read "not indexed".
    """

    index = BM25Index.load(config.chroma_directory / DEFAULT_BM25_INDEX_FILENAME)
    if index is None:
        return {"built": False, "chunk_count": 0, "paper_ids": []}
    paper_ids = sorted(
        {
            candidate["paper_id"]
            for candidate in index.candidates
            if isinstance(candidate.get("paper_id"), str) and candidate["paper_id"]
        }
    )
    return {"built": True, "chunk_count": index.chunk_count, "paper_ids": paper_ids}


def handle_reset_all_indices(config: CLIConfig) -> dict[str, Any]:
    """Wipe every paper index: Chroma vectors, the BM25 file, and statuses.

    Each step is best-effort and independent so a failure in one still clears
    the others; the returned ``reset`` map reports what was cleared so the
    frontend can surface a partial failure.
    """

    reset = {"vector": False, "bm25": False, "statuses": False}
    stop_graph_store_owner(config)

    if chromadb_available():
        try:
            client = create_client(config.chroma_directory)
            try:
                client.delete_collection(name=DEFAULT_COLLECTION_NAME)
            except Exception:  # noqa: BLE001 - an absent collection is already reset
                pass
            reset["vector"] = True
        except Exception:  # noqa: BLE001 - client construction failed; reset best-effort
            pass

    bm25_path = config.chroma_directory / DEFAULT_BM25_INDEX_FILENAME
    try:
        bm25_path.unlink(missing_ok=True)
        reset["bm25"] = True
    except Exception:  # noqa: BLE001 - reset is best-effort
        pass

    database_path = config.database_path
    if not database_path.exists():
        reset["statuses"] = True
    else:
        try:
            with SQLiteHistoryStore(database_path) as store:
                store.clear_paper_statuses()
            reset["statuses"] = True
        except Exception:  # noqa: BLE001 - reset is best-effort
            pass

    return {"ok": True, "reset": reset}


def _open_status_store(config: CLIConfig) -> SQLiteHistoryStore | None:
    """Open the SQLite store for status persistence, degrading to ``None``."""

    try:
        return build_history_store(config)
    except Exception:  # noqa: BLE001 - status persistence is best-effort
        return None


def _refresh_bm25_index(config: CLIConfig) -> None:
    """Rebuild the persisted BM25 index after indexing, best-effort.

    Runs once the corpus is fully written so the lexical index reflects every
    chunk (including papers skipped as already indexed). Any failure leaves the
    query path to fall back to on-the-fly BM25, so this never breaks indexing.
    """

    if not chromadb_available():
        return
    try:
        index = build_paper_search_index(config)
        if index is not None:
            index.refresh_bm25_index()
    except Exception:  # noqa: BLE001 - BM25 index refresh is best-effort
        pass


def _already_indexed_ids(store: SQLiteHistoryStore | None) -> set[str]:
    """Return paper ids previously recorded as ``"indexed"`` for skip decisions.

    Only successfully indexed papers are skipped; ``empty`` and ``failed``
    outcomes are re-attempted on the next run. Degrades to an empty set so a
    read failure simply re-indexes everything.
    """

    if store is None:
        return set()
    try:
        return {
            record.paper_id
            for record in store.get_paper_statuses()
            if record.status == "indexed"
        }
    except Exception:  # noqa: BLE001 - skip optimization is best-effort
        return set()


def _persist_paper_status(store: SQLiteHistoryStore | None, outcome: Any) -> None:
    """Best-effort persist one paper's terminal indexing outcome."""

    if store is None:
        return
    try:
        store.record_paper_status(
            outcome.paper_id,
            outcome.filename,
            outcome.status,
            chunk_count=outcome.chunk_count,
            error=outcome.error,
        )
    except Exception:  # noqa: BLE001 - status persistence is best-effort
        pass


def handle_evaluation_stream(
    config: CLIConfig,
    request: dict[str, Any],
    runner: Any | None = None,
) -> Iterator[dict[str, Any]]:
    """Run validated evaluation cases sequentially and stream ordered events."""

    from .stream_workflows import evaluation_events

    yield from evaluation_events(config, request, runner, build_evaluation_runner)


def _sanitize_evaluation_result(result: Mapping[str, Any]) -> dict[str, Any]:
    """Rebuild failure diagnostics before they reach stream state or the wire."""
    from ..evaluation.diagnostics import sanitize_result_diagnostics

    return sanitize_result_diagnostics(dict(result))


def _evaluation_error_message(stage: str) -> str:
    return {
        "planner": "Question planning failed for this case. Check backend configuration and retry.",
        "retrieval": "Retrieval failed for this case. Check the paper index and retry.",
        "subanswer": "Subanswer generation failed for this case. Check backend configuration and retry.",
        "coverage": "Coverage review failed for this case. Check backend configuration and retry.",
        "followup_retrieval": "Follow-up retrieval failed for this case. Check the paper index and retry.",
        "followup_subanswer": "Follow-up subanswer generation failed for this case. Check backend configuration and retry.",
        "composer": "Final answer composition failed for this case. Check backend configuration and retry.",
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


def handle_paper_metadata(config: CLIConfig, request: dict[str, Any]) -> dict[str, Any]:
    """Read already-indexed paper metadata without importing optional Chroma."""

    paper_ids = _require_paper_ids(request.get("paper_ids"))
    if not paper_ids:
        return {"available": True, "papers": []}
    database = config.chroma_directory / "chroma.sqlite3"
    if not database.is_file():
        return {
            "available": False,
            "papers": [],
            "reason": "indexed paper metadata unavailable",
        }
    placeholders = ",".join("?" for _ in paper_ids)
    try:
        with closing(
            sqlite3.connect(f"{database.resolve().as_uri()}?mode=ro", uri=True)
        ) as connection:
            rows = connection.execute(
                "SELECT paper.string_value, title.string_value, "
                "authors.string_value, topics.string_value "
                "FROM embedding_metadata AS paper "
                "JOIN embedding_metadata AS title ON title.id = paper.id AND title.key = 'title' "
                "JOIN embedding_metadata AS authors ON authors.id = paper.id AND authors.key = 'authors' "
                "JOIN embedding_metadata AS topics ON topics.id = paper.id AND topics.key = 'topics' "
                f"WHERE paper.key = 'paper_id' AND paper.string_value IN ({placeholders}) "
                "ORDER BY paper.id",
                paper_ids,
            )
            metadata = _unique_paper_metadata(rows)
    except sqlite3.Error:
        return {
            "available": False,
            "papers": [],
            "reason": "indexed paper metadata unavailable",
        }
    return {
        "available": True,
        "papers": [metadata[paper_id] for paper_id in paper_ids if paper_id in metadata],
    }


def _require_paper_ids(value: Any) -> tuple[str, ...]:
    if not isinstance(value, list) or len(value) > 4096:
        raise ValueError("paper_ids must be an array of at most 4096 identifiers")
    result: list[str] = []
    seen: set[str] = set()
    for item in value:
        if not isinstance(item, str) or not item.strip():
            raise ValueError("paper_ids must contain non-empty strings")
        if item not in seen:
            result.append(item)
            seen.add(item)
    return tuple(result)


def _unique_paper_metadata(rows: Any) -> dict[str, dict[str, Any]]:
    result: dict[str, dict[str, Any]] = {}
    conflicts: set[str] = set()
    for paper_id, title, authors_json, topics_json in rows:
        try:
            authors = _metadata_names(authors_json)
            topics = _metadata_names(topics_json)
        except (TypeError, ValueError, json.JSONDecodeError):
            conflicts.add(paper_id)
            continue
        if not isinstance(title, str) or not title.strip():
            conflicts.add(paper_id)
            continue
        value = {
            "paper_id": paper_id,
            "title": title.strip(),
            "authors": authors,
            "topics": topics,
        }
        if paper_id in result and result[paper_id] != value:
            conflicts.add(paper_id)
        else:
            result[paper_id] = value
    for paper_id in conflicts:
        result.pop(paper_id, None)
    return result


def _metadata_names(value: Any) -> list[str]:
    if not isinstance(value, str):
        raise TypeError("metadata must be JSON text")
    decoded = json.loads(value)
    if not isinstance(decoded, list) or not decoded:
        raise ValueError("metadata must be a non-empty array")
    names: list[str] = []
    for item in decoded:
        if not isinstance(item, str) or not item.strip():
            raise ValueError("metadata names must be non-empty strings")
        normalized = item.strip()
        if normalized not in names:
            names.append(normalized)
    return names


def dispatch(
    command: Any, config: CLIConfig, request: dict[str, Any]
) -> dict[str, Any]:
    """Route ``command`` to its handler, raising ``ValueError`` when unknown."""

    handlers = {
        "health": lambda: handle_health(config),
        "chat": lambda: handle_chat(config, request),
        "sessions": lambda: handle_sessions(config),
        "messages": lambda: handle_messages(config, request),
        "search": lambda: handle_search(config, request),
        "search_papers": lambda: handle_search_papers(config, request),
        "paper_metadata": lambda: handle_paper_metadata(config, request),
        "index_papers": lambda: handle_index_papers(config),
        "paper_statuses": lambda: handle_paper_statuses(config),
        "bm25_status": lambda: handle_bm25_status(config),
        "reset_all_indices": lambda: handle_reset_all_indices(config),
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
    if record.exchange_id is not None:
        payload["exchange_id"] = record.exchange_id
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
    "handle_paper_statuses",
    "handle_search",
    "handle_search_papers",
    "handle_sessions",
]
