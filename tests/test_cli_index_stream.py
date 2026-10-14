import io
import json
from pathlib import Path

import pytest

from src.cli import __main__ as cli_main
from src.cli.commands import (
    handle_chat,
    handle_chat_stream,
    handle_paper_statuses,
    handle_sessions,
)
from src.cli.config import CLIConfig
from src.cli.models import build_agent_loop
from src.chat.agent_loop import AgentIncompleteResponseError
from src.cli.stream_workflows import index_events
from src.indexing.paper_indexer import PaperIndexOutcome, PaperIndexProgress
from src.storage.sqlite_history import SQLiteHistoryStore, SourceRecord


def config(tmp_path: Path, chat_max_tokens: int | None = None) -> CLIConfig:
    return CLIConfig(
        database_path=tmp_path / "history.sqlite3",
        papers_directory=tmp_path / "papers",
        chroma_directory=tmp_path / "chroma",
        chat_model="chat-model",
        chat_base_url="https://example.test/v1",
        embedding_model="embed-model",
        embedding_base_url="https://embed.example.test/v1",
        api_key="test-key",
        system_prompt=None,
        log_path=tmp_path / "chat_logs.txt",
        retrieval_log_path=tmp_path / "retrieval_logs.txt",
        chat_max_tokens=chat_max_tokens,
    )


def test_chat_output_budget_environment_default_override_and_validation(monkeypatch):
    monkeypatch.delenv("CAPSTONE_CHAT_MAX_TOKENS", raising=False)
    assert CLIConfig.from_env().chat_max_tokens is None

    monkeypatch.setenv("CAPSTONE_CHAT_MAX_TOKENS", " 2048 ")
    assert CLIConfig.from_env().chat_max_tokens == 2048

    for invalid in ("0", "-1", "1.5", "many"):
        monkeypatch.setenv("CAPSTONE_CHAT_MAX_TOKENS", invalid)
        with pytest.raises(ValueError, match="CAPSTONE_CHAT_MAX_TOKENS"):
            CLIConfig.from_env()


def test_agent_builder_passes_configured_chat_output_budget(monkeypatch, tmp_path):
    captured = {}

    def fake_agent_loop(**options):
        captured.update(options)
        return object()

    monkeypatch.setattr("src.cli.models.AgentLoop", fake_agent_loop)

    build_agent_loop(config(tmp_path, 1536), object(), None, object())

    assert captured["config"].max_tokens == 1536


def test_chat_stream_entrypoint_emits_incomplete_response_error(monkeypatch, tmp_path):
    settings = config(tmp_path)

    def events(_config, _request):
        yield {"delta": "partial"}
        raise AgentIncompleteResponseError("response incomplete; shorten and retry")

    stdout = io.StringIO()
    monkeypatch.setattr(cli_main.CLIConfig, "from_env", lambda: settings)
    monkeypatch.setattr(cli_main, "handle_chat_stream", events)
    monkeypatch.setattr(
        cli_main.sys,
        "stdin",
        io.StringIO('{"command": "chat", "stream": true}'),
    )
    monkeypatch.setattr(cli_main.sys, "stdout", stdout)

    assert cli_main.main() == 1
    assert [json.loads(line) for line in stdout.getvalue().splitlines()] == [
        {"delta": "partial"},
        {
            "ok": False,
            "error": "response incomplete; shorten and retry",
            "error_type": "AgentIncompleteResponseError",
        },
    ]


def test_sync_and_stream_chat_return_the_same_source_schema(monkeypatch, tmp_path):
    source = SourceRecord(1, "paper", 2, "paper.pdf", "Paper", 3, 0.75, "evidence")

    class FakeStore:
        def close(self):
            pass

    class FakeLoop:
        last_sources = (source,)
        last_context = "untrusted context"

        def run(self, _session_id, _message):
            return "answer"

        def stream(self, _session_id, _message):
            yield "an"
            yield "swer"

    monkeypatch.setattr("src.cli.commands.build_history_store", lambda cfg: FakeStore())
    monkeypatch.setattr("src.cli.commands.build_index", lambda cfg, store: None)
    monkeypatch.setattr(
        "src.cli.commands.build_paper_search_index", lambda cfg: object()
    )
    monkeypatch.setattr(
        "src.cli.commands.build_agent_loop",
        lambda config, store, index, retriever: FakeLoop(),
    )
    request = {"session_id": "session", "message": "question"}

    synchronous = handle_chat(config(tmp_path), request)
    streamed = list(handle_chat_stream(config(tmp_path), request))

    assert streamed[:2] == [{"delta": "an"}, {"delta": "swer"}]
    assert synchronous["sources"] == streamed[-1]["sources"]
    assert synchronous["sources"] == [
        {
            "rank": 1,
            "paper_id": "paper",
            "chunk_index": 2,
            "filename": "paper.pdf",
            "title": "Paper",
            "page": 3,
            "score": 0.75,
            "snippet": "evidence",
        }
    ]


def test_sessions_handler_orders_sessions_by_latest_message_activity(tmp_path):
    settings = config(tmp_path)
    with SQLiteHistoryStore(settings.database_path) as store:
        store.append_message("older", "user", "first session")
        store.append_message("newer", "user", "second session")
        store.append_message("older", "assistant", "most recent activity")

    result = handle_sessions(settings)

    assert [session["session_id"] for session in result["sessions"]] == [
        "older",
        "newer",
    ]


def test_index_warning_streams_and_reloads_as_vector_success(tmp_path):
    settings = config(tmp_path)
    warning = "Fact enrichment unavailable: invalid model output after retry."
    outcome = PaperIndexOutcome("paper", "paper.pdf", "indexed", 3, warning)
    updates = iter(
        (
            PaperIndexProgress(0, 1),
            PaperIndexProgress(1, 1, outcome=outcome),
        )
    )
    with SQLiteHistoryStore(settings.database_path) as store:
        events = list(
            index_events(
                updates,
                counts={
                    status: 0 for status in ("indexed", "empty", "failed", "skipped")
                },
                persist=lambda item: store.record_paper_status(
                    item.paper_id,
                    item.filename,
                    item.status,
                    chunk_count=item.chunk_count,
                    error=item.error,
                ),
            )
        )

    progress = events[1]
    persisted = handle_paper_statuses(settings)["statuses"][0]
    assert progress["status"] == "indexed"
    assert progress["chunk_count"] == 3
    assert progress["error"] == warning
    assert persisted["status"] == "indexed"
    assert persisted["chunk_count"] == 3
    assert persisted["error"] == warning
