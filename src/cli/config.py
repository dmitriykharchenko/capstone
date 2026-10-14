"""Runtime configuration for the JSON stdin/stdout CLI bridge.

The bridge is invoked by the Rust/Tauri backend (``python -m src.cli``); it
reads one JSON request on stdin and writes one JSON response on stdout.  All
locations and model settings derive from the repository layout and can be
overridden via environment variables.  Constructing a config performs no I/O.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_DATABASE_PATH = PROJECT_ROOT / "data" / "chat_history.sqlite3"
DEFAULT_PAPERS_DIRECTORY = PROJECT_ROOT / "ResearchPapers"
DEFAULT_CHROMA_DIRECTORY = PROJECT_ROOT / "data" / "chroma"
DEFAULT_LOG_PATH = PROJECT_ROOT / "data" / "chat_logs.txt"
DEFAULT_RETRIEVAL_LOG_PATH = PROJECT_ROOT / "data" / "retrieval_logs.txt"
DEFAULT_CHAT_BASE_URL = "https://api.openai.com/v1"
OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
DEFAULT_OPENROUTER_CHAT_MODEL = "openai/gpt-5.4-mini"
DEFAULT_INDEX_MAX_WORKERS = 4


@dataclass(frozen=True)
class CLIConfig:
    """Settings that determine how the bridge locates data and answers chats."""

    database_path: Path
    papers_directory: Path
    chroma_directory: Path
    chat_model: str
    chat_base_url: str
    embedding_model: str
    embedding_base_url: str
    api_key: str | None
    system_prompt: str | None
    log_path: Path = DEFAULT_LOG_PATH
    retrieval_log_path: Path = DEFAULT_RETRIEVAL_LOG_PATH
    chat_max_tokens: int | None = None
    index_max_workers: int = DEFAULT_INDEX_MAX_WORKERS

    def __post_init__(self) -> None:
        if self.chat_max_tokens is not None and (
            isinstance(self.chat_max_tokens, bool)
            or not isinstance(self.chat_max_tokens, int)
            or self.chat_max_tokens < 1
        ):
            raise ValueError("chat_max_tokens must be a positive integer")
        if (
            isinstance(self.index_max_workers, bool)
            or not isinstance(self.index_max_workers, int)
            or self.index_max_workers < 1
        ):
            raise ValueError("index_max_workers must be a positive integer")

    @property
    def has_api_key(self) -> bool:
        """Return ``True`` when a real chat/embedding API key is configured."""

        return bool(self.api_key and self.api_key.strip())

    @classmethod
    def from_env(cls) -> "CLIConfig":
        """Build a configuration from environment variables with defaults.

        When only ``OPENROUTER_API_KEY`` is set, the chat/embedding endpoints and
        the default chat model switch to OpenRouter so the key targets the right
        service; explicit ``CAPSTONE_*`` overrides always win.
        """

        openai_key = os.getenv("OPENAI_API_KEY")
        openrouter_key = os.getenv("OPENROUTER_API_KEY")
        api_key = openai_key or openrouter_key
        use_openrouter = not openai_key and bool(openrouter_key)
        default_chat_base_url = (
            OPENROUTER_BASE_URL if use_openrouter else DEFAULT_CHAT_BASE_URL
        )
        default_chat_model = (
            DEFAULT_OPENROUTER_CHAT_MODEL if use_openrouter else "gpt-4o-mini"
        )
        chat_base_url = os.getenv("CAPSTONE_CHAT_BASE_URL", default_chat_base_url)
        embedding_base_url = os.getenv("CAPSTONE_EMBED_BASE_URL", chat_base_url)
        system_prompt = os.getenv("CAPSTONE_SYSTEM_PROMPT")
        return cls(
            database_path=Path(
                os.getenv("CAPSTONE_DB_PATH", str(DEFAULT_DATABASE_PATH))
            ),
            papers_directory=Path(
                os.getenv("CAPSTONE_PAPERS_DIR", str(DEFAULT_PAPERS_DIRECTORY))
            ),
            chroma_directory=Path(
                os.getenv("CAPSTONE_CHROMA_DIR", str(DEFAULT_CHROMA_DIRECTORY))
            ),
            chat_model=os.getenv("CAPSTONE_CHAT_MODEL", default_chat_model),
            chat_base_url=chat_base_url,
            embedding_model=os.getenv("CAPSTONE_EMBED_MODEL", "text-embedding-3-small"),
            embedding_base_url=embedding_base_url,
            api_key=api_key,
            system_prompt=system_prompt or None,
            log_path=Path(os.getenv("CAPSTONE_LOG_PATH", str(DEFAULT_LOG_PATH))),
            retrieval_log_path=Path(
                os.getenv(
                    "CAPSTONE_RETRIEVAL_LOG_PATH", str(DEFAULT_RETRIEVAL_LOG_PATH)
                )
            ),
            chat_max_tokens=_optional_positive_int("CAPSTONE_CHAT_MAX_TOKENS"),
            index_max_workers=(
                _optional_positive_int("CAPSTONE_INDEX_MAX_WORKERS")
                or DEFAULT_INDEX_MAX_WORKERS
            ),
        )


def _optional_positive_int(name: str) -> int | None:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return None
    try:
        value = int(raw)
    except ValueError as exc:
        raise ValueError(f"{name} must be a positive integer") from exc
    if value < 1:
        raise ValueError(f"{name} must be a positive integer")
    return value


def _find_dotenv() -> Path | None:
    """Return the nearest ``.env`` at or above ``PROJECT_ROOT``, if any.

    The repository root is checked first, then each parent directory, so a key
    kept in a ``.env`` outside the project (e.g. one level up) is still found.
    """

    for directory in (PROJECT_ROOT, *PROJECT_ROOT.parents):
        candidate = directory / ".env"
        if candidate.is_file():
            return candidate
    return None


def load_dotenv_if_present() -> None:
    """Best-effort load of the nearest ``.env`` at or above the repo root."""

    env_path = _find_dotenv()
    if env_path is None:
        return
    try:
        from dotenv import load_dotenv
    except Exception:
        return
    try:
        load_dotenv(env_path)
    except Exception:
        pass


__all__ = [
    "CLIConfig",
    "DEFAULT_CHROMA_DIRECTORY",
    "DEFAULT_DATABASE_PATH",
    "DEFAULT_PAPERS_DIRECTORY",
    "PROJECT_ROOT",
    "load_dotenv_if_present",
]
