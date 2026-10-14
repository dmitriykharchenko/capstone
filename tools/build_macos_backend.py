"""Build the self-contained macOS arm64 JSON CLI backend with PyInstaller."""

from __future__ import annotations

import os
import platform
from pathlib import Path

BACKEND_NAME = "capstone-cli"
BACKEND_DIRECTORY = Path("app/src-tauri/resources/backend/macos-arm64")
RUNTIME_RESOURCES = (
    (Path("tests/judge.md"), Path("tests")),
    (Path("src/orchestration/wire.json"), Path("src/orchestration")),
)
COLLECT_ALL = (
    "src",
    "chromadb",
    "fitz",
    "langchain_core",
    "langchain_openai",
    "langgraph",
    "openai",
    "pycozo",
    "pymupdf",
    "pymupdf4llm",
    "pypdf",
)


def repository_root() -> Path:
    return Path(__file__).resolve().parents[1]


def output_directory(root: Path) -> Path:
    return root / BACKEND_DIRECTORY


def pyinstaller_args(root: Path) -> list[str]:
    """Return deterministic arguments for the bundled CLI and runtime resources."""

    output = output_directory(root)
    args = [
        "--noconfirm",
        "--clean",
        "--onedir",
        "--name",
        BACKEND_NAME,
        "--target-architecture",
        "arm64",
        "--distpath",
        str(output),
        "--workpath",
        str(root / ".build" / "pyinstaller"),
        "--specpath",
        str(root / ".build" / "pyinstaller-spec"),
        "--paths",
        str(root),
    ]
    for source, destination in RUNTIME_RESOURCES:
        args.extend(("--add-data", f"{root / source}{os.pathsep}{destination}"))
    for package in COLLECT_ALL:
        args.extend(("--collect-all", package))
    args.append(str(root / "tools" / "frozen_cli_entry.py"))
    return args


def main() -> None:
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise SystemExit("macOS arm64 is required to build this backend")
    try:
        import PyInstaller.__main__
    except ImportError as error:
        raise SystemExit(
            "PyInstaller is required; install it with conda in the ragcourse environment"
        ) from error
    PyInstaller.__main__.run(pyinstaller_args(repository_root()))


if __name__ == "__main__":
    main()