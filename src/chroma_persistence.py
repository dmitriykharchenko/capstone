"""Small persistence adapter for a local, persistent Chroma collection.

The adapter deliberately does not create embeddings or make network calls.  Callers
provide embedding vectors and may inject a client/module in tests.  Chroma is loaded
only when :func:`create_client` is called, so importing this module is safe when the
optional ``chromadb`` package is not installed.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from numbers import Real
from pathlib import Path
from typing import Any

DEFAULT_COLLECTION_NAME = "documents"


@dataclass(frozen=True)
class ChromaConfig:
    """Configuration for a local Chroma collection."""

    persist_directory: str | Path
    collection_name: str = DEFAULT_COLLECTION_NAME

    def __post_init__(self) -> None:
        _validate_persist_directory(self.persist_directory)
        _validate_collection_name(self.collection_name)


def _validate_persist_directory(persist_directory: str | Path) -> None:
    if not isinstance(persist_directory, (str, Path)):
        raise TypeError("persist_directory must be a non-empty string or Path")
    if not str(persist_directory).strip():
        raise ValueError("persist_directory must not be empty")


def _validate_collection_name(collection_name: str) -> None:
    if not isinstance(collection_name, str):
        raise TypeError("collection_name must be a string")
    if not collection_name.strip():
        raise ValueError("collection_name must not be empty")


def _load_chromadb() -> Any:
    try:
        import chromadb
    except ImportError as exc:
        raise RuntimeError(
            "chromadb is required to open a collection; install it before "
            "using create_client or open_collection"
        ) from exc
    return chromadb


def create_client(
    persist_directory: str | Path,
    *,
    chroma_module: Any | None = None,
) -> Any:
    """Create a Chroma persistent client rooted at ``persist_directory``.

    ``chroma_module`` is an optional injection point for tests and must expose a
    ``PersistentClient`` constructor.  No embedding provider is configured here.
    """

    _validate_persist_directory(persist_directory)
    module = chroma_module if chroma_module is not None else _load_chromadb()
    try:
        constructor = module.PersistentClient
    except AttributeError as exc:
        raise TypeError("chroma_module must expose PersistentClient") from exc
    return constructor(path=str(persist_directory))


def open_collection(
    persist_directory: str | Path,
    collection_name: str = DEFAULT_COLLECTION_NAME,
    *,
    client: Any | None = None,
    chroma_module: Any | None = None,
) -> Any:
    """Open or create a named persistent Chroma collection.

    Pass ``client`` to reuse an existing Chroma client (or a mock).  If omitted,
    :func:`create_client` creates a local persistent client.
    """

    _validate_persist_directory(persist_directory)
    _validate_collection_name(collection_name)
    resolved_client = client if client is not None else create_client(
        persist_directory, chroma_module=chroma_module
    )
    try:
        get_or_create = resolved_client.get_or_create_collection
    except AttributeError as exc:
        raise TypeError(
            "client must expose get_or_create_collection"
        ) from exc
    return get_or_create(name=collection_name)


def _materialize(value: Any, label: str) -> list[Any]:
    if isinstance(value, (str, bytes)) or not isinstance(value, Sequence):
        raise TypeError(f"{label} must be a sequence")
    return list(value)


def _validate_embedding(embedding: Any, label: str) -> list[float | int]:
    values = _materialize(embedding, label)
    if not values:
        raise ValueError(f"{label} must not be empty")
    if any(isinstance(value, bool) or not isinstance(value, Real) for value in values):
        raise TypeError(f"{label} must contain only numeric values")
    return values


def _validate_records(
    ids: Sequence[str],
    texts: Sequence[str],
    embeddings: Sequence[Sequence[float]],
    metadatas: Sequence[Mapping[str, Any]] | None,
) -> tuple[list[str], list[str], list[list[float | int]], list[Mapping[str, Any]]]:
    normalized_ids = _materialize(ids, "ids")
    normalized_texts = _materialize(texts, "texts")
    normalized_embeddings = _materialize(embeddings, "embeddings")
    if not normalized_ids:
        raise ValueError("records must contain at least one item")
    lengths = {
        "ids": len(normalized_ids),
        "texts": len(normalized_texts),
        "embeddings": len(normalized_embeddings),
    }
    if len(set(lengths.values())) != 1:
        raise ValueError(
            "ids, texts, and embeddings must have equal lengths "
            f"(got {lengths['ids']}, {lengths['texts']}, {lengths['embeddings']})"
        )
    if any(not isinstance(item, str) or not item.strip() for item in normalized_ids):
        raise TypeError("ids must contain non-empty strings")
    if len(set(normalized_ids)) != len(normalized_ids):
        raise ValueError("ids must be unique within a write")
    if any(not isinstance(item, str) for item in normalized_texts):
        raise TypeError("texts must contain only strings")
    vectors = [
        _validate_embedding(embedding, f"embeddings[{index}]")
        for index, embedding in enumerate(normalized_embeddings)
    ]
    dimensions = {len(vector) for vector in vectors}
    if len(dimensions) != 1:
        raise ValueError("all embeddings must have the same vector dimension")
    if metadatas is None:
        raise ValueError("metadatas are required and must include paper_id and chunk_index")
    normalized_metadatas = _materialize(metadatas, "metadatas")
    if len(normalized_metadatas) != len(normalized_ids):
        raise ValueError("metadatas must have the same length as ids")
    for index, metadata in enumerate(normalized_metadatas):
        if not isinstance(metadata, Mapping):
            raise TypeError("metadatas must contain mappings")
        if "paper_id" not in metadata:
            raise ValueError(f"metadatas[{index}] must include paper_id")
        paper_id = metadata["paper_id"]
        if not isinstance(paper_id, str):
            raise TypeError(f"metadatas[{index}].paper_id must be a non-empty string")
        if not paper_id.strip():
            raise ValueError(f"metadatas[{index}].paper_id must be a non-empty string")
        if "chunk_index" not in metadata:
            raise ValueError(f"metadatas[{index}] must include chunk_index")
        chunk_index = metadata["chunk_index"]
        if isinstance(chunk_index, bool) or not isinstance(chunk_index, int):
            raise TypeError(f"metadatas[{index}].chunk_index must be a non-negative integer")
        if chunk_index < 0:
            raise ValueError(f"metadatas[{index}].chunk_index must be a non-negative integer")
    return normalized_ids, normalized_texts, vectors, normalized_metadatas


def upsert_records(
    collection: Any,
    ids: Sequence[str],
    texts: Sequence[str],
    embeddings: Sequence[Sequence[float]],
    metadatas: Sequence[Mapping[str, Any]] | None = None,
) -> None:
    """Insert or update embedded text records with required paper provenance."""

    if collection is None or not hasattr(collection, "upsert"):
        raise TypeError("collection must expose an upsert method")
    normalized = _validate_records(ids, texts, embeddings, metadatas)
    normalized_ids, normalized_texts, vectors, normalized_metadatas = normalized
    kwargs: dict[str, Any] = {
        "ids": normalized_ids,
        "documents": normalized_texts,
        "embeddings": vectors,
    }
    if normalized_metadatas is not None:
        kwargs["metadatas"] = normalized_metadatas
    collection.upsert(**kwargs)


def write_records(
    collection: Any,
    ids: Sequence[str],
    texts: Sequence[str],
    embeddings: Sequence[Sequence[float]],
    metadatas: Sequence[Mapping[str, Any]] | None = None,
) -> None:
    """Alias for :func:`upsert_records` using a write-oriented name."""

    upsert_records(collection, ids, texts, embeddings, metadatas)


def _validate_query_embedding(query_embedding: Sequence[float]) -> list[float | int]:
    return _validate_embedding(query_embedding, "query_embedding")


def query_records(
    collection: Any,
    query_embedding: Sequence[float],
    n_results: int = 5,
    *,
    where: Mapping[str, Any] | None = None,
    include: Sequence[str] | None = None,
) -> dict[str, Any]:
    """Return records most similar to one caller-supplied embedding vector."""

    if collection is None or not hasattr(collection, "query"):
        raise TypeError("collection must expose a query method")
    if isinstance(n_results, bool) or not isinstance(n_results, int):
        raise TypeError("n_results must be an integer")
    if n_results < 1:
        raise ValueError("n_results must be at least 1")
    if where is not None and not isinstance(where, Mapping):
        raise TypeError("where must be a mapping")
    if include is not None:
        include = _materialize(include, "include")
        if any(not isinstance(item, str) for item in include):
            raise TypeError("include must contain only strings")
    kwargs: dict[str, Any] = {
        "query_embeddings": [_validate_query_embedding(query_embedding)],
        "n_results": n_results,
    }
    if where is not None:
        kwargs["where"] = where
    if include is not None:
        kwargs["include"] = include
    return collection.query(**kwargs)


def get_records(
    collection: Any,
    *,
    ids: Sequence[str] | None = None,
    where: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """Read records by IDs, metadata filter, or all records when unfiltered."""

    if collection is None or not hasattr(collection, "get"):
        raise TypeError("collection must expose a get method")
    kwargs: dict[str, Any] = {}
    if ids is not None:
        normalized_ids = _materialize(ids, "ids")
        if any(not isinstance(item, str) or not item.strip() for item in normalized_ids):
            raise TypeError("ids must contain non-empty strings")
        kwargs["ids"] = normalized_ids
    if where is not None:
        if not isinstance(where, Mapping):
            raise TypeError("where must be a mapping")
        kwargs["where"] = where
    return collection.get(**kwargs)


def delete_records(
    collection: Any,
    *,
    ids: Sequence[str] | None = None,
    where: Mapping[str, Any] | None = None,
) -> None:
    """Delete records by IDs or metadata filter; never delete unconditionally."""

    if collection is None or not hasattr(collection, "delete"):
        raise TypeError("collection must expose a delete method")
    if ids is None and where is None:
        raise ValueError("provide ids or where to avoid deleting the collection")
    if ids is not None and where is not None:
        raise ValueError("provide either ids or where, not both")
    kwargs: dict[str, Any] = {}
    if ids is not None:
        normalized_ids = _materialize(ids, "ids")
        if not normalized_ids:
            raise ValueError("ids must not be empty")
        if any(not isinstance(item, str) or not item.strip() for item in normalized_ids):
            raise TypeError("ids must contain non-empty strings")
        kwargs["ids"] = normalized_ids
    else:
        if not isinstance(where, Mapping):
            raise TypeError("where must be a mapping")
        kwargs["where"] = where
    collection.delete(**kwargs)


__all__ = [
    "ChromaConfig",
    "DEFAULT_COLLECTION_NAME",
    "create_client",
    "delete_records",
    "get_records",
    "open_collection",
    "query_records",
    "upsert_records",
    "write_records",
]