
import pytest

from src.storage.sqlite_history import (
    SQLiteHistoryStore,
)


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
