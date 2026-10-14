"""Chroma-backed semantic index for canonical SQLite chat exchanges."""

from __future__ import annotations

import math
from collections.abc import Callable, Mapping, Sequence
from numbers import Real
from pathlib import Path
from typing import Any

from ..storage.chroma_persistence import open_collection
from .embeddings import EmbeddingBackend, EmbeddingConfig, create_embedding
from ..storage.sqlite_history import ExchangeRecord, SQLiteHistoryStore

DEFAULT_CHAT_COLLECTION_NAME = "chat_exchanges"


class ChatHistoryIndexError(RuntimeError):
    """Raised when exchange indexing or vector-result hydration fails."""


def build_exchange_context(exchange: ExchangeRecord) -> str:
    """Return labeled user and assistant text for one completed exchange."""

    if not isinstance(exchange, ExchangeRecord):
        raise TypeError("exchange must be an ExchangeRecord")
    return (
        f"User: {exchange.user_message.content}\n"
        f"Assistant: {exchange.assistant_message.content}"
    )


def exchange_vector_id(exchange_id: int) -> str:
    """Return the deterministic Chroma ID for a SQLite exchange ID."""

    if isinstance(exchange_id, bool) or not isinstance(exchange_id, int):
        raise TypeError("exchange_id must be a positive integer")
    if exchange_id <= 0:
        raise ValueError("exchange_id must be a positive integer")
    return f"exchange:{exchange_id}"


class ChatHistoryIndex:
    """Embed exchanges in Chroma and hydrate search results from SQLite.

    A collection may be injected for offline tests. Otherwise an explicit
    persistence directory opens the dedicated ``chat_exchanges`` collection.
    SQLite remains canonical: indexing reads from the history store, and
    searches return freshly hydrated :class:`ExchangeRecord` values.
    """

    def __init__(
        self,
        history_store: SQLiteHistoryStore,
        *,
        collection: Any | None = None,
        persist_directory: str | Path | None = None,
        collection_name: str = DEFAULT_CHAT_COLLECTION_NAME,
        embedding_config: EmbeddingConfig | None = None,
        embedding_backend: EmbeddingBackend | Any | None = None,
        embedder: Callable[[str], Sequence[float]] | None = None,
    ) -> None:
        if not callable(getattr(history_store, "get_exchange", None)):
            raise TypeError("history_store must expose get_exchange")
        if collection is None:
            if persist_directory is None:
                raise ValueError(
                    "persist_directory is required when collection is not provided"
                )
            collection = open_collection(persist_directory, collection_name)
        if not callable(getattr(collection, "upsert", None)) or not callable(
            getattr(collection, "query", None)
        ):
            raise TypeError("collection must expose upsert and query methods")
        if embedder is not None and not callable(embedder):
            raise TypeError("embedder must be callable")
        self._history = history_store
        self._collection = collection
        self._embedding_config = embedding_config
        self._embedding_backend = embedding_backend
        self._embedder = embedder

    def _embed(self, text: str) -> list[float]:
        values = (
            self._embedder(text)
            if self._embedder is not None
            else create_embedding(
                text,
                config=self._embedding_config,
                backend=self._embedding_backend,
            )
        )
        if (
            isinstance(values, (str, bytes))
            or not isinstance(values, Sequence)
            or not values
        ):
            raise ChatHistoryIndexError("embedder returned an empty or invalid vector")
        vector: list[float] = []
        for value in values:
            if isinstance(value, bool) or not isinstance(value, Real):
                raise ChatHistoryIndexError("embedder returned a non-numeric vector")
            number = float(value)
            if not math.isfinite(number):
                raise ChatHistoryIndexError("embedder returned a non-finite vector")
            vector.append(number)
        return vector

    def index_exchange(self, exchange_id: int, session_id: str) -> None:
        """Upsert one canonical exchange under an idempotent vector ID."""

        exchange = self._history.get_exchange(exchange_id, session_id)
        context = build_exchange_context(exchange)
        try:
            self._collection.upsert(
                ids=[exchange_vector_id(exchange.exchange_id)],
                documents=[context],
                embeddings=[self._embed(context)],
                metadatas=[
                    {"exchange_id": exchange.exchange_id, "session_id": exchange.session_id}
                ],
            )
        except ChatHistoryIndexError:
            raise
        except Exception as exc:
            raise ChatHistoryIndexError(
                f"failed to index exchange {exchange.exchange_id}"
            ) from exc

    def search(
        self, query: str, n_results: int = 5, *, session_id: str | None = None
    ) -> list[ExchangeRecord]:
        """Return canonical SQLite exchanges in Chroma similarity order."""

        if not isinstance(query, str):
            raise TypeError("query must be a non-empty string")
        if not query.strip():
            raise ValueError("query must not be empty")
        if isinstance(n_results, bool) or not isinstance(n_results, int):
            raise TypeError("n_results must be an integer")
        if n_results < 1:
            raise ValueError("n_results must be at least 1")
        if session_id is not None:
            if not isinstance(session_id, str):
                raise TypeError("session_id must be a non-empty string")
            if not session_id.strip():
                raise ValueError("session_id must not be empty")
        kwargs: dict[str, Any] = {
            "query_embeddings": [self._embed(query)],
            "n_results": n_results,
            "include": ["metadatas"],
        }
        if session_id is not None:
            kwargs["where"] = {"session_id": session_id}
        try:
            result = self._collection.query(**kwargs)
        except Exception as exc:
            raise ChatHistoryIndexError("chat-history search failed") from exc
        return self._hydrate(result, session_id)

    def _hydrate(
        self, result: Mapping[str, Any], session_filter: str | None
    ) -> list[ExchangeRecord]:
        try:
            metadata_rows = result["metadatas"][0]
        except (KeyError, IndexError, TypeError) as exc:
            raise ChatHistoryIndexError("Chroma result is missing metadata") from exc
        hydrated: list[ExchangeRecord] = []
        for index, metadata in enumerate(metadata_rows):
            if not isinstance(metadata, Mapping):
                raise ChatHistoryIndexError(f"invalid metadata for result {index}")
            exchange_id = metadata.get("exchange_id")
            session_id = metadata.get("session_id")
            if (
                isinstance(exchange_id, bool)
                or not isinstance(exchange_id, int)
                or exchange_id <= 0
            ):
                raise ChatHistoryIndexError(f"invalid exchange_id for result {index}")
            if not isinstance(session_id, str) or not session_id.strip():
                raise ChatHistoryIndexError(f"invalid session_id for result {index}")
            if session_filter is not None and session_id != session_filter:
                raise ChatHistoryIndexError(f"session filter mismatch for result {index}")
            try:
                hydrated.append(self._history.get_exchange(exchange_id, session_id))
            except (KeyError, RuntimeError, TypeError, ValueError) as exc:
                raise ChatHistoryIndexError(
                    f"failed to hydrate exchange {exchange_id} for result {index}"
                ) from exc
        return hydrated


__all__ = [
    "ChatHistoryIndex",
    "ChatHistoryIndexError",
    "DEFAULT_CHAT_COLLECTION_NAME",
    "build_exchange_context",
    "exchange_vector_id",
]