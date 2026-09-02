"""Semantic search over the Chroma-indexed paper collection.

Companion read-side to :mod:`src.paper_indexer`.  The indexer writes per-chunk
records into the shared ``documents`` collection; this module embeds a query,
ranks chunks by similarity, and collapses them to one best match per paper so
the UI can list relevant papers rather than raw chunks.
"""

from __future__ import annotations

import math
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from numbers import Real
from pathlib import Path
from typing import Any

from .chroma_persistence import DEFAULT_COLLECTION_NAME, open_collection
from .embeddings import EmbeddingBackend, EmbeddingConfig, create_embedding


class PaperSearchError(RuntimeError):
    """Raised when a paper search query cannot be embedded or executed."""


@dataclass(frozen=True)
class PaperMatch:
    """One paper ranked by its most similar indexed chunk."""

    paper_id: str
    title: str
    filename: str
    page: int | None
    snippet: str
    score: float


class PaperSearchIndex:
    """Embed a query and rank papers by their most similar indexed chunk."""

    def __init__(
        self,
        *,
        collection: Any | None = None,
        persist_directory: str | Path | None = None,
        collection_name: str = DEFAULT_COLLECTION_NAME,
        embedding_config: EmbeddingConfig | None = None,
        embedding_backend: EmbeddingBackend | Any | None = None,
        embedder: Callable[[str], Sequence[float]] | None = None,
    ) -> None:
        if collection is None:
            if persist_directory is None:
                raise ValueError(
                    "persist_directory is required when collection is not provided"
                )
            collection = open_collection(persist_directory, collection_name)
        if not callable(getattr(collection, "query", None)):
            raise TypeError("collection must expose a query method")
        if embedder is not None and not callable(embedder):
            raise TypeError("embedder must be callable")
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
        if isinstance(values, (str, bytes)) or not isinstance(values, Sequence) or not values:
            raise PaperSearchError("embedder returned an empty or invalid vector")
        vector: list[float] = []
        for value in values:
            if isinstance(value, bool) or not isinstance(value, Real):
                raise PaperSearchError("embedder returned a non-numeric vector")
            number = float(value)
            if not math.isfinite(number):
                raise PaperSearchError("embedder returned a non-finite vector")
            vector.append(number)
        return vector

    def count(self) -> int:
        """Return the number of indexed chunks, or ``0`` when unavailable."""

        try:
            return int(self._collection.count())
        except Exception:
            return 0

    def search(self, query: str, n_results: int = 5) -> list[PaperMatch]:
        """Return papers in descending similarity to ``query``."""

        if not isinstance(query, str):
            raise TypeError("query must be a non-empty string")
        if not query.strip():
            raise ValueError("query must not be empty")
        if isinstance(n_results, bool) or not isinstance(n_results, int):
            raise TypeError("n_results must be an integer")
        if n_results < 1:
            raise ValueError("n_results must be at least 1")
        chunk_budget = min(max(n_results * 5, n_results), 50)
        try:
            result = self._collection.query(
                query_embeddings=[self._embed(query)],
                n_results=chunk_budget,
                include=["metadatas", "documents", "distances"],
            )
        except PaperSearchError:
            raise
        except Exception as exc:
            raise PaperSearchError("paper search failed") from exc
        return _group_by_paper(result, n_results)


def _first_row(result: Mapping[str, Any], key: str) -> list[Any]:
    value = result.get(key) if isinstance(result, Mapping) else None
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes)) or not value:
        return []
    row = value[0]
    if not isinstance(row, Sequence) or isinstance(row, (str, bytes)):
        return []
    return list(row)


def _similarity(distance: Any) -> float:
    if isinstance(distance, bool) or not isinstance(distance, Real):
        return 0.0
    number = float(distance)
    if not math.isfinite(number) or number < 0:
        return 0.0
    return round(1.0 / (1.0 + number), 4)


def _snippet(document: Any, limit: int = 240) -> str:
    if not isinstance(document, str):
        return ""
    text = " ".join(document.split())
    if len(text) <= limit:
        return text
    return text[:limit].rstrip() + "\u2026"


def _group_by_paper(result: Mapping[str, Any], n_results: int) -> list[PaperMatch]:
    metadatas = _first_row(result, "metadatas")
    documents = _first_row(result, "documents")
    distances = _first_row(result, "distances")
    best: dict[str, PaperMatch] = {}
    for index, metadata in enumerate(metadatas):
        if not isinstance(metadata, Mapping):
            continue
        paper_id = metadata.get("paper_id")
        if not isinstance(paper_id, str) or not paper_id.strip():
            continue
        score = _similarity(distances[index] if index < len(distances) else None)
        if paper_id in best and best[paper_id].score >= score:
            continue
        document = documents[index] if index < len(documents) else ""
        page = metadata.get("page")
        best[paper_id] = PaperMatch(
            paper_id=paper_id,
            title=str(metadata.get("title") or paper_id),
            filename=str(metadata.get("filename") or f"{paper_id}.pdf"),
            page=page if isinstance(page, int) and not isinstance(page, bool) else None,
            snippet=_snippet(document),
            score=score,
        )
    ranked = sorted(best.values(), key=lambda match: match.score, reverse=True)
    return ranked[:n_results]


__all__ = [
    "PaperMatch",
    "PaperSearchError",
    "PaperSearchIndex",
]
