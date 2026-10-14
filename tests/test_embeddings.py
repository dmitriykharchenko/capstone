from __future__ import annotations

import pytest

from src.retrieval.embeddings import (
    EmbeddingConfig,
    EmbeddingError,
    create_embedding,
)


class FakeBackend:
    def __init__(self, vector: list[float] | None = None, error: Exception | None = None):
        self.vector = vector if vector is not None else [0.1, -0.2, 0.3]
        self.error = error
        self.calls: list[tuple[str, EmbeddingConfig]] = []

    def embed(self, text: str, config: EmbeddingConfig) -> list[float]:
        self.calls.append((text, config))
        if self.error:
            raise self.error
        return self.vector


def test_create_embedding_wraps_backend_failure() -> None:
    backend = FakeBackend(error=RuntimeError("service unavailable"))

    with pytest.raises(EmbeddingError, match="Embedding backend failed: service unavailable"):
        create_embedding("text", backend=backend)
