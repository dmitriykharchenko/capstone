import sqlite3
import subprocess
import sys

import pytest

from src.sqlite_history import MessageRecord, SQLiteHistoryStore


def test_messages_persist_in_order_and_are_isolated_by_session(tmp_path):
    database = tmp_path / "nested" / "history.sqlite3"
    store = SQLiteHistoryStore(database)
    store.append_message("alpha", "user", "first")
    store.append_message("alpha", "assistant", "second")
    store.append_message("beta", "user", "other session")
    store.close()

    reopened = SQLiteHistoryStore(database)
    assert reopened.get_messages("alpha") == [
        MessageRecord("user", "first"),
        MessageRecord("assistant", "second"),
    ]
    assert reopened.get_messages("beta") == [MessageRecord("user", "other session")]
    reopened.close()


def test_fresh_process_reads_existing_messages(tmp_path):
    database = tmp_path / "history.sqlite3"
    with SQLiteHistoryStore(database) as store:
        store.append_message("session", "user", "persist me")

    script = (
        "from src.sqlite_history import SQLiteHistoryStore; "
        "print([(m.role, m.content) for m in "
        f"SQLiteHistoryStore({str(database)!r}).get_messages('session')])"
    )
    result = subprocess.run(
        [sys.executable, "-c", script], capture_output=True, text=True, check=True
    )
    assert result.stdout.strip() == "[('user', 'persist me')]"


def test_empty_session_returns_empty_list(tmp_path):
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        assert store.get_messages("new-session") == []


@pytest.mark.parametrize(
    ("session_id", "role", "content", "message"),
    [
        ("", "user", "text", "session_id must not be empty"),
        ("s", "", "text", "role must not be empty"),
        ("s", "unknown", "text", "unsupported role"),
        ("s", "user", " ", "content must not be empty"),
    ],
)
def test_append_validates_message_fields(tmp_path, session_id, role, content, message):
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        with pytest.raises((TypeError, ValueError), match=message):
            store.append_message(session_id, role, content)


@pytest.mark.parametrize("value", [None, 42, " "])
def test_store_and_read_validate_session_ids(tmp_path, value):
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        with pytest.raises((TypeError, ValueError), match="session_id"):
            store.get_messages(value)  # type: ignore[arg-type]


def test_schema_uses_parameterized_session_queries(tmp_path):
    database = tmp_path / "history.sqlite3"
    with SQLiteHistoryStore(database) as store:
        store.append_message("safe", "user", "text")
    with sqlite3.connect(database) as connection:
        assert connection.execute("SELECT COUNT(*) FROM messages").fetchone() == (1,)