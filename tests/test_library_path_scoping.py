"""Offline contracts for per-library desktop data and generated assets."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

from src.cli import config as config_module
from src.cli import __main__ as cli_main
from src.cli import models
from src.cli.config import CLIConfig
from src.cli.orchestration import store_path
from src.indexing import pdf_to_markdown as converter
from src.indexing.paper_indexer import PaperIndexer
from src.retrieval.paper_search import DEFAULT_BM25_INDEX_FILENAME
from src.storage.sqlite_history import SQLiteHistoryStore


PATH_ENV = (
    "CAPSTONE_LIBRARY_DIR",
    "CAPSTONE_DATA_DIR",
    "CAPSTONE_DB_PATH",
    "CAPSTONE_PAPERS_DIR",
    "CAPSTONE_CHROMA_DIR",
    "CAPSTONE_LOG_PATH",
    "CAPSTONE_RETRIEVAL_LOG_PATH",
)
CLEAR_ENV = PATH_ENV + (
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
)


def _clean_paths(monkeypatch) -> None:
    for name in CLEAR_ENV:
        monkeypatch.delenv(name, raising=False)


def _select(monkeypatch, library: Path) -> CLIConfig:
    monkeypatch.setenv("CAPSTONE_LIBRARY_DIR", str(library))
    return CLIConfig.from_env()


@pytest.mark.parametrize("frozen", [False, True])
def test_selected_library_is_authoritative_in_source_and_frozen_modes(
    monkeypatch, tmp_path, frozen
):
    _clean_paths(monkeypatch)
    library = tmp_path / "Papers α"
    library.mkdir()
    legacy = tmp_path / "legacy"
    for name in PATH_ENV[1:]:
        monkeypatch.setenv(name, str(legacy / name))
    monkeypatch.setattr(config_module, "_is_frozen", lambda: frozen)

    config = _select(monkeypatch, library)
    root = library / ".research-assistant"

    assert config.library_directory == library
    assert config.papers_directory == library
    assert config.database_path == root / "chat_history.sqlite3"
    assert config.chroma_directory == root / "chroma"
    assert config.log_path == root / "logs" / "chat_logs.txt"
    assert config.retrieval_log_path == root / "logs" / "retrieval_logs.txt"
    assert config.conversion_directory == root / "pdf-artifacts"
    assert config.evaluation_results_path == root / "evaluations" / "eval_results.json"
    assert (
        config_module.runtime_resource_path("tests/judge.md")
        == (
            (Path(getattr(config_module.sys, "_MEIPASS", config_module.PROJECT_ROOT)))
            if frozen
            else config_module.PROJECT_ROOT
        )
        / "tests"
        / "judge.md"
    )


def test_complete_desktop_artifact_map_is_contained(monkeypatch, tmp_path):
    _clean_paths(monkeypatch)
    library = tmp_path / "library"
    library.mkdir()
    config = _select(monkeypatch, library)
    root = library / ".research-assistant"
    paths = (
        config.database_path,
        config.chroma_directory,
        config.chroma_directory / DEFAULT_BM25_INDEX_FILENAME,
        config.chroma_directory.parent / "paper_graph.db",
        store_path(config),
        store_path(config).parent / ".orchestration-locks",
        config.conversion_directory,
        config.log_path,
        config.retrieval_log_path,
        config.evaluation_results_path,
    )

    assert all(path is not None and path.is_relative_to(root) for path in paths)


def test_cli_indexer_receives_selected_conversion_directory(monkeypatch, tmp_path):
    _clean_paths(monkeypatch)
    library = tmp_path / "library"
    library.mkdir()
    config = _select(monkeypatch, library)
    captured = {}

    class Graph:
        def close(self):
            pass

    monkeypatch.setattr(models, "chromadb_available", lambda: True)
    monkeypatch.setattr(models, "CozoGraphStore", lambda path: Graph())
    monkeypatch.setattr(
        models,
        "PaperIndexer",
        lambda *args, **kwargs: captured.update(args=args, kwargs=kwargs) or object(),
    )

    assert models.build_paper_indexer(config) is not None
    assert captured["args"] == (library,)
    assert captured["kwargs"]["persist_directory"] == config.chroma_directory
    assert captured["kwargs"]["artifacts_directory"] == config.conversion_directory


def test_switching_libraries_keeps_history_status_and_legacy_data_isolated(
    monkeypatch, tmp_path
):
    _clean_paths(monkeypatch)
    legacy = tmp_path / "legacy"
    legacy.mkdir()
    sentinel = legacy / "sentinel"
    sentinel.write_text("unchanged", encoding="utf-8")
    monkeypatch.setenv("CAPSTONE_DATA_DIR", str(legacy))
    library_a, library_b = tmp_path / "A", tmp_path / "B"
    library_a.mkdir()
    library_b.mkdir()
    existing_root = library_a / ".research-assistant"
    existing_root.mkdir()
    existing_marker = existing_root / "keep.txt"
    existing_marker.write_text("keep", encoding="utf-8")

    config_a = _select(monkeypatch, library_a)
    with SQLiteHistoryStore(config_a.database_path) as store:
        store.append_exchange("a", "question A", "answer A")
        store.record_paper_status("paper-a", "a.pdf", "indexed", chunk_count=1)

    config_b = _select(monkeypatch, library_b)
    assert not config_b.database_path.exists()
    with SQLiteHistoryStore(config_b.database_path) as store:
        assert store.get_messages("a") == []
        assert store.get_paper_statuses() == []
        store.append_exchange("b", "question B", "answer B")

    config_a_again = _select(monkeypatch, library_a)
    with SQLiteHistoryStore(config_a_again.database_path) as store:
        assert [message.content for message in store.get_messages("a")] == [
            "question A",
            "answer A",
        ]
        assert [status.paper_id for status in store.get_paper_statuses()] == ["paper-a"]
    assert sentinel.read_text(encoding="utf-8") == "unchanged"
    assert existing_marker.read_text(encoding="utf-8") == "keep"
    assert not (legacy / "chat_history.sqlite3").exists()


def test_desktop_conversion_and_evaluation_write_only_under_artifact_root(
    monkeypatch, tmp_path
):
    _clean_paths(monkeypatch)
    library = tmp_path / "library"
    library.mkdir()
    pdf = library / "paper.pdf"
    pdf.write_bytes(b"%PDF isolated fixture")
    config = _select(monkeypatch, library)

    def fake_convert(source, staging):
        digest = hashlib.sha256(source.read_bytes()).hexdigest()
        (staging / "document.md").write_text(
            f"<!-- pdf-artifacts: v1 pages=1 sha256={digest} -->\n"
            "\n<!-- pdf-page: 1 -->\nOffline evidence\n",
            encoding="utf-8",
        )

    class Collection:
        def delete(self, **_kwargs):
            pass

        def upsert(self, **_kwargs):
            pass

    monkeypatch.setattr(converter, "_extract_pdf", fake_convert)
    indexer = PaperIndexer(
        library,
        collection=Collection(),
        embedder=lambda _text: [1.0],
        artifacts_directory=config.conversion_directory,
    )
    assert indexer.index_paper(pdf).status == "indexed"
    assert (config.conversion_directory / "paper.md").is_file()
    assert not (library / "paper.md").exists()

    case = {"question": "q", "grading_notes": "g", "sources": []}
    result = {
        "case_index": 1,
        "input": case,
        "retrieval_strategy": "hybrid",
        "retrieved_chunks": [],
        "generated_answer": None,
        "judge_scores": None,
        "error": {"stage": "judge", "message": "offline fixture"},
    }
    events = iter(
        (
            {"event": "case_start", "input": case},
            {"event": "case_failure", "result": result},
            {
                "event": "complete",
                "retrieval_type": "hybrid",
                "aggregate_scores": {},
                "succeeded": 0,
                "failed": 1,
            },
        )
    )
    assert (
        list(
            cli_main._persisted_evaluation_stream(
                events, results_path=config.evaluation_results_path
            )
        )[-1]["event"]
        == "complete"
    )
    results = list(config.evaluation_results_path.parent.glob("eval_results_*.json"))
    assert len(results) == 1
    artifact = json.loads(results[0].read_text(encoding="utf-8"))
    assert artifact["results"][0]["case_index"] == 1
    assert artifact["results"][0]["error"]["stage"] == "judge"
    assert artifact["summary"] == {"total": 1, "succeeded": 0, "failed": 1}
    assert not list((tmp_path / "legacy").glob("eval_results_*.json"))


def test_no_selection_keeps_legacy_overrides(monkeypatch, tmp_path):
    _clean_paths(monkeypatch)
    overrides = {
        "CAPSTONE_DB_PATH": tmp_path / "history.sqlite3",
        "CAPSTONE_PAPERS_DIR": tmp_path / "papers",
        "CAPSTONE_CHROMA_DIR": tmp_path / "vectors",
        "CAPSTONE_LOG_PATH": tmp_path / "chat.log",
        "CAPSTONE_RETRIEVAL_LOG_PATH": tmp_path / "retrieval.log",
    }
    for name, path in overrides.items():
        monkeypatch.setenv(name, str(path))

    config = CLIConfig.from_env()

    assert config.library_directory is None
    assert config.database_path == overrides["CAPSTONE_DB_PATH"]
    assert config.papers_directory == overrides["CAPSTONE_PAPERS_DIR"]
    assert config.chroma_directory == overrides["CAPSTONE_CHROMA_DIR"]
    assert config.log_path == overrides["CAPSTONE_LOG_PATH"]
    assert config.retrieval_log_path == overrides["CAPSTONE_RETRIEVAL_LOG_PATH"]
    assert config.conversion_directory is None
    assert config.evaluation_results_path is None


@pytest.mark.parametrize("link_name", [".research-assistant", "chroma", "logs"])
def test_selected_library_rejects_artifact_symlink_escapes(
    monkeypatch, tmp_path, link_name
):
    _clean_paths(monkeypatch)
    library = tmp_path / "library"
    outside = tmp_path / "outside"
    library.mkdir()
    outside.mkdir()
    root = library / ".research-assistant"
    if link_name == ".research-assistant":
        root.symlink_to(outside, target_is_directory=True)
    else:
        root.mkdir()
        (root / link_name).symlink_to(outside, target_is_directory=True)
    monkeypatch.setenv("CAPSTONE_LIBRARY_DIR", str(library))

    with pytest.raises(ValueError, match="symlink|escapes"):
        CLIConfig.from_env()


@pytest.mark.parametrize("value", ["relative/library", "/definitely/missing/library"])
def test_invalid_selected_library_never_falls_back(monkeypatch, value):
    _clean_paths(monkeypatch)
    monkeypatch.setenv("CAPSTONE_LIBRARY_DIR", value)

    with pytest.raises(ValueError, match="CAPSTONE_LIBRARY_DIR"):
        CLIConfig.from_env()
