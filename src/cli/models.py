"""Model and index construction for the CLI bridge.

When an API key is configured, chat uses a real OpenAI/OpenRouter model and
search embeds with the OpenAI-compatible backend.  Otherwise chat falls back to
a deterministic mock model and search uses a local, deterministic embedder so
the desktop app always runs offline.  Chroma is used when installed; when it is
absent, index construction returns ``None`` and search degrades gracefully.
"""

from __future__ import annotations

import hashlib
import importlib.util
import math
import re
from dataclasses import dataclass
from typing import Any, Sequence

from ..agent_loop import AgentLoop, ChatOpenAIConfig
from ..chat_history_index import ChatHistoryIndex
from ..embeddings import EmbeddingConfig
from ..evaluation_runner import EvaluationRunner
from ..paper_indexer import PaperIndexer
from ..paper_search import PaperSearchIndex
from ..sqlite_history import SQLiteHistoryStore
from .config import CLIConfig

_EMBED_DIM = 256


@dataclass
class _MockResponse:
    """Minimal response object exposing ``content`` like a LangChain message."""

    content: str


class MockChatModel:
    """Deterministic offline chat model used when no API key is configured."""

    def invoke(self, messages: Sequence[Any]) -> _MockResponse:
        return _MockResponse(content=self._reply(messages))

    def stream(self, messages: Sequence[Any]):
        """Yield the mock reply in word-sized chunks so the UI can stream it."""

        reply = self._reply(messages)
        for token in re.findall(r"\S+\s*", reply) or [reply]:
            yield _MockResponse(content=token)

    @staticmethod
    def _reply(messages: Sequence[Any]) -> str:
        last = messages[-1] if messages else None
        user_text = getattr(last, "content", "") if last is not None else ""
        if not isinstance(user_text, str):
            user_text = str(user_text)
        return (
            "[offline mock reply] No API key is configured, so this is a "
            "deterministic response. You said: " + user_text.strip()
        )


def make_local_embedder(dim: int = _EMBED_DIM):
    """Return a deterministic hashing embedder mapping text to a unit vector."""

    def embed(text: str) -> list[float]:
        vector = [0.0] * dim
        for token in text.lower().split():
            digest = hashlib.sha256(token.encode("utf-8")).digest()
            bucket = int.from_bytes(digest[:4], "big") % dim
            vector[bucket] += 1.0 if digest[4] & 1 else -1.0
        norm = math.sqrt(sum(value * value for value in vector))
        if norm == 0.0:
            vector[0] = 1.0
            return vector
        return [value / norm for value in vector]

    return embed


class _SafeIndex:
    """Wrap a ``ChatHistoryIndex`` so indexing failures never break a chat turn."""

    def __init__(self, index: ChatHistoryIndex) -> None:
        self._index = index

    def index_exchange(self, exchange_id: int, session_id: str) -> None:
        try:
            self._index.index_exchange(exchange_id, session_id)
        except Exception:
            pass

    def search(self, query: str, n_results: int = 5, *, session_id=None):
        return self._index.search(query, n_results=n_results, session_id=session_id)


def chromadb_available() -> bool:
    """Return ``True`` when the optional ``chromadb`` package is importable."""

    return importlib.util.find_spec("chromadb") is not None


def build_history_store(config: CLIConfig) -> SQLiteHistoryStore:
    """Open the canonical SQLite history store (creating parent dirs)."""

    return SQLiteHistoryStore(config.database_path)


def build_index(config: CLIConfig, history_store: SQLiteHistoryStore):
    """Build a Chroma-backed chat-history index, or ``None`` when unavailable."""

    if not chromadb_available():
        return None
    try:
        if config.has_api_key:
            embedding_config = EmbeddingConfig(
                model=config.embedding_model,
                base_url=config.embedding_base_url,
                api_key=config.api_key,
            )
            index = ChatHistoryIndex(
                history_store,
                persist_directory=config.chroma_directory,
                embedding_config=embedding_config,
            )
        else:
            index = ChatHistoryIndex(
                history_store,
                persist_directory=config.chroma_directory,
                embedder=make_local_embedder(),
            )
    except Exception:
        return None
    return _SafeIndex(index)


def _embedding_config(config: CLIConfig) -> EmbeddingConfig:
    """Build the OpenAI-compatible embedding config from the CLI config."""

    return EmbeddingConfig(
        model=config.embedding_model,
        base_url=config.embedding_base_url,
        api_key=config.api_key,
    )


def build_paper_search_index(config: CLIConfig):
    """Build a Chroma-backed paper search index, or ``None`` when unavailable.

    Uses the OpenAI-compatible embedder when an API key is configured and the
    deterministic local embedder otherwise, matching :func:`build_paper_indexer`
    so indexing and search share the same vector space.
    """

    if not chromadb_available():
        return None
    try:
        if config.has_api_key:
            return PaperSearchIndex(
                persist_directory=config.chroma_directory,
                embedding_config=_embedding_config(config),
            )
        return PaperSearchIndex(
            persist_directory=config.chroma_directory,
            embedder=make_local_embedder(),
        )
    except Exception:
        return None


def build_paper_indexer(config: CLIConfig):
    """Build a paper indexer for the configured directory, or ``None``."""

    if not chromadb_available():
        return None
    try:
        if config.has_api_key:
            return PaperIndexer(
                config.papers_directory,
                persist_directory=config.chroma_directory,
                embedding_config=_embedding_config(config),
            )
        return PaperIndexer(
            config.papers_directory,
            persist_directory=config.chroma_directory,
            embedder=make_local_embedder(),
        )
    except Exception:
        return None


def build_evaluation_runner(config: CLIConfig) -> EvaluationRunner:
    """Build the stateless evaluation runner from backend-only configuration."""

    retriever = build_paper_search_index(config)
    if retriever is None:
        raise RuntimeError("paper search index unavailable")
    if config.has_api_key:
        model = None
        chat_config = ChatOpenAIConfig(
            model=config.chat_model,
            base_url=config.chat_base_url,
            api_key=config.api_key,
            max_tokens=config.chat_max_tokens,
        )
    else:
        model = MockChatModel()
        chat_config = None
    return EvaluationRunner(retriever, model, config=chat_config)


def build_agent_loop(
    config: CLIConfig, history_store, history_index, paper_retriever=None
) -> AgentLoop:
    """Construct the agent loop with chat history and required paper retrieval."""

    if config.has_api_key:
        model = None
        chat_config = ChatOpenAIConfig(
            model=config.chat_model,
            base_url=config.chat_base_url,
            api_key=config.api_key,
        )
    else:
        model = MockChatModel()
        chat_config = None
    kwargs: dict[str, Any] = {}
    if config.system_prompt:
        kwargs["system_prompt"] = config.system_prompt
    return AgentLoop(
        history_store=history_store,
        model=model,
        history_index=history_index,
        paper_retriever=paper_retriever,
        config=chat_config,
        **kwargs,
    )


__all__ = [
    "MockChatModel",
    "build_agent_loop",
    "build_evaluation_runner",
    "build_history_store",
    "build_index",
    "build_paper_indexer",
    "build_paper_search_index",
    "chromadb_available",
    "make_local_embedder",
]
