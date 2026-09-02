"""Semantic search over the Chroma-indexed paper collection.

Companion read-side to :mod:`src.paper_indexer`.  The indexer writes per-chunk
records into the shared ``documents`` collection; this module embeds a query,
ranks chunks by a hybrid of vector similarity and BM25 lexical score, and
collapses them to one best match per paper so the UI can list relevant papers
rather than raw chunks.
"""

from __future__ import annotations

import math
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from numbers import Real
from pathlib import Path
from typing import Any

from .bm25 import bm25_scores
from .chroma_persistence import DEFAULT_COLLECTION_NAME, open_collection
from .embeddings import EmbeddingBackend, EmbeddingConfig, create_embedding

# Weight of the (normalized) vector-similarity signal when blended with the
# (normalized) BM25 lexical signal; the lexical signal takes the remainder.
HYBRID_VECTOR_WEIGHT = 0.5


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


@dataclass(frozen=True)
class PaperChunkMatch:
    """One raw indexed chunk with complete paper provenance and stable rank."""

    rank: int
    text: str
    score: float
    paper_id: str
    chunk_index: int
    filename: str
    title: str
    page: int


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
        return _group_by_paper(result, query, n_results)

    def retrieve_chunks(
        self, query: str, n_results: int = 5
    ) -> list[PaperChunkMatch]:
        """Return at most ``n_results`` raw chunks by hybrid vector + BM25 rank."""

        _validate_query(query, n_results)
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
            raise PaperSearchError("paper chunk retrieval failed") from exc
        return _raw_chunks(result, query, n_results)


def _validate_query(query: str, n_results: int) -> None:
    if not isinstance(query, str):
        raise TypeError("query must be a non-empty string")
    if not query.strip():
        raise ValueError("query must not be empty")
    if isinstance(n_results, bool) or not isinstance(n_results, int):
        raise TypeError("n_results must be an integer")
    if n_results < 1:
        raise ValueError("n_results must be at least 1")


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


def _min_max_normalize(values: Sequence[float]) -> list[float]:
    """Scale ``values`` into ``[0, 1]``; all-equal inputs map to ``1.0``."""

    if not values:
        return []
    low = min(values)
    high = max(values)
    spread = high - low
    if spread <= 0:
        return [1.0 for _ in values]
    return [(value - low) / spread for value in values]


def _max_normalize(values: Sequence[float]) -> list[float]:
    """Scale ``values`` by their maximum; a non-positive maximum maps to ``0``."""

    if not values:
        return []
    high = max(values)
    if high <= 0:
        return [0.0 for _ in values]
    return [value / high for value in values]


def _hybrid_scores(
    similarities: Sequence[float], lexical: Sequence[float]
) -> list[float]:
    """Blend normalized vector similarity with normalized BM25 lexical scores."""

    vector_norm = _min_max_normalize(similarities)
    lexical_norm = _max_normalize(lexical)
    weight = HYBRID_VECTOR_WEIGHT
    return [
        round(weight * vector + (1.0 - weight) * keyword, 4)
        for vector, keyword in zip(vector_norm, lexical_norm)
    ]


def _raw_chunks(
    result: Mapping[str, Any], query: str, n_results: int
) -> list[PaperChunkMatch]:
    metadatas = _first_row(result, "metadatas")
    documents = _first_row(result, "documents")
    distances = _first_row(result, "distances")
    if not metadatas and not documents and not distances:
        return []
    if len(metadatas) != len(documents) or len(documents) != len(distances):
        raise PaperSearchError("paper chunk retrieval returned incomplete records")

    candidates: list[dict[str, Any]] = []
    for metadata, document, distance in zip(metadatas, documents, distances):
        if not isinstance(metadata, Mapping):
            raise PaperSearchError("paper chunk metadata is missing")
        paper_id = _required_text(metadata, "paper_id")
        chunk_index = _required_int(metadata, "chunk_index", minimum=0)
        filename = _required_text(metadata, "filename")
        title = _required_text(metadata, "title")
        page = _required_int(metadata, "page", minimum=1)
        if not isinstance(document, str) or not document.strip():
            raise PaperSearchError("paper chunk text is missing")
        if (
            isinstance(distance, bool)
            or not isinstance(distance, Real)
            or not math.isfinite(float(distance))
            or float(distance) < 0
        ):
            raise PaperSearchError("paper chunk distance is invalid")
        candidates.append(
            {
                "text": document,
                "similarity": _similarity(distance),
                "paper_id": paper_id,
                "chunk_index": chunk_index,
                "filename": filename,
                "title": title,
                "page": page,
            }
        )
    if not candidates:
        return []

    scores = _hybrid_scores(
        [candidate["similarity"] for candidate in candidates],
        bm25_scores(query, [candidate["text"] for candidate in candidates]),
    )
    order = sorted(range(len(candidates)), key=lambda index: scores[index], reverse=True)
    chunks: list[PaperChunkMatch] = []
    for rank, index in enumerate(order[:n_results], start=1):
        candidate = candidates[index]
        chunks.append(
            PaperChunkMatch(
                rank=rank,
                text=candidate["text"],
                score=scores[index],
                paper_id=candidate["paper_id"],
                chunk_index=candidate["chunk_index"],
                filename=candidate["filename"],
                title=candidate["title"],
                page=candidate["page"],
            )
        )
    return chunks


def _required_text(metadata: Mapping[str, Any], field: str) -> str:
    value = metadata.get(field)
    if not isinstance(value, str) or not value.strip():
        raise PaperSearchError(f"paper chunk {field} is missing")
    return value


def _required_int(metadata: Mapping[str, Any], field: str, *, minimum: int) -> int:
    value = metadata.get(field)
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise PaperSearchError(f"paper chunk {field} is invalid")
    return value


def _snippet(document: Any, limit: int = 240) -> str:
    if not isinstance(document, str):
        return ""
    text = " ".join(document.split())
    if len(text) <= limit:
        return text
    return text[:limit].rstrip() + "\u2026"


def _group_by_paper(
    result: Mapping[str, Any], query: str, n_results: int
) -> list[PaperMatch]:
    metadatas = _first_row(result, "metadatas")
    documents = _first_row(result, "documents")
    distances = _first_row(result, "distances")
    candidates: list[dict[str, Any]] = []
    for index, metadata in enumerate(metadatas):
        if not isinstance(metadata, Mapping):
            continue
        paper_id = metadata.get("paper_id")
        if not isinstance(paper_id, str) or not paper_id.strip():
            continue
        document = documents[index] if index < len(documents) else ""
        page = metadata.get("page")
        candidates.append(
            {
                "paper_id": paper_id,
                "title": str(metadata.get("title") or paper_id),
                "filename": str(metadata.get("filename") or f"{paper_id}.pdf"),
                "page": page if isinstance(page, int) and not isinstance(page, bool) else None,
                "document": document if isinstance(document, str) else "",
                "similarity": _similarity(distances[index] if index < len(distances) else None),
            }
        )
    if not candidates:
        return []

    scores = _hybrid_scores(
        [candidate["similarity"] for candidate in candidates],
        bm25_scores(query, [candidate["document"] for candidate in candidates]),
    )
    best: dict[str, PaperMatch] = {}
    for candidate, score in zip(candidates, scores):
        paper_id = candidate["paper_id"]
        if paper_id in best and best[paper_id].score >= score:
            continue
        best[paper_id] = PaperMatch(
            paper_id=paper_id,
            title=candidate["title"],
            filename=candidate["filename"],
            page=candidate["page"],
            snippet=_snippet(candidate["document"]),
            score=score,
        )
    ranked = sorted(best.values(), key=lambda match: match.score, reverse=True)
    return ranked[:n_results]


__all__ = [
    "PaperChunkMatch",
    "PaperMatch",
    "PaperSearchError",
    "PaperSearchIndex",
]
