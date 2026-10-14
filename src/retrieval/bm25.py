"""Pure-Python Okapi BM25 lexical scoring for hybrid retrieval.

The retrieval modules first pull candidate chunks from Chroma by vector
similarity, then re-rank them with a lexical signal so exact keyword matches
that embeddings rank lower can still surface.  This module supplies that
lexical signal without a third-party dependency: :func:`bm25_scores` returns one
Okapi BM25 score per document, computed over the supplied candidate set.
"""

from __future__ import annotations

import math
import re
from collections import Counter
from collections.abc import Sequence

DEFAULT_K1 = 1.5
DEFAULT_B = 0.75

_TOKEN_RE = re.compile(r"[a-z0-9]+")


def tokenize(text: str) -> list[str]:
    """Lowercase ``text`` and split it into alphanumeric word tokens."""

    if not isinstance(text, str):
        return []
    return _TOKEN_RE.findall(text.lower())


def bm25_scores(
    query: str,
    documents: Sequence[str],
    *,
    k1: float = DEFAULT_K1,
    b: float = DEFAULT_B,
) -> list[float]:
    """Return one Okapi BM25 score per document, ranked against ``query``.

    IDF is computed over ``documents`` themselves (the retrieved candidate set),
    so scores are only comparable within a single call.  A document with no
    query-term overlap scores ``0.0``.  An empty document set or query yields an
    all-zero list of matching length.
    """

    docs = list(documents)
    if not docs:
        return []
    query_terms = tokenize(query)
    if not query_terms:
        return [0.0] * len(docs)

    doc_terms = [tokenize(document) for document in docs]
    doc_lengths = [len(terms) for terms in doc_terms]
    total_length = sum(doc_lengths)
    if total_length == 0:
        return [0.0] * len(docs)
    avg_length = total_length / len(docs)

    document_frequency: Counter[str] = Counter()
    for terms in doc_terms:
        for term in set(terms):
            document_frequency[term] += 1

    n_docs = len(docs)
    idf = {
        term: math.log(1.0 + (n_docs - freq + 0.5) / (freq + 0.5))
        for term, freq in document_frequency.items()
    }

    unique_query_terms = set(query_terms)
    scores: list[float] = []
    for terms, length in zip(doc_terms, doc_lengths):
        if not terms:
            scores.append(0.0)
            continue
        frequencies = Counter(terms)
        score = 0.0
        for term in unique_query_terms:
            term_frequency = frequencies.get(term, 0)
            if term_frequency == 0:
                continue
            denominator = term_frequency + k1 * (1.0 - b + b * length / avg_length)
            score += idf[term] * (term_frequency * (k1 + 1.0)) / denominator
        scores.append(score)
    return scores


__all__ = ["bm25_scores", "tokenize", "DEFAULT_K1", "DEFAULT_B"]
