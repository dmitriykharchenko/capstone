"""Runtime configuration for the JSON stdin/stdout CLI bridge.

The bridge is invoked by the Rust/Tauri backend (``python -m src.cli``); it
reads one JSON request on stdin and writes one JSON response on stdout.  All
locations and model settings derive from the repository layout and can be
overridden via environment variables. Selected-library construction validates
existing path components; legacy construction performs no I/O.
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CHAT_BASE_URL = "https://api.openai.com/v1"
OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
DEFAULT_OPENROUTER_CHAT_MODEL = "openai/gpt-5.4-mini"
DEFAULT_INDEX_MAX_WORKERS = 4
LIBRARY_ENV_VAR = "CAPSTONE_LIBRARY_DIR"
ARTIFACT_DIRECTORY_NAME = ".research-assistant"


def _is_frozen() -> bool:
    """Return whether this process is running from a PyInstaller bundle."""

    return bool(getattr(sys, "frozen", False))


def runtime_resource_path(relative_path: str) -> Path:
    """Resolve a read-only packaged resource without using the current directory."""

    root = (
        Path(getattr(sys, "_MEIPASS", PROJECT_ROOT)) if _is_frozen() else PROJECT_ROOT
    )
    return root / relative_path


def selected_library_directory() -> Path | None:
    """Return the canonical selected PDF root, or ``None`` in legacy mode."""

    raw = os.getenv(LIBRARY_ENV_VAR)
    if raw is None or not raw.strip():
        return None
    library = Path(raw)
    if not library.is_absolute():
        raise ValueError(f"{LIBRARY_ENV_VAR} must be an absolute canonical directory")
    try:
        resolved = library.resolve(strict=True)
    except OSError as exc:
        raise ValueError(f"{LIBRARY_ENV_VAR} must be an existing directory") from exc
    if resolved != library or not library.is_dir():
        raise ValueError(f"{LIBRARY_ENV_VAR} must be an absolute canonical directory")
    return library


def _artifact_directory(library: Path) -> Path:
    return library / ARTIFACT_DIRECTORY_NAME


def _validate_artifact_paths(library: Path, paths: tuple[Path, ...]) -> None:
    """Reject existing links that could redirect application-owned writes."""

    artifact_directory = _artifact_directory(library)
    for target in paths:
        try:
            relative = target.relative_to(artifact_directory)
        except ValueError as exc:
            raise ValueError(
                "selected-library artifact path escapes its data root"
            ) from exc
        candidate = artifact_directory
        for part in relative.parts:
            if candidate.is_symlink():
                raise ValueError("selected-library artifact paths cannot use symlinks")
            candidate = candidate / part
        if candidate.is_symlink():
            raise ValueError("selected-library artifact paths cannot use symlinks")
        try:
            candidate.resolve(strict=False).relative_to(library)
        except ValueError as exc:
            raise ValueError(
                "selected-library artifact path escapes its data root"
            ) from exc


def default_data_directory() -> Path:
    """Return the user-writable data root for this runtime mode."""

    library = selected_library_directory()
    if library is not None:
        artifact_directory = _artifact_directory(library)
        _validate_artifact_paths(library, (artifact_directory,))
        return artifact_directory
    override = os.getenv("CAPSTONE_DATA_DIR")
    if override and override.strip():
        return Path(override)
    if _is_frozen():
        return Path.home() / "Library" / "Application Support" / "Capstone"
    return PROJECT_ROOT / "data"


def default_evaluation_results_path() -> Path:
    """Keep development artifacts in tests while frozen builds use app data."""

    library = selected_library_directory()
    if library is not None:
        return _artifact_directory(library) / "evaluations" / "eval_results.json"
    if _is_frozen():
        return default_data_directory() / "eval_results.json"
    return PROJECT_ROOT / "tests" / "eval_results.json"


def default_papers_directory() -> Path:
    """Keep source papers in the repository and packaged papers in app data."""

    library = selected_library_directory()
    if library is not None:
        return library
    if _is_frozen():
        return default_data_directory() / "ResearchPapers"
    return PROJECT_ROOT / "ResearchPapers"


DEFAULT_DATA_DIRECTORY = default_data_directory()
DEFAULT_DATABASE_PATH = DEFAULT_DATA_DIRECTORY / "chat_history.sqlite3"
DEFAULT_PAPERS_DIRECTORY = default_papers_directory()
DEFAULT_CHROMA_DIRECTORY = DEFAULT_DATA_DIRECTORY / "chroma"
DEFAULT_LOG_PATH = DEFAULT_DATA_DIRECTORY / "chat_logs.txt"
DEFAULT_RETRIEVAL_LOG_PATH = DEFAULT_DATA_DIRECTORY / "retrieval_logs.txt"


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
    library_directory: Path | None = None

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
        if self.library_directory is not None:
            library = Path(self.library_directory)
            artifact_directory = _artifact_directory(library)
            expected = {
                "database_path": artifact_directory / "chat_history.sqlite3",
                "papers_directory": library,
                "chroma_directory": artifact_directory / "chroma",
                "log_path": artifact_directory / "logs" / "chat_logs.txt",
                "retrieval_log_path": artifact_directory
                / "logs"
                / "retrieval_logs.txt",
            }
            if any(
                Path(getattr(self, name)) != path for name, path in expected.items()
            ):
                raise ValueError(
                    "selected library must own every writable backend path"
                )
            _validate_artifact_paths(
                library,
                (
                    expected["database_path"],
                    expected["chroma_directory"],
                    expected["log_path"],
                    expected["retrieval_log_path"],
                    artifact_directory / "chat_orchestration.sqlite3",
                    artifact_directory / "paper_graph.db",
                    artifact_directory / "pdf-artifacts",
                    artifact_directory / "evaluations" / "eval_results.json",
                ),
            )

    @property
    def has_api_key(self) -> bool:
        """Return ``True`` when a real chat/embedding API key is configured."""

        return bool(self.api_key and self.api_key.strip())

    @property
    def artifact_directory(self) -> Path | None:
        """Return the selected library's private data root, when selected."""

        if self.library_directory is None:
            return None
        return _artifact_directory(Path(self.library_directory))

    @property
    def conversion_directory(self) -> Path | None:
        """Return the desktop conversion directory without changing standalone defaults."""

        root = self.artifact_directory
        return None if root is None else root / "pdf-artifacts"

    @property
    def evaluation_results_path(self) -> Path | None:
        """Return the selected library evaluation template, or the legacy sentinel."""

        root = self.artifact_directory
        return None if root is None else root / "evaluations" / "eval_results.json"

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
        library = selected_library_directory()
        if library is not None:
            data_directory = _artifact_directory(library)
            database_path = data_directory / "chat_history.sqlite3"
            papers_directory = library
            chroma_directory = data_directory / "chroma"
            log_path = data_directory / "logs" / "chat_logs.txt"
            retrieval_log_path = data_directory / "logs" / "retrieval_logs.txt"
        else:
            data_directory = default_data_directory()
            database_path = Path(
                os.getenv(
                    "CAPSTONE_DB_PATH", str(data_directory / "chat_history.sqlite3")
                )
            )
            papers_directory = Path(
                os.getenv("CAPSTONE_PAPERS_DIR", str(default_papers_directory()))
            )
            chroma_directory = Path(
                os.getenv("CAPSTONE_CHROMA_DIR", str(data_directory / "chroma"))
            )
            log_path = Path(
                os.getenv("CAPSTONE_LOG_PATH", str(data_directory / "chat_logs.txt"))
            )
            retrieval_log_path = Path(
                os.getenv(
                    "CAPSTONE_RETRIEVAL_LOG_PATH",
                    str(data_directory / "retrieval_logs.txt"),
                )
            )
        return cls(
            database_path=database_path,
            papers_directory=papers_directory,
            chroma_directory=chroma_directory,
            chat_model=os.getenv("CAPSTONE_CHAT_MODEL", default_chat_model),
            chat_base_url=chat_base_url,
            embedding_model=os.getenv("CAPSTONE_EMBED_MODEL", "text-embedding-3-small"),
            embedding_base_url=embedding_base_url,
            api_key=api_key,
            system_prompt=system_prompt or None,
            log_path=log_path,
            retrieval_log_path=retrieval_log_path,
            chat_max_tokens=_optional_positive_int("CAPSTONE_CHAT_MAX_TOKENS"),
            index_max_workers=(
                _optional_positive_int("CAPSTONE_INDEX_MAX_WORKERS")
                or DEFAULT_INDEX_MAX_WORKERS
            ),
            library_directory=library,
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

    if _is_frozen():
        return None
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
    "ARTIFACT_DIRECTORY_NAME",
    "CLIConfig",
    "DEFAULT_CHROMA_DIRECTORY",
    "DEFAULT_DATA_DIRECTORY",
    "DEFAULT_DATABASE_PATH",
    "DEFAULT_PAPERS_DIRECTORY",
    "PROJECT_ROOT",
    "default_data_directory",
    "default_evaluation_results_path",
    "default_papers_directory",
    "load_dotenv_if_present",
    "runtime_resource_path",
    "selected_library_directory",
]
