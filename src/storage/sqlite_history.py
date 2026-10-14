"""Persistent, provider-neutral conversation history and source snapshots.

SQLite remains the canonical store for ordered messages, completed exchanges,
and compact retrieved-paper metadata associated with assistant responses.
"""

from __future__ import annotations

import math
import sqlite3
from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

DatabasePath = str | Path
MAX_EXCHANGE_SOURCE_COUNT = 35
MAX_SOURCE_SNIPPET_LENGTH = 280

_VALID_ROLES = frozenset(
    {"system", "user", "assistant", "human", "ai", "tool", "function", "developer"}
)


@dataclass(frozen=True, slots=True)
class SourceRecord:
    """A compact retrieved-paper snapshot associated with one exchange."""

    rank: int
    paper_id: str
    chunk_index: int
    filename: str
    title: str
    page: int
    score: float
    snippet: str


@dataclass(frozen=True, slots=True)
class MessageRecord:
    """A provider-neutral message returned by a history store.

    ``role`` uses a conventional chat role (for example ``"user"`` or
    ``"assistant"``), while ``content`` is the message text exactly as supplied
    by the caller.
    """

    role: str
    content: str
    sources: tuple[SourceRecord, ...] = ()
    exchange_id: int | None = field(default=None, compare=False)


@dataclass(frozen=True, slots=True)
class ExchangeRecord:
    """A completed user/assistant exchange stored for one session."""

    exchange_id: int
    session_id: str
    user_message: MessageRecord
    assistant_message: MessageRecord


@dataclass(frozen=True, slots=True)
class PaperStatusRecord:
    """The last recorded indexing outcome for one paper.

    ``status`` mirrors the indexer's terminal outcomes (``"indexed"``,
    ``"empty"``, or ``"failed"``) and ``updated_at`` is an ISO-8601 UTC
    timestamp so the frontend can hydrate indicators after a restart.
    """

    paper_id: str
    filename: str
    status: str
    chunk_count: int
    error: str | None
    updated_at: str


class SQLiteHistoryStore:
    """Append and read ordered conversation messages in a SQLite database.

    Args:
        database_path: Explicit path to the SQLite database file.  Missing
            parent directories are created automatically.

    The store owns one SQLite connection.  Call :meth:`close` when finished,
    or use the store as a context manager.  Every append is committed before
    the method returns, making messages available to a newly created store.
    """

    def __init__(self, database_path: DatabasePath) -> None:
        self._database_path = _validate_database_path(database_path)
        self._database_path.parent.mkdir(parents=True, exist_ok=True)
        self._connection = sqlite3.connect(self._database_path)
        self._connection.execute("PRAGMA foreign_keys = ON")
        self._initialize_schema()

    def _initialize_schema(self) -> None:
        """Create the current schema and migrate legacy message-only databases."""

        with self._connection:
            self._connection.execute(
                """
                CREATE TABLE IF NOT EXISTS exchanges (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL
                )
                """
            )
            self._connection.execute(
                """
                CREATE TABLE IF NOT EXISTS messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    exchange_id INTEGER REFERENCES exchanges(id) ON DELETE CASCADE
                )
                """
            )
            columns = {
                row[1]
                for row in self._connection.execute("PRAGMA table_info(messages)")
            }
            if "exchange_id" not in columns:
                self._connection.execute(
                    "ALTER TABLE messages ADD COLUMN exchange_id INTEGER "
                    "REFERENCES exchanges(id) ON DELETE CASCADE"
                )
            self._connection.execute(
                "CREATE INDEX IF NOT EXISTS idx_messages_session_id "
                "ON messages (session_id, id)"
            )
            self._connection.execute(
                "CREATE INDEX IF NOT EXISTS idx_messages_exchange_id "
                "ON messages (exchange_id, id)"
            )
            self._connection.execute(
                """
                CREATE TABLE IF NOT EXISTS exchange_sources (
                    exchange_id INTEGER NOT NULL
                        REFERENCES exchanges(id) ON DELETE CASCADE,
                    rank INTEGER NOT NULL,
                    paper_id TEXT NOT NULL,
                    chunk_index INTEGER NOT NULL,
                    filename TEXT NOT NULL,
                    title TEXT NOT NULL,
                    page INTEGER NOT NULL,
                    score REAL NOT NULL,
                    snippet TEXT NOT NULL,
                    PRIMARY KEY (exchange_id, rank)
                )
                """
            )
            self._connection.execute(
                """
                CREATE TABLE IF NOT EXISTS paper_index_status (
                    paper_id TEXT PRIMARY KEY,
                    filename TEXT NOT NULL,
                    status TEXT NOT NULL,
                    chunk_count INTEGER NOT NULL DEFAULT 0,
                    error TEXT,
                    updated_at TEXT NOT NULL
                )
                """
            )

    def append_message(self, session_id: str, role: str, content: str) -> None:
        """Append one message to ``session_id``.

        Args:
            session_id: Non-empty identifier for the conversation.
            role: One of the supported conventional chat roles.
            content: Non-empty message text.

        Raises:
            TypeError: If an argument has the wrong type.
            ValueError: If an argument is empty or ``role`` is unsupported.
        """

        session = _validate_session_id(session_id)
        normalized_role = _validate_role(role)
        message = _validate_content(content)
        with self._connection:
            self._connection.execute(
                "INSERT INTO messages (session_id, role, content) VALUES (?, ?, ?)",
                (session, normalized_role, message),
            )

    def append_exchange(
        self,
        session_id: str,
        user_content: str,
        assistant_content: str,
        *,
        sources: Sequence[SourceRecord] = (),
    ) -> int:
        """Atomically store a completed turn and return its stable exchange ID.

        Both message rows and every source are committed in one transaction.
        Any failed insert rolls back the complete exchange.
        """

        session = _validate_session_id(session_id)
        user_message = _validate_content(user_content)
        assistant_message = _validate_content(assistant_content)
        source_records = _validate_sources(sources)
        with self._connection:
            cursor = self._connection.execute(
                "INSERT INTO exchanges (session_id) VALUES (?)", (session,)
            )
            exchange_id = cursor.lastrowid
            if exchange_id is None:  # pragma: no cover - guaranteed by SQLite
                raise RuntimeError("SQLite did not return an exchange ID")
            self._connection.executemany(
                "INSERT INTO messages "
                "(session_id, role, content, exchange_id) VALUES (?, ?, ?, ?)",
                [
                    (session, "user", user_message, exchange_id),
                    (session, "assistant", assistant_message, exchange_id),
                ],
            )
            self._connection.executemany(
                "INSERT INTO exchange_sources "
                "(exchange_id, rank, paper_id, chunk_index, filename, title, "
                "page, score, snippet) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    (
                        exchange_id,
                        source.rank,
                        source.paper_id,
                        source.chunk_index,
                        source.filename,
                        source.title,
                        source.page,
                        source.score,
                        source.snippet,
                    )
                    for source in source_records
                ],
            )
        return exchange_id

    def get_exchange(self, exchange_id: int, session_id: str) -> ExchangeRecord:
        """Return a completed exchange after validating its owning session.

        Raises:
            TypeError: If an identifier has the wrong type.
            ValueError: If an identifier is empty or invalid.
            KeyError: If the exchange does not exist for ``session_id``.
            RuntimeError: If stored rows do not form one complete exchange.
        """

        identity = _validate_exchange_id(exchange_id)
        session = _validate_session_id(session_id)
        owner = self._connection.execute(
            "SELECT session_id FROM exchanges WHERE id = ?", (identity,)
        ).fetchone()
        if owner is None or owner[0] != session:
            raise KeyError(f"exchange {identity} does not exist for session {session!r}")
        rows = self._connection.execute(
            "SELECT role, content FROM messages "
            "WHERE exchange_id = ? AND session_id = ? ORDER BY id ASC",
            (identity, session),
        ).fetchall()
        if len(rows) != 2 or [row[0] for row in rows] != ["user", "assistant"]:
            raise RuntimeError(f"exchange {identity} is incomplete or invalid")
        sources = self._sources_by_exchange((identity,)).get(identity, ())
        return ExchangeRecord(
            exchange_id=identity,
            session_id=session,
            user_message=MessageRecord(*rows[0]),
            assistant_message=MessageRecord(*rows[1], sources=sources),
        )

    def _sources_by_exchange(
        self, exchange_ids: Sequence[int]
    ) -> dict[int, tuple[SourceRecord, ...]]:
        identities = tuple(dict.fromkeys(exchange_ids))
        if not identities:
            return {}
        placeholders = ", ".join("?" for _ in identities)
        rows = self._connection.execute(
            "SELECT exchange_id, rank, paper_id, chunk_index, filename, title, "
            "page, score, snippet FROM exchange_sources "
            f"WHERE exchange_id IN ({placeholders}) "
            "ORDER BY exchange_id, rank",
            identities,
        ).fetchall()
        grouped: dict[int, list[SourceRecord]] = {}
        for row in rows:
            (
                exchange_id,
                rank,
                paper_id,
                chunk_index,
                filename,
                title,
                page,
                score,
                snippet,
            ) = row
            grouped.setdefault(exchange_id, []).append(
                SourceRecord(
                    rank=rank,
                    paper_id=paper_id,
                    chunk_index=chunk_index,
                    filename=filename,
                    title=title,
                    page=page,
                    score=score,
                    snippet=snippet,
                )
            )
        try:
            return {
                exchange_id: _validate_sources(sources)
                for exchange_id, sources in grouped.items()
            }
        except (TypeError, ValueError) as exc:
            raise RuntimeError("stored exchange source metadata is invalid") from exc

    def get_messages(self, session_id: str) -> list[MessageRecord]:
        """Return all messages for ``session_id`` in insertion order.

        An unknown, but valid, session identifier returns an empty list.

        Raises:
            TypeError: If ``session_id`` is not a string.
            ValueError: If ``session_id`` is empty or whitespace-only.
        """

        session = _validate_session_id(session_id)
        rows = self._connection.execute(
            "SELECT role, content, exchange_id FROM messages "
            "WHERE session_id = ? ORDER BY id ASC",
            (session,),
        ).fetchall()
        sources = self._sources_by_exchange(
            tuple(
                exchange_id
                for role, _content, exchange_id in rows
                if role == "assistant" and exchange_id is not None
            )
        )
        return [
            MessageRecord(
                role=role,
                content=content,
                sources=sources.get(exchange_id, ()) if role == "assistant" else (),
                exchange_id=exchange_id,
            )
            for role, content, exchange_id in rows
        ]

    def read_messages(self, session_id: str) -> list[MessageRecord]:
        """Alias for :meth:`get_messages` using a read-oriented name."""

        return self.get_messages(session_id)

    def record_paper_status(
        self,
        paper_id: str,
        filename: str,
        status: str,
        *,
        chunk_count: int = 0,
        error: str | None = None,
    ) -> None:
        """Insert or replace the last known indexing outcome for one paper.

        The row is keyed by ``paper_id`` so re-indexing overwrites the prior
        outcome, and ``updated_at`` records when the outcome was persisted.
        """

        identifier = _validate_content(paper_id)
        name = _validate_content(filename)
        state = _validate_content(status)
        if (
            isinstance(chunk_count, bool)
            or not isinstance(chunk_count, int)
            or chunk_count < 0
        ):
            raise ValueError("chunk_count must be a non-negative integer")
        if error is not None and not isinstance(error, str):
            raise TypeError("error must be a string or None")
        with self._connection:
            self._connection.execute(
                """
                INSERT INTO paper_index_status
                    (paper_id, filename, status, chunk_count, error, updated_at)
                VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT(paper_id) DO UPDATE SET
                    filename = excluded.filename,
                    status = excluded.status,
                    chunk_count = excluded.chunk_count,
                    error = excluded.error,
                    updated_at = excluded.updated_at
                """,
                (
                    identifier,
                    name,
                    state,
                    chunk_count,
                    error,
                    datetime.now(timezone.utc).isoformat(),
                ),
            )

    def clear_paper_statuses(self) -> None:
        """Delete every persisted paper indexing outcome.

        Used by the reset-all-indices command so the per-paper indicators fall
        back to "not indexed" once the vector and lexical indexes are wiped.
        """

        with self._connection:
            self._connection.execute("DELETE FROM paper_index_status")

    def get_paper_statuses(self) -> list[PaperStatusRecord]:
        """Return every persisted paper indexing outcome, ordered by paper id."""

        rows = self._connection.execute(
            "SELECT paper_id, filename, status, chunk_count, error, updated_at "
            "FROM paper_index_status ORDER BY paper_id ASC"
        ).fetchall()
        return [
            PaperStatusRecord(
                paper_id=paper_id,
                filename=filename,
                status=status,
                chunk_count=chunk_count,
                error=error,
                updated_at=updated_at,
            )
            for paper_id, filename, status, chunk_count, error, updated_at in rows
        ]

    def close(self) -> None:
        """Close the store's SQLite connection."""

        self._connection.close()

    def __enter__(self) -> SQLiteHistoryStore:
        """Return this store for use in a ``with`` statement."""

        return self

    def __exit__(self, _exc_type: object, _exc_value: object, _traceback: object) -> None:
        """Close the store when leaving a ``with`` statement."""

        self.close()


def _validate_database_path(database_path: DatabasePath) -> Path:
    if not isinstance(database_path, (str, Path)):
        raise TypeError("database_path must be a non-empty string or Path")
    if not str(database_path).strip():
        raise ValueError("database_path must not be empty")
    return Path(database_path)


def _validate_session_id(session_id: str) -> str:
    if not isinstance(session_id, str):
        raise TypeError("session_id must be a non-empty string")
    if not session_id.strip():
        raise ValueError("session_id must not be empty")
    return session_id


def _validate_role(role: str) -> str:
    if not isinstance(role, str):
        raise TypeError("role must be a string")
    normalized = role.strip().lower()
    if not normalized:
        raise ValueError("role must not be empty")
    if normalized not in _VALID_ROLES:
        supported = ", ".join(sorted(_VALID_ROLES))
        raise ValueError(f"unsupported role {role!r}; expected one of: {supported}")
    return normalized


def _validate_content(content: str) -> str:
    if not isinstance(content, str):
        raise TypeError("content must be a string")
    if not content.strip():
        raise ValueError("content must not be empty")
    return content


def _validate_exchange_id(exchange_id: int) -> int:
    if isinstance(exchange_id, bool) or not isinstance(exchange_id, int):
        raise TypeError("exchange_id must be a positive integer")
    if exchange_id <= 0:
        raise ValueError("exchange_id must be a positive integer")
    return exchange_id


def _validate_sources(sources: Sequence[SourceRecord]) -> tuple[SourceRecord, ...]:
    if isinstance(sources, (str, bytes)) or not isinstance(sources, Sequence):
        raise TypeError("sources must be a sequence of SourceRecord values")
    records = tuple(sources)
    if len(records) > MAX_EXCHANGE_SOURCE_COUNT:
        raise ValueError(f"sources cannot contain more than {MAX_EXCHANGE_SOURCE_COUNT} records")
    for expected_rank, source in enumerate(records, start=1):
        if not isinstance(source, SourceRecord):
            raise TypeError("sources must contain only SourceRecord values")
        if source.rank != expected_rank:
            raise ValueError("source ranks must be consecutive from 1")
        if isinstance(source.chunk_index, bool) or not isinstance(
            source.chunk_index, int
        ):
            raise TypeError("source chunk_index must be a non-negative integer")
        if source.chunk_index < 0:
            raise ValueError("source chunk_index must be a non-negative integer")
        if isinstance(source.page, bool) or not isinstance(source.page, int):
            raise TypeError("source page must be a positive integer")
        if source.page < 1:
            raise ValueError("source page must be a positive integer")
        if (
            isinstance(source.score, bool)
            or not isinstance(source.score, (int, float))
            or not math.isfinite(float(source.score))
        ):
            raise ValueError("source score must be a finite number")
        for source_field, value in (
            ("paper_id", source.paper_id),
            ("filename", source.filename),
            ("title", source.title),
            ("snippet", source.snippet),
        ):
            if not isinstance(value, str) or not value.strip():
                raise ValueError(f"source {source_field} must be a non-empty string")
        if len(source.snippet) > MAX_SOURCE_SNIPPET_LENGTH:
            raise ValueError("source snippet is too long")
    return records


__all__ = [
    "ExchangeRecord",
    "MAX_EXCHANGE_SOURCE_COUNT",
    "MAX_SOURCE_SNIPPET_LENGTH",
    "MessageRecord",
    "PaperStatusRecord",
    "SQLiteHistoryStore",
    "SourceRecord",
]
