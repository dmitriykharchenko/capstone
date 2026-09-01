import sqlite3
import subprocess
import sys

import pytest

from src.sqlite_history import ExchangeRecord, MessageRecord, SQLiteHistoryStore


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


def test_legacy_database_migrates_without_changing_existing_rows(tmp_path):
    database = tmp_path / "legacy.sqlite3"
    with sqlite3.connect(database) as connection:
        connection.execute(
            "CREATE TABLE messages ("
            "id INTEGER PRIMARY KEY AUTOINCREMENT, "
            "session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL)"
        )
        connection.executemany(
            "INSERT INTO messages (session_id, role, content) VALUES (?, ?, ?)",
            [("legacy", "user", "old question"), ("legacy", "assistant", "old answer")],
        )

    with SQLiteHistoryStore(database) as store:
        assert store.get_messages("legacy") == [
            MessageRecord("user", "old question"),
            MessageRecord("assistant", "old answer"),
        ]
        exchange_id = store.append_exchange("legacy", "new question", "new answer")

    with sqlite3.connect(database) as connection:
        columns = {row[1] for row in connection.execute("PRAGMA table_info(messages)")}
        assert "exchange_id" in columns
        assert connection.execute(
            "SELECT exchange_id FROM messages ORDER BY id"
        ).fetchall() == [(None,), (None,), (exchange_id,), (exchange_id,)]


def test_append_exchange_is_atomic_and_returns_retrievable_stable_id(tmp_path):
    database = tmp_path / "history.sqlite3"
    with SQLiteHistoryStore(database) as store:
        exchange_id = store.append_exchange("session", "question", "answer")
        assert store.get_exchange(exchange_id, "session") == ExchangeRecord(
            exchange_id,
            "session",
            MessageRecord("user", "question"),
            MessageRecord("assistant", "answer"),
        )

    with SQLiteHistoryStore(database) as reopened:
        assert reopened.get_exchange(exchange_id, "session").exchange_id == exchange_id
        assert reopened.get_messages("session") == [
            MessageRecord("user", "question"),
            MessageRecord("assistant", "answer"),
        ]


def test_exchange_write_rolls_back_all_rows_when_second_message_fails(tmp_path):
    database = tmp_path / "history.sqlite3"
    SQLiteHistoryStore(database).close()
    with sqlite3.connect(database) as connection:
        connection.execute(
            "CREATE TRIGGER fail_assistant BEFORE INSERT ON messages "
            "WHEN NEW.exchange_id IS NOT NULL AND NEW.role = 'assistant' "
            "BEGIN SELECT RAISE(ABORT, 'test failure'); END"
        )

    with SQLiteHistoryStore(database) as store:
        with pytest.raises(sqlite3.IntegrityError, match="test failure"):
            store.append_exchange("session", "question", "answer")
        assert store.get_messages("session") == []

    with sqlite3.connect(database) as connection:
        assert connection.execute("SELECT COUNT(*) FROM exchanges").fetchone() == (0,)
        assert connection.execute("SELECT COUNT(*) FROM messages").fetchone() == (0,)


def test_exchanges_preserve_message_order_with_standalone_rows(tmp_path):
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        store.append_message("session", "system", "standalone")
        first_id = store.append_exchange("session", "first question", "first answer")
        second_id = store.append_exchange("session", "second question", "second answer")
        assert second_id > first_id
        assert store.get_messages("session") == [
            MessageRecord("system", "standalone"),
            MessageRecord("user", "first question"),
            MessageRecord("assistant", "first answer"),
            MessageRecord("user", "second question"),
            MessageRecord("assistant", "second answer"),
        ]


def test_exchange_retrieval_enforces_session_ownership(tmp_path):
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        exchange_id = store.append_exchange("owner", "question", "answer")
        store.append_exchange("other", "other question", "other answer")
        with pytest.raises(KeyError, match="does not exist for session"):
            store.get_exchange(exchange_id, "other")
        assert store.get_messages("other") == [
            MessageRecord("user", "other question"),
            MessageRecord("assistant", "other answer"),
        ]


@pytest.mark.parametrize("exchange_id", [None, True, 1.5, "1", 0, -1])
def test_get_exchange_validates_exchange_id(tmp_path, exchange_id):
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        with pytest.raises((TypeError, ValueError), match="exchange_id"):
            store.get_exchange(exchange_id, "session")  # type: ignore[arg-type]


def test_get_exchange_rejects_unknown_id_and_invalid_session(tmp_path):
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        with pytest.raises(KeyError, match="does not exist"):
            store.get_exchange(999, "session")
        with pytest.raises(ValueError, match="session_id"):
            store.get_exchange(1, " ")