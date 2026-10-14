"""Offline contract tests for the frozen macOS CLI backend."""

from __future__ import annotations

import importlib.util
import os
from pathlib import Path

import pytest

from src.cli import config
from src.cli import __main__ as cli_main


def _builder_module():
    path = Path(__file__).parents[1] / "tools" / "build_macos_backend.py"
    spec = importlib.util.spec_from_file_location("macos_backend_builder", path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_frozen_runtime_uses_external_data_and_packaged_resources(monkeypatch, tmp_path):
    monkeypatch.setattr(config, "_is_frozen", lambda: True)
    monkeypatch.setattr(config.Path, "home", lambda: tmp_path)
    monkeypatch.setattr(config.sys, "_MEIPASS", str(tmp_path / "bundle"), raising=False)

    assert config.default_data_directory() == (
        tmp_path / "Library" / "Application Support" / "Capstone"
    )
    assert config.runtime_resource_path("tests/judge.md") == (
        tmp_path / "bundle" / "tests" / "judge.md"
    )
    assert config.default_papers_directory() == (
        tmp_path / "Library" / "Application Support" / "Capstone" / "ResearchPapers"
    )


def test_source_runtime_keeps_repository_papers_default(monkeypatch):
    monkeypatch.setattr(config, "_is_frozen", lambda: False)

    assert config.default_papers_directory() == config.PROJECT_ROOT / "ResearchPapers"


def test_frozen_runtime_never_discovers_repository_or_ancestor_dotenv(monkeypatch):
    monkeypatch.setattr(config, "_is_frozen", lambda: True)

    assert config._find_dotenv() is None


def test_builder_contract_includes_only_required_resources_and_native_packages(tmp_path):
    builder = _builder_module()
    args = builder.pyinstaller_args(tmp_path)

    assert "--onedir" in args
    assert args[args.index("--name") + 1] == "capstone-cli"
    assert args[args.index("--target-architecture") + 1] == "arm64"
    assert str(tmp_path / builder.BACKEND_DIRECTORY) in args
    assert any("tests/judge.md" in value for value in args)
    assert any("src/orchestration/wire.json" in value for value in args)
    assert args[-1] == str(tmp_path / "tools" / "frozen_cli_entry.py")
    assert "chromadb" in args and "pycozo" in args and "pymupdf" in args
    assert not any(".env" in value or "ResearchPapers" in value for value in args)


def test_frozen_entry_supports_spawn_before_cli_processing():
    entry = (Path(__file__).parents[1] / "tools" / "frozen_cli_entry.py").read_text()

    assert entry.index("multiprocessing.freeze_support()") < entry.index(
        "_exit_when_orphaned()"
    )


def test_parent_watch_exits_when_started_without_a_parent(monkeypatch):
    monkeypatch.setattr(os, "getppid", lambda: 1)
    monkeypatch.setattr(os, "_exit", lambda code: (_ for _ in ()).throw(SystemExit(code)))

    with pytest.raises(SystemExit, match="1"):
        cli_main._exit_when_orphaned()