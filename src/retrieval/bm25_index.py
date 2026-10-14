"""Persisted Okapi BM25 index built once at paper-index time.

The on-the-fly :func:`src.retrieval.bm25.bm25_scores` retokenizes the whole corpus and
recomputes IDF on every query.  Because the CLI bridge spawns a fresh process
per query, that work cannot be cached in memory, so this module precomputes the
per-document term frequencies, lengths, and corpus document-frequency table
during indexing and persists them next to the Chroma store.  At query time the
retriever loads this index and only tokenizes the query, scoring against the
cached statistics with the exact same formula as :func:`bm25_scores`.
"""

from __future__ import annotations

import json
import math
import os
from collections import Counter
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

from .bm25 import DEFAULT_B, DEFAULT_K1, tokenize

INDEX_VERSION = 1
_PROVENANCE_KEYS = ("paper_id", "chunk_index", "filename", "title", "page", "id")


class BM25Index:
    """Precomputed BM25 statistics over a fixed corpus of chunk candidates."""

    def __init__(
        self,
        candidates: Sequence[Mapping[str, Any]],
        term_freqs: Sequence[Mapping[str, int]],
        lengths: Sequence[int],
        document_frequency: Mapping[str, int],
        *,
        k1: float = DEFAULT_K1,
        b: float = DEFAULT_B,
    ) -> None:
        self.candidates = [dict(candidate) for candidate in candidates]
        self._term_freqs = [dict(freqs) for freqs in term_freqs]
        self._lengths = [int(length) for length in lengths]
        self._document_frequency = dict(document_frequency)
        self._k1 = float(k1)
        self._b = float(b)

    @property
    def chunk_count(self) -> int:
        return len(self.candidates)

    @classmethod
    def build(
        cls,
        candidates: Sequence[Mapping[str, Any]],
        *,
        k1: float = DEFAULT_K1,
        b: float = DEFAULT_B,
    ) -> "BM25Index":
        """Tokenize every candidate once and precompute the corpus statistics."""

        term_freqs: list[dict[str, int]] = []
        lengths: list[int] = []
        document_frequency: Counter[str] = Counter()
        for candidate in candidates:
            terms = tokenize(candidate.get("text", ""))
            frequencies = Counter(terms)
            term_freqs.append(dict(frequencies))
            lengths.append(len(terms))
            for term in frequencies:
                document_frequency[term] += 1
        return cls(
            candidates,
            term_freqs,
            lengths,
            dict(document_frequency),
            k1=k1,
            b=b,
        )

    def score(self, query: str) -> list[float]:
        """Return one BM25 score per candidate, matching ``bm25_scores`` exactly."""

        n_docs = len(self.candidates)
        if n_docs == 0:
            return []
        query_terms = tokenize(query)
        if not query_terms:
            return [0.0] * n_docs
        total_length = sum(self._lengths)
        if total_length == 0:
            return [0.0] * n_docs
        avg_length = total_length / n_docs
        unique_query_terms = set(query_terms)
        idf = {
            term: math.log(
                1.0
                + (n_docs - freq + 0.5) / (freq + 0.5)
            )
            for term in unique_query_terms
            if (freq := self._document_frequency.get(term, 0)) > 0
        }
        scores: list[float] = []
        for frequencies, length in zip(self._term_freqs, self._lengths):
            if length == 0:
                scores.append(0.0)
                continue
            score = 0.0
            for term in unique_query_terms:
                term_frequency = frequencies.get(term, 0)
                if term_frequency == 0:
                    continue
                denominator = term_frequency + self._k1 * (
                    1.0 - self._b + self._b * length / avg_length
                )
                score += idf[term] * (term_frequency * (self._k1 + 1.0)) / denominator
            scores.append(score)
        return scores

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": INDEX_VERSION,
            "k1": self._k1,
            "b": self._b,
            "document_frequency": self._document_frequency,
            "documents": [
                {
                    "provenance": {key: candidate.get(key) for key in _PROVENANCE_KEYS},
                    "text": candidate.get("text", ""),
                    "term_freqs": frequencies,
                    "length": length,
                }
                for candidate, frequencies, length in zip(
                    self.candidates, self._term_freqs, self._lengths
                )
            ],
        }

    @classmethod
    def from_dict(cls, payload: Mapping[str, Any]) -> "BM25Index":
        if not isinstance(payload, Mapping) or payload.get("version") != INDEX_VERSION:
            raise ValueError("unsupported or missing BM25 index version")
        documents = payload.get("documents")
        if not isinstance(documents, Sequence):
            raise ValueError("BM25 index documents must be a sequence")
        candidates: list[dict[str, Any]] = []
        term_freqs: list[dict[str, int]] = []
        lengths: list[int] = []
        for document in documents:
            provenance = document.get("provenance") if isinstance(document, Mapping) else None
            provenance = provenance if isinstance(provenance, Mapping) else {}
            candidate = {key: provenance.get(key) for key in _PROVENANCE_KEYS}
            candidate["text"] = document.get("text", "") if isinstance(document, Mapping) else ""
            candidates.append(candidate)
            raw_freqs = document.get("term_freqs") if isinstance(document, Mapping) else None
            term_freqs.append(
                {str(term): int(freq) for term, freq in raw_freqs.items()}
                if isinstance(raw_freqs, Mapping)
                else {}
            )
            length = document.get("length") if isinstance(document, Mapping) else 0
            lengths.append(int(length) if isinstance(length, int) and not isinstance(length, bool) else 0)
        return cls(
            candidates,
            term_freqs,
            lengths,
            {
                str(term): int(freq)
                for term, freq in (payload.get("document_frequency") or {}).items()
            },
            k1=float(payload.get("k1", DEFAULT_K1)),
            b=float(payload.get("b", DEFAULT_B)),
        )

    def save(self, path: str | Path) -> None:
        """Atomically write the index JSON so a concurrent reader never tears."""

        target = Path(path)
        target.parent.mkdir(parents=True, exist_ok=True)
        temporary = target.with_name(f"{target.name}.tmp")
        with temporary.open("w", encoding="utf-8") as handle:
            json.dump(self.to_dict(), handle)
        os.replace(temporary, target)

    @classmethod
    def load(cls, path: str | Path) -> "BM25Index | None":
        """Load a persisted index, or ``None`` when it is absent or unreadable."""

        target = Path(path)
        if not target.exists():
            return None
        try:
            with target.open("r", encoding="utf-8") as handle:
                payload = json.load(handle)
            return cls.from_dict(payload)
        except Exception:  # noqa: BLE001 - a bad file degrades to on-the-fly BM25
            return None


__all__ = ["BM25Index", "INDEX_VERSION"]
