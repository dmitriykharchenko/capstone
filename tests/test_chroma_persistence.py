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
    upsert_records(collection, ["a", "b"], ["one", "two"], [[1.0, 0.0], [0.0, 1.0]], [{"x": 1}, {"x": 2}])
    collection.query.return_value = {"ids": [["a"]]}

    result = query_records(collection, [1.0, 0.0], 1)
    records = get_records(collection, ids=["a"])
    delete_records(collection, ids=["a"])

    assert result == {"ids": [["a"]]}
    assert records == collection.get.return_value
    collection.upsert.assert_called_once()
    collection.delete.assert_called_once_with(ids=["a"])


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
