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
from .chroma_persistence import DEFAULT_COLLECTION_NAME, get_records, open_collection
from .embeddings import EmbeddingBackend, EmbeddingConfig, create_embedding

# Weight of the (normalized) vector-similarity signal when blended with the
# (normalized) BM25 lexical signal; the lexical signal takes the remainder.
HYBRID_VECTOR_WEIGHT = 0.5
EVALUATION_RETRIEVAL_STRATEGIES = ("vector", "bm25", "hybrid")
EVALUATION_CHUNK_LIMIT = 5


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
        self,
        query: str,
        n_results: int = 5,
        *,
        strategy: str | None = None,
    ) -> list[PaperChunkMatch]:
        """Return raw chunks by the existing chat hybrid rank.

        Supplying ``strategy`` opts into the full-corpus evaluation interface;
        the default keeps the chat candidate budget and ranking unchanged.
        """

        if strategy is not None:
            return self.retrieve_evaluation_chunks(query, strategy, n_results)

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

    def retrieve_evaluation_chunks(
        self,
        query: str,
        strategy: str = "hybrid",
        n_results: int = EVALUATION_CHUNK_LIMIT,
    ) -> list[PaperChunkMatch]:
        """Retrieve up to five full-provenance chunks using an explicit strategy.

        ``vector`` uses Chroma's similarity query, ``bm25`` scores every indexed
        chunk returned by ``get()``, and ``hybrid`` combines both signals over
        the complete corpus.  The unfiltered ``get()`` call is intentional:
        Chroma returns all records when neither ``ids`` nor ``where`` is set.
        """

        _validate_query(query, n_results)
        if not isinstance(strategy, str) or strategy not in EVALUATION_RETRIEVAL_STRATEGIES:
            allowed = ", ".join(EVALUATION_RETRIEVAL_STRATEGIES)
            raise ValueError(f"strategy must be one of: {allowed}")
        limit = min(n_results, EVALUATION_CHUNK_LIMIT)
        try:
            if strategy == "vector":
                result = self._collection.query(
                    query_embeddings=[self._embed(query)],
                    n_results=limit,
                    include=["metadatas", "documents", "distances"],
                )
                return _rank_vector_candidates(_query_candidates(result), limit)

            corpus = _corpus_candidates(get_records(self._collection))
            if strategy == "bm25":
                scores = bm25_scores(query, [candidate["text"] for candidate in corpus])
                return _rank_scored_candidates(corpus, scores, limit)
            if not corpus:
                return []

            result = self._collection.query(
                query_embeddings=[self._embed(query)],
                n_results=len(corpus),
                include=["metadatas", "documents", "distances"],
            )
            vector_candidates = _query_candidates(result)
            if len(vector_candidates) != len(corpus):
                raise PaperSearchError("paper chunk retrieval returned incomplete records")
            lexical = bm25_scores(
                query, [candidate["text"] for candidate in vector_candidates]
            )
            scores = _hybrid_scores(
                [candidate["similarity"] for candidate in vector_candidates], lexical
            )
            return _rank_scored_candidates(vector_candidates, scores, limit)
        except PaperSearchError:
            raise
        except Exception as exc:
            raise PaperSearchError("paper evaluation retrieval failed") from exc


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


def _validated_candidate(
    metadata: Any,
    document: Any,
    distance: Any = 0.0,
    record_id: Any = "",
) -> dict[str, Any]:
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
    return {
        "text": document,
        "similarity": _similarity(distance),
        "paper_id": paper_id,
        "chunk_index": chunk_index,
        "filename": filename,
        "title": title,
        "page": page,
        "id": record_id if isinstance(record_id, str) else "",
    }


def _query_candidates(result: Mapping[str, Any]) -> list[dict[str, Any]]:
    metadatas = _first_row(result, "metadatas")
    documents = _first_row(result, "documents")
    distances = _first_row(result, "distances")
    ids = _first_row(result, "ids")
    if not metadatas and not documents and not distances:
        return []
    if len(metadatas) != len(documents) or len(documents) != len(distances):
        raise PaperSearchError("paper chunk retrieval returned incomplete records")
    return [
        _validated_candidate(
            metadata,
            document,
            distance,
            ids[index] if index < len(ids) else "",
        )
        for index, (metadata, document, distance) in enumerate(
            zip(metadatas, documents, distances)
        )
    ]


def _flat_values(result: Mapping[str, Any], key: str) -> list[Any]:
    value = result.get(key) if isinstance(result, Mapping) else None
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes)):
        return []
    values = list(value)
    if len(values) == 1 and isinstance(values[0], Sequence) and not isinstance(
        values[0], (str, bytes)
    ):
        return list(values[0])
    return values


def _corpus_candidates(result: Mapping[str, Any]) -> list[dict[str, Any]]:
    metadatas = _flat_values(result, "metadatas")
    documents = _flat_values(result, "documents")
    ids = _flat_values(result, "ids")
    if not metadatas and not documents:
        return []
    if len(metadatas) != len(documents):
        raise PaperSearchError("paper chunk corpus returned incomplete records")
    return [
        _validated_candidate(
            metadata,
            document,
            0.0,
            ids[index] if index < len(ids) else "",
        )
        for index, (metadata, document) in enumerate(zip(metadatas, documents))
    ]


def _candidate_key(candidate: Mapping[str, Any]) -> tuple[Any, ...]:
    return (
        candidate["paper_id"],
        candidate["chunk_index"],
        candidate["filename"],
        candidate["page"],
        candidate.get("id", ""),
    )


def _to_chunk(candidate: Mapping[str, Any], score: float, rank: int) -> PaperChunkMatch:
    return PaperChunkMatch(
        rank=rank,
        text=candidate["text"],
        score=score,
        paper_id=candidate["paper_id"],
        chunk_index=candidate["chunk_index"],
        filename=candidate["filename"],
        title=candidate["title"],
        page=candidate["page"],
    )


def _rank_vector_candidates(
    candidates: Sequence[Mapping[str, Any]], limit: int
) -> list[PaperChunkMatch]:
    ranked = sorted(
        candidates,
        key=lambda candidate: (-candidate["similarity"], _candidate_key(candidate)),
    )
    return [
        _to_chunk(candidate, candidate["similarity"], rank)
        for rank, candidate in enumerate(ranked[:limit], start=1)
    ]


def _rank_scored_candidates(
    candidates: Sequence[Mapping[str, Any]], scores: Sequence[float], limit: int
) -> list[PaperChunkMatch]:
    ranked = sorted(
        zip(candidates, scores),
        key=lambda item: (-item[1], _candidate_key(item[0])),
    )
    return [
        _to_chunk(candidate, float(score), rank)
        for rank, (candidate, score) in enumerate(ranked[:limit], start=1)
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
    "EVALUATION_CHUNK_LIMIT",
    "EVALUATION_RETRIEVAL_STRATEGIES",
    "PaperChunkMatch",
    "PaperMatch",
    "PaperSearchError",
    "PaperSearchIndex",
]
