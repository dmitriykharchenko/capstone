"""Create text embeddings through an OpenAI-compatible embedding service.

The OpenAI SDK is imported lazily, so importing this module does not require the
optional SDK or credentials.  Callers can inject a backend implementing
``embed`` in tests or when integrating another OpenAI-compatible client.
"""

from __future__ import annotations

import math
import os
from dataclasses import dataclass
from functools import lru_cache
from typing import Any, Protocol, Sequence, cast


class EmbeddingError(RuntimeError):
    """Raised when the embedding backend cannot create a vector."""


class EmbeddingConfigurationError(EmbeddingError, ValueError):
    """Raised when embedding configuration is missing or unsupported."""


@dataclass(frozen=True)
class EmbeddingConfig:
    """Configuration for an OpenAI-compatible embedding backend.

    ``api_key`` may be omitted when ``OPENAI_API_KEY`` is set in the runtime
    environment.  ``base_url`` can point at OpenRouter or another compatible
    service instead of the OpenAI default.
    """

    model: str = "text-embedding-3-small"
    backend: str = "openai"
    api_key: str | None = None
    base_url: str = "https://api.openai.com/v1"
    timeout: float = 30.0

    def __post_init__(self) -> None:
        if not isinstance(self.model, str) or not self.model.strip():
            raise EmbeddingConfigurationError("model must be a non-empty string")
        if not isinstance(self.backend, str) or not self.backend.strip():
            raise EmbeddingConfigurationError("backend must be a non-empty string")
        if not isinstance(self.base_url, str) or not self.base_url.strip():
            raise EmbeddingConfigurationError("base_url must be a non-empty string")
        if not isinstance(self.timeout, (int, float)) or isinstance(self.timeout, bool):
            raise EmbeddingConfigurationError("timeout must be a positive number")
        if not math.isfinite(float(self.timeout)) or self.timeout <= 0:
            raise EmbeddingConfigurationError("timeout must be a positive number")
        if self.api_key is not None and (
            not isinstance(self.api_key, str) or not self.api_key.strip()
        ):
            raise EmbeddingConfigurationError("api_key must be a non-empty string")


class EmbeddingBackend(Protocol):
    """Protocol implemented by injectable embedding backends."""

    def embed(self, text: str, config: EmbeddingConfig) -> Sequence[float]:
        """Return a numeric vector for ``text`` using ``config``."""


def _api_key(config: EmbeddingConfig) -> str:
    key = config.api_key or os.getenv("OPENAI_API_KEY") or os.getenv("OPENROUTER_API_KEY")
    if not key:
        raise EmbeddingConfigurationError(
            "No API key configured; pass EmbeddingConfig(api_key=...) or set "
            "OPENAI_API_KEY"
        )
    return key


@lru_cache(maxsize=16)
def _openai_client(api_key: str, base_url: str, timeout: float) -> Any:
    """Construct and cache one SDK client for each connection configuration."""

    try:
        from openai import OpenAI
    except ImportError as exc:
        raise EmbeddingConfigurationError(
            "The OpenAI-compatible backend requires the 'openai' package"
        ) from exc
    try:
        return OpenAI(api_key=api_key, base_url=base_url, timeout=timeout)
    except Exception as exc:
        raise EmbeddingConfigurationError("Unable to initialize embedding client") from exc


class OpenAIEmbeddingBackend:
    """Embedding backend backed by the OpenAI Python client."""

    def embed(self, text: str, config: EmbeddingConfig) -> Sequence[float]:
        """Create one embedding using the configured OpenAI-compatible service."""

        client = _openai_client(_api_key(config), config.base_url, float(config.timeout))
        response = client.embeddings.create(model=config.model, input=text)
        try:
            return cast(Sequence[float], response.data[0].embedding)
        except (AttributeError, IndexError, KeyError, TypeError) as exc:
            raise EmbeddingError("Embedding backend returned an invalid response") from exc


def _invoke_backend(
    backend: EmbeddingBackend | Any, text: str, config: EmbeddingConfig
) -> Sequence[float]:
    method = getattr(backend, "embed", None)
    if method is not None:
        try:
            return method(text, config)
        except TypeError as first_error:
            try:
                return method(text)
            except TypeError:
                raise first_error
    if callable(backend):
        return backend(text)
    raise EmbeddingConfigurationError("backend must provide embed(text, config) or be callable")


def _validate_vector(vector: Sequence[float]) -> list[float]:
    if isinstance(vector, (str, bytes)) or not isinstance(vector, Sequence) or not vector:
        raise EmbeddingError("Embedding backend returned an empty or invalid vector")
    result: list[float] = []
    for value in vector:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise EmbeddingError("Embedding vector must contain only numbers")
        if not math.isfinite(float(value)):
            raise EmbeddingError("Embedding vector must contain finite numbers")
        result.append(float(value))
    return result


def create_embedding(
    text: str,
    *,
    config: EmbeddingConfig | None = None,
    backend: EmbeddingBackend | Any | None = None,
) -> list[float]:
    """Create an embedding vector for one text string.

    Args:
        text: Non-empty text to embed.
        config: Optional model, backend, endpoint, and credential settings.
        backend: Optional injected backend (or callable) for testing or custom
            clients.  When omitted, :class:`OpenAIEmbeddingBackend` is used.

    Raises:
        TypeError: If ``text`` is not a string.
        ValueError: If ``text`` is empty or configuration is invalid.
        EmbeddingError: If the backend fails or returns a malformed vector.
    """

    if not isinstance(text, str):
        raise TypeError("text must be a string")
    if not text.strip():
        raise ValueError("text must not be empty")
    resolved_config = config or EmbeddingConfig()
    backend_name = resolved_config.backend.strip().lower().replace("_", "-")
    if backend is None:
        if backend_name not in {"openai", "openai-compatible"}:
            raise EmbeddingConfigurationError(
                f"Unsupported embedding backend: {resolved_config.backend}"
            )
        backend = OpenAIEmbeddingBackend()
    try:
        return _validate_vector(_invoke_backend(backend, text, resolved_config))
    except EmbeddingError:
        raise
    except Exception as exc:
        raise EmbeddingError(f"Embedding backend failed: {exc}") from exc


__all__ = [
    "EmbeddingBackend",
    "EmbeddingConfig",
    "EmbeddingConfigurationError",
    "EmbeddingError",
    "OpenAIEmbeddingBackend",
    "create_embedding",
]