from unittest.mock import Mock

import pytest

from src.retrieval.chat_history_index import (
    ChatHistoryIndex,
    ChatHistoryIndexError,
)
from src.storage.sqlite_history import SQLiteHistoryStore


class FakeEmbedder:
    def __init__(self):
        self.texts = []

    def __call__(self, text):
        self.texts.append(text)
        return [float(len(text)), 1.0]


def _index(store, collection=None, embedder=None):
    return ChatHistoryIndex(
        store,
        collection=collection or Mock(),
        embedder=embedder or FakeEmbedder(),
    )


def test_global_and_session_search_hydrate_in_vector_order(tmp_path):
    collection = Mock()
    embedder = FakeEmbedder()
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        first_id = store.append_exchange("alpha", "first question", "first answer")
        second_id = store.append_exchange("beta", "second question", "second answer")
        collection.query.side_effect = [
            {
                "metadatas": [[
                    {"exchange_id": second_id, "session_id": "beta"},
                    {"exchange_id": first_id, "session_id": "alpha"},
                ]]
            },
            {"metadatas": [[{"exchange_id": first_id, "session_id": "alpha"}]]},
        ]
        index = _index(store, collection, embedder)

        global_results = index.search("question", 2)
        session_results = index.search("follow-up", session_id="alpha")

    assert [item.exchange_id for item in global_results] == [second_id, first_id]
    assert session_results == [global_results[1]]
    assert collection.query.call_args_list[0].kwargs == {
        "query_embeddings": [[8.0, 1.0]],
        "n_results": 2,
        "include": ["metadatas"],
    }
    assert collection.query.call_args_list[1].kwargs["where"] == {
        "session_id": "alpha"
    }


@pytest.mark.parametrize(
    "response, message",
    [
        ({}, "missing metadata"),
        ({"metadatas": [[{"session_id": "session"}]]}, "exchange_id"),
        (
            {"metadatas": [[{"exchange_id": 999, "session_id": "session"}]]},
            "failed to hydrate",
        ),
    ],
)
def test_search_fails_explicitly_for_invalid_or_dangling_metadata(
    tmp_path, response, message
):
    collection = Mock()
    collection.query.return_value = response
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        index = _index(store, collection)
        with pytest.raises(ChatHistoryIndexError, match=message):
            index.search("query")


def test_collection_failure_is_explicit_and_does_not_change_sqlite(tmp_path):
    collection = Mock()
    collection.upsert.side_effect = RuntimeError("offline Chroma")
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        exchange_id = store.append_exchange("session", "question", "answer")
        with pytest.raises(ChatHistoryIndexError, match="failed to index"):
            _index(store, collection).index_exchange(exchange_id, "session")
        assert store.get_exchange(exchange_id, "session").assistant_message.content == "answer"
