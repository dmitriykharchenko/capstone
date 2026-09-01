from __future__ import annotations

import pytest

from src.embeddings import (
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


def test_create_embedding_uses_injected_backend() -> None:
    backend = FakeBackend()

    result = create_embedding(
        "A paragraph", config=EmbeddingConfig(model="test-model"), backend=backend
    )

    assert result == [0.1, -0.2, 0.3]
    assert backend.calls[0][0] == "A paragraph"
    assert backend.calls[0][1].model == "test-model"


@pytest.mark.parametrize("value", [None, 42, ["text"]])
def test_create_embedding_rejects_non_string_input(value: object) -> None:
    with pytest.raises(TypeError, match="text must be a string"):
        create_embedding(value)  # type: ignore[arg-type]


def test_create_embedding_rejects_empty_input() -> None:
    with pytest.raises(ValueError, match="must not be empty"):
        create_embedding(" \n\t")


def test_create_embedding_wraps_backend_failure() -> None:
    backend = FakeBackend(error=RuntimeError("service unavailable"))

    with pytest.raises(EmbeddingError, match="Embedding backend failed: service unavailable"):
        create_embedding("text", backend=backend)


def test_create_embedding_rejects_malformed_backend_vector() -> None:
    with pytest.raises(EmbeddingError, match="only numbers"):
        create_embedding("text", backend=FakeBackend(vector=[1.0, "bad"]))  # type: ignore[list-item]