from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from src.chroma_persistence import (
    ChromaConfig,
    create_client,
    delete_records,
    get_records,
    open_collection,
    query_records,
    upsert_records,
)


def test_config_and_client_use_explicit_persistence_path(tmp_path):
    config = ChromaConfig(tmp_path / "db", "papers")
    persistent_client = Mock()
    module = SimpleNamespace(PersistentClient=Mock(return_value=persistent_client))

    client = create_client(config.persist_directory, chroma_module=module)

    assert client is persistent_client
    module.PersistentClient.assert_called_once_with(path=str(tmp_path / "db"))


def test_open_collection_creates_named_collection():
    client = Mock()
    expected = object()
    client.get_or_create_collection.return_value = expected

    result = open_collection("db", "papers", client=client)

    assert result is expected
    client.get_or_create_collection.assert_called_once_with(name="papers")


def test_upsert_query_read_and_delete_records():
    collection = Mock()
    metadata = [
        {"paper_id": "paper-a", "chunk_index": 0, "title": "Paper A"},
        {"paper_id": "paper-b", "chunk_index": 1, "source": "archive"},
    ]
    upsert_records(collection, ["a", "b"], ["one", "two"], [[1.0, 0.0], [0.0, 1.0]], metadata)
    collection.query.return_value = {"ids": [["a"]], "metadatas": [[metadata[0]]]}
    collection.get.return_value = {"ids": ["a"], "metadatas": [metadata[0]]}

    result = query_records(collection, [1.0, 0.0], 1)
    records = get_records(collection, ids=["a"])
    delete_records(collection, ids=["a"])

    assert result == {"ids": [["a"]], "metadatas": [[metadata[0]]]}
    assert records == {"ids": ["a"], "metadatas": [metadata[0]]}
    collection.upsert.assert_called_once_with(
        ids=["a", "b"],
        documents=["one", "two"],
        embeddings=[[1.0, 0.0], [0.0, 1.0]],
        metadatas=metadata,
    )
    collection.delete.assert_called_once_with(ids=["a"])


@pytest.mark.parametrize(
    "metadata, message",
    [
        (None, "paper_id and chunk_index"),
        ([{"chunk_index": 0}], "paper_id"),
        ([{"paper_id": "paper",}], "chunk_index"),
        ([{"paper_id": "", "chunk_index": 0}], "paper_id"),
        ([{"paper_id": 4, "chunk_index": 0}], "paper_id"),
        ([{"paper_id": "paper", "chunk_index": True}], "chunk_index"),
        ([{"paper_id": "paper", "chunk_index": -1}], "chunk_index"),
    ],
)
def test_upsert_requires_valid_paper_provenance(metadata, message):
    with pytest.raises((TypeError, ValueError), match=message):
        upsert_records(Mock(), ["a"], ["text"], [[1.0]], metadata)


@pytest.mark.parametrize(
    "args, message",
    [
        ((["a"], [], [[1.0]]), "equal lengths"),
        ((["a"], ["text"], [[]]), "must not be empty"),
        ((["a"], ["text"], [[1.0], [2.0]]), "equal lengths"),
    ],
)
def test_upsert_validates_lengths_and_vectors(args, message):
    with pytest.raises((TypeError, ValueError), match=message):
        upsert_records(Mock(), *args)


def test_query_and_delete_validate_inputs():
    collection = Mock()
    with pytest.raises(ValueError, match="at least 1"):
        query_records(collection, [1.0], 0)
    with pytest.raises(ValueError, match="provide ids or where"):
        delete_records(collection)
