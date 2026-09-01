from unittest.mock import Mock

import pytest

from src.chat_history_index import (
    ChatHistoryIndex,
    ChatHistoryIndexError,
    DEFAULT_CHAT_COLLECTION_NAME,
    build_exchange_context,
    exchange_vector_id,
)
from src.chroma_persistence import upsert_records
from src.sqlite_history import ExchangeRecord, MessageRecord, SQLiteHistoryStore


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


def test_context_is_labeled_and_vector_id_is_deterministic():
    exchange = ExchangeRecord(
        7,
        "session",
        MessageRecord("user", "How does it work?"),
        MessageRecord("assistant", "With labeled context."),
    )

    assert build_exchange_context(exchange) == (
        "User: How does it work?\nAssistant: With labeled context."
    )
    assert exchange_vector_id(7) == "exchange:7"
    assert exchange_vector_id(7) == "exchange:7"


def test_index_exchange_upserts_canonical_context_and_link_metadata(tmp_path):
    collection = Mock()
    embedder = FakeEmbedder()
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        exchange_id = store.append_exchange("alpha", "question", "answer")
        index = _index(store, collection, embedder)

        index.index_exchange(exchange_id, "alpha")
        index.index_exchange(exchange_id, "alpha")

    expected_context = "User: question\nAssistant: answer"
    assert embedder.texts == [expected_context, expected_context]
    assert collection.upsert.call_count == 2
    for call in collection.upsert.call_args_list:
        assert call.kwargs == {
            "ids": [f"exchange:{exchange_id}"],
            "documents": [expected_context],
            "embeddings": [[32.0, 1.0]],
            "metadatas": [{"exchange_id": exchange_id, "session_id": "alpha"}],
        }


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


def test_search_uses_sqlite_content_not_vector_documents(tmp_path):
    collection = Mock()
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        exchange_id = store.append_exchange("session", "canonical user", "canonical answer")
        collection.query.return_value = {
            "documents": [["stale vector document"]],
            "metadatas": [[{"exchange_id": exchange_id, "session_id": "session"}]],
        }

        result = _index(store, collection).search("canonical")[0]

    assert result.user_message.content == "canonical user"
    assert result.assistant_message.content == "canonical answer"


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


def test_chat_metadata_is_separate_from_paper_provenance_validation(tmp_path):
    collection = Mock()
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        exchange_id = store.append_exchange("session", "question", "answer")
        _index(store, collection).index_exchange(exchange_id, "session")

    assert collection.upsert.call_args.kwargs["metadatas"] == [
        {"exchange_id": exchange_id, "session_id": "session"}
    ]
    with pytest.raises(ValueError, match="paper_id"):
        upsert_records(
            Mock(),
            ["chat"],
            ["User: question\nAssistant: answer"],
            [[1.0]],
            [{"exchange_id": exchange_id, "session_id": "session"}],
        )


def test_default_opens_dedicated_collection(monkeypatch, tmp_path):
    collection = Mock()
    opener = Mock(return_value=collection)
    monkeypatch.setattr("src.chat_history_index.open_collection", opener)
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        ChatHistoryIndex(store, persist_directory=tmp_path / "chroma", embedder=FakeEmbedder())

    opener.assert_called_once_with(tmp_path / "chroma", DEFAULT_CHAT_COLLECTION_NAME)