"""Package-aware entry point for the frozen JSON CLI backend."""

import multiprocessing
import sys


def run() -> int:
    """Handle PyInstaller child modes before reading the JSON bridge request."""

    multiprocessing.freeze_support()
    from src.indexing.pdf_to_markdown import _FROZEN_WORKER_ARGUMENT

    if len(sys.argv) == 3 and sys.argv[1] == _FROZEN_WORKER_ARGUMENT:
        from src.indexing.pdf_to_markdown import _extract_worker

        return _extract_worker(sys.argv[2])
    from src.cli.__main__ import _exit_when_orphaned, main

    _exit_when_orphaned()
    return main()


if __name__ == "__main__":
    raise SystemExit(run())