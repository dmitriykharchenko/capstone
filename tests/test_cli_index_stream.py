import io
import json
from pathlib import Path

import pytest

from src.cli import __main__ as cli_main
from src.cli.commands import (
    handle_chat,
    handle_chat_stream,
    handle_index_papers_stream,
    handle_messages,
    handle_search_papers,
)
from src.cli.config import CLIConfig
from src.cli.models import build_agent_loop, build_paper_indexer
from src.agent_loop import AgentIncompleteResponseError
from src.embeddings import EmbeddingConfig
from src.paper_indexer import PaperIndexOutcome, PaperIndexProgress
from src.paper_search import PaperMatch
from src.sqlite_history import SQLiteHistoryStore, SourceRecord


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


def test_index_stream_emits_start_progress_and_terminal_summary(monkeypatch, tmp_path):
    class FakeIndexer:
        def iter_directory(self):
            yield PaperIndexProgress(0, 3)
            yield PaperIndexProgress(1, 3, PaperIndexOutcome("a", "a.pdf", "indexed", 2))
            yield PaperIndexProgress(2, 3, PaperIndexOutcome("b", "b.pdf", "empty"))
            yield PaperIndexProgress(
                3,
                3,
                PaperIndexOutcome("c", "c.pdf", "failed", error="RuntimeError: broken"),
            )

    monkeypatch.setattr("src.cli.commands.build_paper_indexer", lambda cfg: FakeIndexer())

    events = list(handle_index_papers_stream(config(tmp_path)))

    assert events[0] == {"event": "start", "completed": 0, "total": 3}
    assert [event["completed"] for event in events[1:4]] == [1, 2, 3]
    assert events[1]["status"] == "indexed" and events[1]["chunk_count"] == 2
    assert events[2]["status"] == "empty"
    assert events[3]["status"] == "failed" and events[3]["error"] == "RuntimeError: broken"
    assert events[4] == {
        "ok": True,
        "event": "done",
        "completed": 3,
        "total": 3,
        "indexed": 1,
        "empty": 1,
        "failed": 1,
    }


def test_index_stream_surfaces_indexer_construction_failure(monkeypatch, tmp_path):
    monkeypatch.setattr("src.cli.commands.build_paper_indexer", lambda cfg: None)

    events = handle_index_papers_stream(config(tmp_path))

    try:
        next(events)
    except RuntimeError as exc:
        assert str(exc) == "paper indexer unavailable"
    else:
        raise AssertionError("construction failure must be surfaced")


def test_builder_uses_configured_paths_and_openai_embedding_config(monkeypatch, tmp_path):
    captured = {}

    def fake_indexer(papers_directory, **options):
        captured.update(papers_directory=papers_directory, **options)
        return object()

    monkeypatch.setattr("src.cli.models.chromadb_available", lambda: True)
    monkeypatch.setattr("src.cli.models.PaperIndexer", fake_indexer)
    settings = config(tmp_path)

    assert build_paper_indexer(settings) is not None
    assert captured["papers_directory"] == settings.papers_directory
    assert captured["persist_directory"] == settings.chroma_directory
    assert captured["embedding_config"] == EmbeddingConfig(
        model=settings.embedding_model,
        base_url=settings.embedding_base_url,
        api_key=settings.api_key,
    )


def test_streamed_cli_command_emits_ndjson_and_process_errors(monkeypatch, tmp_path):
    settings = config(tmp_path)

    def events(_config):
        yield {"event": "start", "completed": 0, "total": 1}
        raise OSError("process failed")

    stdin = io.StringIO('{"command": "index_papers", "stream": true}')
    stdout = io.StringIO()
    monkeypatch.setattr(cli_main.CLIConfig, "from_env", lambda: settings)
    monkeypatch.setattr(cli_main, "handle_index_papers_stream", events)
    monkeypatch.setattr(cli_main.sys, "stdin", stdin)
    monkeypatch.setattr(cli_main.sys, "stdout", stdout)

    assert cli_main.main() == 1
    output = [json.loads(line) for line in stdout.getvalue().splitlines()]
    assert output == [
        {"event": "start", "completed": 0, "total": 1},
        {"ok": False, "error": "process failed", "error_type": "OSError"},
    ]


def test_chat_stream_entrypoint_semantics_are_unchanged(monkeypatch, tmp_path):
    settings = config(tmp_path)
    expected = [{"delta": "hello"}, {"ok": True, "done": True, "reply": "hello"}]
    stdout = io.StringIO()
    monkeypatch.setattr(cli_main.CLIConfig, "from_env", lambda: settings)
    monkeypatch.setattr(cli_main, "handle_chat_stream", lambda cfg, request: iter(expected))
    monkeypatch.setattr(
        cli_main.sys,
        "stdin",
        io.StringIO('{"command": "chat", "stream": true}'),
    )
    monkeypatch.setattr(cli_main.sys, "stdout", stdout)

    assert cli_main.main() == 0
    assert [json.loads(line) for line in stdout.getvalue().splitlines()] == expected


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


def test_handle_chat_logs_model_response_and_retrieved_documents(monkeypatch, tmp_path):
    source = SourceRecord(1, "paper", 2, "paper.pdf", "Paper Title", 3, 0.75, "evidence")

    class FakeStore:
        def close(self):
            pass

    class FakeLoop:
        last_sources = (source,)
        last_context = (
            '<<<BEGIN_UNTRUSTED_PAPER_CONTEXT>>>\n[{"rank": 1, '
            '"text": "full chunk evidence text"}]\n<<<END_UNTRUSTED_PAPER_CONTEXT>>>'
        )

        def run(self, _session_id, _message):
            return "the answer"

    monkeypatch.setattr("src.cli.commands.build_history_store", lambda cfg: FakeStore())
    monkeypatch.setattr("src.cli.commands.build_index", lambda cfg, store: None)
    monkeypatch.setattr(
        "src.cli.commands.build_paper_search_index", lambda cfg: object()
    )
    monkeypatch.setattr(
        "src.cli.commands.build_agent_loop",
        lambda config, store, index, retriever: FakeLoop(),
    )

    settings = config(tmp_path)
    handle_chat(settings, {"session_id": "session", "message": "the question"})

    logged = settings.log_path.read_text(encoding="utf-8")
    assert "model: chat-model" in logged
    assert "chat_mode: real" in logged
    assert "response: the answer" in logged
    assert "retrieved_documents: 1" in logged
    assert "Paper Title" in logged
    assert "paper_id=paper" in logged
    assert "score=0.75" in logged

    retrieval_logged = settings.retrieval_log_path.read_text(encoding="utf-8")
    assert "retrieval_type: chat_context" in retrieval_logged
    assert "query: the question" in retrieval_logged
    assert "retrieved_documents: 1" in retrieval_logged
    assert "Paper Title" in retrieval_logged
    assert "paper_id=paper" in retrieval_logged
    assert "context_provided_to_model:" in retrieval_logged
    assert "full chunk evidence text" in retrieval_logged


def test_handle_search_papers_logs_retrieval(monkeypatch, tmp_path):
    match = PaperMatch(
        paper_id="attn",
        title="Attention Is All You Need",
        filename="attention.pdf",
        page=3,
        snippet="evidence",
        score=0.81,
    )

    class FakeIndex:
        def count(self):
            return 5

        def search(self, _query, n_results=5):
            return [match]

    monkeypatch.setattr("src.cli.commands.chromadb_available", lambda: True)
    monkeypatch.setattr(
        "src.cli.commands.build_paper_search_index", lambda cfg: FakeIndex()
    )

    settings = config(tmp_path)
    result = handle_search_papers(settings, {"query": "transformers", "limit": 5})

    assert result["matches"][0]["paper_id"] == "attn"
    retrieval_logged = settings.retrieval_log_path.read_text(encoding="utf-8")
    assert "retrieval_type: paper_search" in retrieval_logged
    assert "query: transformers" in retrieval_logged
    assert "Attention Is All You Need" in retrieval_logged
    assert "paper_id=attn" in retrieval_logged


def test_messages_handler_restores_sources_on_the_assistant(tmp_path):
    settings = config(tmp_path)
    source = SourceRecord(1, "paper", 0, "paper.pdf", "Paper", 1, 0.5, "evidence")
    with SQLiteHistoryStore(settings.database_path) as store:
        store.append_exchange("session", "question", "answer", sources=(source,))

    result = handle_messages(settings, {"session_id": "session"})

    assert result["messages"][0] == {"role": "user", "content": "question"}
    assert result["messages"][1]["sources"][0]["paper_id"] == "paper"
