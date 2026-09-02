import io
import json
from pathlib import Path

from src.cli import __main__ as cli_main
from src.cli.commands import handle_index_papers_stream
from src.cli.config import CLIConfig
from src.cli.models import build_paper_indexer
from src.embeddings import EmbeddingConfig
from src.paper_indexer import PaperIndexOutcome, PaperIndexProgress


def config(tmp_path: Path) -> CLIConfig:
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
    )


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