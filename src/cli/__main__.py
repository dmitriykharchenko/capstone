"""Run the JSON stdin/stdout CLI bridge.

Usage::

    echo '{"command": "health"}' | python -m src.cli

Reads exactly one JSON request object from stdin and writes exactly one JSON
response object (terminated by a newline) to stdout.  Errors are reported as
``{"ok": false, "error": ...}`` with a non-zero exit code, so the Rust backend
can distinguish success from failure without parsing stderr.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

from .config import CLIConfig, load_dotenv_if_present


def handle_chat_stream(config: CLIConfig, request: dict[str, Any]) -> Any:
    from .commands import handle_chat_stream as handler

    return handler(config, request)


def handle_index_papers_stream(config: CLIConfig) -> Any:
    from .commands import handle_index_papers_stream as handler

    return handler(config)


def handle_evaluation_stream(config: CLIConfig, request: dict[str, Any]) -> Any:
    from .commands import handle_evaluation_stream as handler

    return handler(config, request)


def main() -> int:
    """Read one request, dispatch it, and emit one JSON response.

    A ``chat`` request with ``"stream": true`` is answered incrementally: one
    JSON line per token delta followed by a terminating ``{"done": true}``
    object, so the Rust bridge can forward tokens to the UI as they arrive.
    """

    load_dotenv_if_present()
    raw = sys.stdin.read()
    try:
        request: dict[str, Any] = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError as exc:
        _emit({"ok": False, "error": f"invalid JSON request: {exc}"})
        return 1
    if not isinstance(request, dict):
        _emit({"ok": False, "error": "request must be a JSON object"})
        return 1
    config = CLIConfig.from_env()
    from .orchestration import STREAM_COMMANDS, READ_COMMANDS, control, emit_stream

    if request.get("command") in STREAM_COMMANDS:
        return emit_stream(config, request, _emit)
    if request.get("command") in READ_COMMANDS:
        try:
            _emit(control(config, request))
            return 0
        except Exception:
            from ..orchestration.wire import ERROR

            _emit({"ok": False, "error": ERROR})
            return 1
    if request.get("command") == "chat" and request.get("stream"):
        return _run_stream(handle_chat_stream(config, request), secret=config.api_key)
    if request.get("command") == "index_papers" and request.get("stream"):
        return _run_stream(handle_index_papers_stream(config), secret=config.api_key)
    if request.get("command") == "evaluate" and request.get("stream"):
        return _run_stream(
            _persisted_evaluation_stream(
                handle_evaluation_stream(config, request),
                results_path=config.evaluation_results_path,
            ),
            secret=config.api_key,
        )
    from .commands import dispatch

    try:
        result = dispatch(request.get("command"), config, request)
    except Exception as exc:  # noqa: BLE001 - surfaced as JSON to the caller
        error = _error_payload(exc, config.api_key)
        if getattr(exc, "retryable", False) is True:
            error["retryable"] = True
        _emit(error)
        return 1
    if "ok" not in result:
        result = {"ok": True, **result}
    _emit(result)
    return 0


def _error_payload(exc: Exception, secret: str | None) -> dict[str, Any]:
    """Describe ``exc`` for the caller with the configured API key removed."""

    message = str(exc) or type(exc).__name__
    if secret and secret.strip():
        message = message.replace(secret.strip(), "[redacted]")
    return {"ok": False, "error": message, "error_type": type(exc).__name__}


def _run_stream(events: Any, *, secret: str | None = None) -> int:
    """Emit an iterator of newline-delimited JSON events."""

    try:
        for event in events:
            _emit(event)
    except Exception as exc:  # noqa: BLE001 - surfaced as JSON to the caller
        _emit(_error_payload(exc, secret))
        return 1
    return 0


def _persisted_evaluation_stream(
    events: Any, *, results_path: str | Path | None = None
) -> Any:
    """Persist a completed evaluation before forwarding its terminal event."""

    from ..evaluation.results import (
        DEFAULT_EVALUATION_RESULTS_PATH,
        persist_evaluation_results,
    )

    input_cases = []
    results = {}
    try:
        for event in events:
            event_type = event.get("event")
            if event_type == "case_start":
                input_cases.append(event["input"])
            elif event_type in {"case_success", "case_failure", "case_retry_result"}:
                result = event["result"]
                results[result["case_index"]] = result
            if event_type == "complete":
                persist_evaluation_results(
                    DEFAULT_EVALUATION_RESULTS_PATH
                    if results_path is None
                    else results_path,
                    input_cases=input_cases,
                    retrieval_strategy=event["retrieval_type"],
                    results=[
                        results[index] for index in range(1, len(input_cases) + 1)
                    ],
                    aggregate_scores=event["aggregate_scores"],
                    succeeded=event["succeeded"],
                    failed=event["failed"],
                )
            yield event
    finally:
        close = getattr(events, "close", None)
        if close is not None:
            close()


def _emit(payload: dict[str, Any]) -> None:
    json.dump(payload, sys.stdout)
    sys.stdout.write("\n")
    sys.stdout.flush()


def _exit_when_orphaned(interval: float = 1.0) -> None:
    """Exit once the launching process is gone so no orphan keeps store locks."""

    import os
    import threading
    import time

    parent = os.getppid()
    if parent == 1:
        os._exit(1)

    def watch() -> None:
        while os.getppid() == parent:
            time.sleep(interval)
        os._exit(1)

    threading.Thread(target=watch, name="capstone-parent-watch", daemon=True).start()


if __name__ == "__main__":
    _exit_when_orphaned()
    raise SystemExit(main())
