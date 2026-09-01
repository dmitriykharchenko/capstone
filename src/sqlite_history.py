"""Persistent, provider-neutral conversation message history.

The :class:`SQLiteHistoryStore` keeps only a session identifier, a canonical
message role, and message text.  It intentionally knows nothing about a chat
model or an LLM provider, so callers can adapt :class:`MessageRecord` to the
message type used by their application.
"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass
from pathlib import Path

DatabasePath = str | Path

_VALID_ROLES = frozenset(
    {"system", "user", "assistant", "human", "ai", "tool", "function", "developer"}
)


@dataclass(frozen=True, slots=True)
class MessageRecord:
    """A provider-neutral message returned by a history store.

    ``role`` uses a conventional chat role (for example ``"user"`` or
    ``"assistant"``), while ``content`` is the message text exactly as supplied
    by the caller.
    """

    role: str
    content: str


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
        self._connection.execute(
            """
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL
            )
            """
        )
        self._connection.execute(
            "CREATE INDEX IF NOT EXISTS idx_messages_session_id "
            "ON messages (session_id, id)"
        )
        self._connection.commit()

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

    def get_messages(self, session_id: str) -> list[MessageRecord]:
        """Return all messages for ``session_id`` in insertion order.

        An unknown, but valid, session identifier returns an empty list.

        Raises:
            TypeError: If ``session_id`` is not a string.
            ValueError: If ``session_id`` is empty or whitespace-only.
        """

        session = _validate_session_id(session_id)
        rows = self._connection.execute(
            "SELECT role, content FROM messages WHERE session_id = ? ORDER BY id ASC",
            (session,),
        ).fetchall()
        return [MessageRecord(role=role, content=content) for role, content in rows]

    def read_messages(self, session_id: str) -> list[MessageRecord]:
        """Alias for :meth:`get_messages` using a read-oriented name."""

        return self.get_messages(session_id)

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


__all__ = ["MessageRecord", "SQLiteHistoryStore"]