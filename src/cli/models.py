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
import json
import math
import re
from dataclasses import dataclass
from typing import Any, Sequence

from ..chat.agent_loop import AgentLoop, ChatOpenAIConfig
from ..retrieval.chat_history_index import ChatHistoryIndex
from ..storage.chroma_persistence import open_facts_collection
from ..retrieval.embeddings import EmbeddingConfig
from ..evaluation.runner import JUDGE_CRITERIA, EvaluationRunner
from ..indexing.fact_extraction import FactExtractor
from ..storage.graph_store import CozoGraphStore, stop_lock_owner
from ..indexing.paper_indexer import PaperIndexer
from ..indexing.paper_metadata import PaperMetadataExtractor
from ..retrieval.paper_search import PaperSearchIndex
from ..storage.sqlite_history import SQLiteHistoryStore
from .config import CLIConfig

_EMBED_DIM = 256
_GRAPH_DATABASE_FILENAME = "paper_graph.db"


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
        stage_markers = {
            text.splitlines()[0]
            for message in messages
            if (text := str(getattr(message, "content", message))).startswith(
                "MULTIHOP_"
            )
        }
        last = messages[-1] if messages else None
        user_text = getattr(last, "content", "") if last is not None else ""
        if not isinstance(user_text, str):
            user_text = str(user_text)
        first_text = str(getattr(messages[0], "content", "")) if messages else ""
        if first_text.startswith("You are an evaluation judge"):
            extras = {
                "relevant_chunk_ranking": ("relevant_chunk_ranks", []),
                "context_utilization": ("chunks_used", []),
                "semantic_similarity": ("contradicting_claims", []),
                "completeness": ("missing_elements", []),
                "hallucination_rate": ("unsupported_claims", []),
            }
            scores = {
                criterion: {
                    "score": 3,
                    "confidence": "low",
                    "reasoning": "Deterministic offline evaluation.",
                    "flags": ["offline_mock"],
                }
                for criterion in JUDGE_CRITERIA
            }
            for criterion, (name, value) in extras.items():
                scores[criterion][name] = value
            return json.dumps(scores)
        if "MULTIHOP_PLANNER" in stage_markers:
            question = user_text.strip()
            return json.dumps(
                {
                    "subqueries": [
                        f"Evidence for {question}",
                    ]
                }
            )
        if "MULTIHOP_SUBANSWER" in stage_markers:
            return "[offline mock evidence summary] " + user_text.strip()
        if "MULTIHOP_COVERAGE" in stage_markers:
            return json.dumps({"complete": True, "follow_up_subqueries": []})
        if "MULTIHOP_COMPOSER" in stage_markers:
            try:
                question = json.loads(user_text)["original_question"]
            except (json.JSONDecodeError, KeyError, TypeError):
                question = user_text
            return "[offline mock reply] " + str(question).strip()
        if user_text.startswith("METADATA_EXTRACTION_INPUT"):
            paper_text = user_text.partition("<paper_text>\n")[2].partition(
                "\n</paper_text>"
            )[0]
            title = next(
                (line.strip() for line in paper_text.splitlines() if line.strip()),
                "Untitled paper",
            )
            return json.dumps(
                {
                    "title": title[:200],
                    "authors": ["Unknown author"],
                    "topics": ["research", "paper", "local corpus"],
                }
            )
        if user_text.startswith("FACT_EXTRACTION_INPUT"):
            payload = user_text.partition("<chunks_json>\n")[2].partition(
                "\n</chunks_json>"
            )[0]
            try:
                chunks = json.loads(payload)
            except (json.JSONDecodeError, TypeError):
                chunks = []
            return json.dumps(
                {
                    "chunks": [
                        {"chunk_index": index, "facts": []}
                        for index, _chunk in enumerate(chunks)
                    ]
                }
            )
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
    graph_store = None
    try:
        graph_store = CozoGraphStore(
            config.chroma_directory.parent / _GRAPH_DATABASE_FILENAME
        )
        facts_collection = open_facts_collection(config.chroma_directory)
        if config.has_api_key:
            return PaperSearchIndex(
                persist_directory=config.chroma_directory,
                embedding_config=_embedding_config(config),
                graph_store=graph_store,
                facts_collection=facts_collection,
            )
        return PaperSearchIndex(
            persist_directory=config.chroma_directory,
            embedder=make_local_embedder(),
            graph_store=graph_store,
            facts_collection=facts_collection,
        )
    except Exception:
        if graph_store is not None:
            graph_store.close()
        return None


def stop_graph_store_owner(config: CLIConfig) -> bool:
    """Stop another backend process holding the paper graph database lock."""

    return stop_lock_owner(config.chroma_directory.parent / _GRAPH_DATABASE_FILENAME)


def build_paper_indexer(config: CLIConfig):
    """Build a paper indexer for the configured directory, or ``None``."""

    if not chromadb_available():
        return None
    graph_store = None
    try:
        graph_store = CozoGraphStore(
            config.chroma_directory.parent / _GRAPH_DATABASE_FILENAME
        )
        if config.has_api_key:
            return PaperIndexer(
                config.papers_directory,
                persist_directory=config.chroma_directory,
                embedding_config=_embedding_config(config),
                metadata_extractor=PaperMetadataExtractor(
                    config=ChatOpenAIConfig(
                        model=config.chat_model,
                        base_url=config.chat_base_url,
                        api_key=config.api_key,
                        max_tokens=config.chat_max_tokens,
                    )
                ),
                fact_extractor=FactExtractor(
                    config=ChatOpenAIConfig(
                        model=config.chat_model,
                        base_url=config.chat_base_url,
                        api_key=config.api_key,
                        max_tokens=config.chat_max_tokens,
                    )
                ),
                graph_store=graph_store,
                artifacts_directory=config.conversion_directory,
                max_workers=config.index_max_workers,
            )
        return PaperIndexer(
            config.papers_directory,
            persist_directory=config.chroma_directory,
            embedder=make_local_embedder(),
            metadata_extractor=PaperMetadataExtractor(model=MockChatModel()),
            fact_extractor=FactExtractor(model=MockChatModel()),
            graph_store=graph_store,
            artifacts_directory=config.conversion_directory,
            max_workers=config.index_max_workers,
        )
    except Exception:
        if graph_store is not None:
            graph_store.close()
        return None


def _chat_model(config: CLIConfig) -> tuple[Any, ChatOpenAIConfig | None]:
    """Return the offline mock model, or a provider config when a key is set."""

    if not config.has_api_key:
        return MockChatModel(), None
    return None, ChatOpenAIConfig(
        model=config.chat_model,
        base_url=config.chat_base_url,
        api_key=config.api_key,
        max_tokens=config.chat_max_tokens,
    )


def _prompt_kwargs(config: CLIConfig) -> dict[str, Any]:
    return {"system_prompt": config.system_prompt} if config.system_prompt else {}


def build_evaluation_runner(config: CLIConfig) -> EvaluationRunner:
    """Build the stateless evaluation runner from backend-only configuration."""

    from ..evaluation.metrics import EvaluationPricing

    retriever = build_paper_search_index(config)
    if retriever is None:
        raise RuntimeError("paper search index unavailable")
    model, chat_config = _chat_model(config)
    return EvaluationRunner(
        retriever,
        model,
        config=chat_config,
        pricing=EvaluationPricing.from_env(),
        **_prompt_kwargs(config),
    )


def build_agent_loop(
    config: CLIConfig, history_store, history_index, paper_retriever=None
) -> AgentLoop:
    """Construct the agent loop with chat history and required paper retrieval."""

    model, chat_config = _chat_model(config)
    return AgentLoop(
        history_store=history_store,
        model=model,
        history_index=history_index,
        paper_retriever=paper_retriever,
        config=chat_config,
        **_prompt_kwargs(config),
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
    "stop_graph_store_owner",
]
