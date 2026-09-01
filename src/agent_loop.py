"""A small synchronous LangChain chat loop with SQLite-backed history.

The loop deliberately handles only prompt construction, one synchronous model
invocation, and persistence of completed human/assistant turns.  LangChain is
optional at import time: applications that inject a compatible model can import
this module without installing the provider packages.
"""

from __future__ import annotations

import math
import os
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Any, Protocol, cast

from .sqlite_history import DatabasePath, MessageRecord, SQLiteHistoryStore

DEFAULT_SYSTEM_PROMPT = (
    "You are a helpful assistant. Answer the user's question accurately and "
    "concisely. If you are unsure, say so rather than guessing."
)


class AgentLoopError(RuntimeError):
    """Base error raised by the agent loop."""


class AgentConfigurationError(AgentLoopError, ValueError):
    """Raised when LangChain or model configuration is unavailable or invalid."""


class AgentModelError(AgentLoopError):
    """Raised when the injected chat model fails or returns invalid text."""


class ChatModel(Protocol):
    """Minimal synchronous interface required from an injectable chat model."""

    def invoke(self, messages: Sequence[Any]) -> Any:
        """Return a LangChain-compatible response for ``messages``."""


class ExchangeIndexer(Protocol):
    """Minimal interface for indexing a completed SQLite exchange."""

    def index_exchange(self, exchange_id: int, session_id: str) -> None:
        """Index the exchange after its SQLite transaction commits."""


@dataclass(frozen=True)
class ChatOpenAIConfig:
    """Configuration used when constructing the optional ``ChatOpenAI`` model.

    ``api_key`` may be omitted when ``OPENAI_API_KEY`` or
    ``OPENROUTER_API_KEY`` is set in the environment.  ``base_url`` can point
    at OpenRouter or another OpenAI-compatible service.
    """

    model: str = "gpt-4o-mini"
    base_url: str = "https://api.openai.com/v1"
    api_key: str | None = None
    temperature: float = 0.2
    timeout: float = 30.0

    def __post_init__(self) -> None:
        if not isinstance(self.model, str) or not self.model.strip():
            raise AgentConfigurationError("model must be a non-empty string")
        if not isinstance(self.base_url, str) or not self.base_url.strip():
            raise AgentConfigurationError("base_url must be a non-empty string")
        if self.api_key is not None and (
            not isinstance(self.api_key, str) or not self.api_key.strip()
        ):
            raise AgentConfigurationError("api_key must be a non-empty string")
        for name, value in (("temperature", self.temperature), ("timeout", self.timeout)):
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                raise AgentConfigurationError(f"{name} must be a positive number")
            if (
                not math.isfinite(float(value))
                or value < 0
                or (name == "timeout" and value == 0)
            ):
                raise AgentConfigurationError(f"{name} must be a positive number")


AgentConfig = ChatOpenAIConfig


def _load_message_types() -> tuple[Any, Any, Any, Any]:
    try:
        from langchain_core.messages import (
            AIMessage,
            ChatMessage,
            HumanMessage,
            SystemMessage,
        )
    except ImportError as exc:
        raise AgentConfigurationError(
            "langchain-core is required to run the agent loop; install "
            "langchain-core or inject a model after installing its message types"
        ) from exc
    return HumanMessage, AIMessage, SystemMessage, ChatMessage


def _resolve_api_key(config: ChatOpenAIConfig) -> str:
    key = config.api_key or os.getenv("OPENAI_API_KEY") or os.getenv("OPENROUTER_API_KEY")
    if not key:
        raise AgentConfigurationError(
            "No API key configured; pass ChatOpenAIConfig(api_key=...) or set "
            "OPENAI_API_KEY"
        )
    return key


def _create_chat_openai(config: ChatOpenAIConfig) -> ChatModel:
    try:
        from langchain_openai import ChatOpenAI
    except ImportError as exc:
        raise AgentConfigurationError(
            "langchain-openai is required for the default chat model; install it "
            "or inject a LangChain-compatible model"
        ) from exc
    try:
        return cast(
            ChatModel,
            ChatOpenAI(
                model=config.model,
                base_url=config.base_url,
                api_key=_resolve_api_key(config),
                temperature=config.temperature,
                timeout=config.timeout,
            ),
        )
    except AgentConfigurationError:
        raise
    except Exception as exc:
        raise AgentConfigurationError("Unable to initialize ChatOpenAI") from exc


def _validate_session_id(session_id: str) -> None:
    if not isinstance(session_id, str):
        raise TypeError("session_id must be a non-empty string")
    if not session_id.strip():
        raise ValueError("session_id must not be empty")


def _validate_user_text(user_text: str) -> None:
    if not isinstance(user_text, str):
        raise TypeError("user_text must be a non-empty string")
    if not user_text.strip():
        raise ValueError("user_text must not be empty")


def _message_for_record(record: MessageRecord, types: tuple[Any, Any, Any, Any]) -> Any:
    human_message, ai_message, system_message, chat_message = types
    constructors = {
        "user": human_message,
        "human": human_message,
        "assistant": ai_message,
        "ai": ai_message,
        "system": system_message,
    }
    constructor = constructors.get(record.role)
    return constructor(content=record.content) if constructor else chat_message(
        role=record.role, content=record.content
    )


def _response_text(response: Any) -> str:
    content = getattr(response, "content", response)
    if isinstance(content, str):
        text = content
    elif isinstance(content, Sequence) and not isinstance(content, (str, bytes)):
        parts: list[str] = []
        for part in content:
            if isinstance(part, str):
                parts.append(part)
            elif isinstance(part, dict) and isinstance(part.get("text"), str):
                parts.append(part["text"])
        text = "".join(parts)
    else:
        text = str(content)
    if not text.strip():
        raise AgentModelError("Chat model returned empty response text")
    return text


class AgentLoop:
    """Run synchronous chat turns and persist them in a SQLite history store.

    Pass either an existing ``history_store`` or a ``database_path``.  The
    latter creates and owns a :class:`SQLiteHistoryStore`, which is closed by
    :meth:`close` or when leaving a context manager.  ``model`` must expose
    LangChain's synchronous ``invoke(messages)`` method; when omitted,
    ``ChatOpenAI`` is constructed lazily from ``config``.
    """

    def __init__(
        self,
        database_path: DatabasePath | SQLiteHistoryStore | None = None,
        model: ChatModel | Any | None = None,
        *,
        history_store: SQLiteHistoryStore | None = None,
        history_index: ExchangeIndexer | Any | None = None,
        config: ChatOpenAIConfig | None = None,
        system_prompt: str = DEFAULT_SYSTEM_PROMPT,
    ) -> None:
        if database_path is not None and history_store is not None:
            raise AgentConfigurationError(
                "pass either database_path or history_store, not both"
            )
        if not isinstance(system_prompt, str) or not system_prompt.strip():
            raise AgentConfigurationError("system_prompt must be a non-empty string")
        if history_store is not None:
            self._history = history_store
            self._owns_history = False
        elif isinstance(database_path, SQLiteHistoryStore):
            self._history = database_path
            self._owns_history = False
        elif database_path is not None:
            self._history = SQLiteHistoryStore(database_path)
            self._owns_history = True
        else:
            raise AgentConfigurationError("database_path or history_store is required")
        if model is not None and not callable(getattr(model, "invoke", None)):
            raise AgentConfigurationError(
                "model must provide a synchronous invoke(messages) method"
            )
        if history_index is not None and not callable(
            getattr(history_index, "index_exchange", None)
        ):
            raise AgentConfigurationError("history_index must provide index_exchange")
        self._config = config or ChatOpenAIConfig()
        self._model = model
        self._history_index = history_index
        self._system_prompt = system_prompt

    @property
    def history_store(self) -> SQLiteHistoryStore:
        """Return the history store used by this loop."""

        return self._history

    def run(self, session_id: str, user_text: str) -> str:
        """Invoke one turn for ``session_id`` and return assistant text.

        Existing messages for the session are sent before the current human
        message. The completed exchange is atomically persisted after a
        successful, non-empty response, then indexed when configured.
        """

        _validate_session_id(session_id)
        _validate_user_text(user_text)
        types = _load_message_types()
        human_message, _, system_message, _ = types
        messages = [system_message(content=self._system_prompt)]
        messages.extend(
            _message_for_record(record, types)
            for record in self._history.get_messages(session_id)
        )
        messages.append(human_message(content=user_text))
        if self._model is None:
            self._model = _create_chat_openai(self._config)
        model = self._model
        try:
            response = model.invoke(messages)
            answer = _response_text(response)
        except AgentLoopError:
            raise
        except Exception as exc:
            raise AgentModelError(f"Chat model invocation failed: {exc}") from exc
        exchange_id = self._history.append_exchange(session_id, user_text, answer)
        if self._history_index is not None:
            self._history_index.index_exchange(exchange_id, session_id)
        return answer

    def invoke(self, session_id: str, user_text: str) -> str:
        """Alias for :meth:`run` for callers using LangChain terminology."""

        return self.run(session_id, user_text)

    def close(self) -> None:
        """Close an internally owned history store."""

        if self._owns_history:
            self._history.close()
            self._owns_history = False

    def __enter__(self) -> AgentLoop:
        return self

    def __exit__(self, _exc_type: object, _exc_value: object, _traceback: object) -> None:
        self.close()


ChatConfig = ChatOpenAIConfig

__all__ = [
    "AgentConfig",
    "AgentConfigurationError",
    "AgentLoop",
    "AgentLoopError",
    "AgentModelError",
    "ChatConfig",
    "ChatModel",
    "ChatOpenAIConfig",
    "DEFAULT_SYSTEM_PROMPT",
    "ExchangeIndexer",
]