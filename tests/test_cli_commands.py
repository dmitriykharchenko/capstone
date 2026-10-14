"""Offline contracts and exactly-once effects for unary CLI commands."""

import io
import json
import sqlite3
import sys
from pathlib import Path
from unittest.mock import Mock

import pytest

from src.cli import __main__ as cli_main
from src.cli import commands
from src.cli.config import CLIConfig
from src.indexing.paper_indexer import PaperIndexOutcome, PaperIndexReport
from src.retrieval.paper_search import PaperMatch
from src.storage.sqlite_history import SQLiteHistoryStore

COMMANDS = (
    "health",
    "sessions",
    "messages",
    "search",
    "search_papers",
    "index_papers",
    "paper_statuses",
    "bm25_status",
    "reset_all_indices",
)


@pytest.fixture
def config(tmp_path):
    return CLIConfig(
        database_path=tmp_path / "history.db",
        chroma_directory=tmp_path / "chroma",
        papers_directory=tmp_path / "papers",
        chat_model="unused",
        chat_base_url="http://unused.invalid",
        embedding_model="unused",
        embedding_base_url="http://unused.invalid",
        api_key=None,
        system_prompt=None,
        log_path=tmp_path / "chat.log",
        retrieval_log_path=tmp_path / "retrieval.log",
    )


def main_response(monkeypatch, config, request):
    output = io.StringIO()
    monkeypatch.setattr(cli_main, "load_dotenv_if_present", lambda: None)
    monkeypatch.setattr(CLIConfig, "from_env", lambda: config)
    monkeypatch.setattr(sys, "stdin", io.StringIO(json.dumps(request)))
    monkeypatch.setattr(sys, "stdout", output)
    return cli_main.main(), output.getvalue()


def seed_reset_targets(config):
    with SQLiteHistoryStore(config.database_path) as store:
        store.record_paper_status("p1", "one.pdf", "indexed", chunk_count=2)
    with sqlite3.connect(config.database_path) as connection:
        connection.execute(
            "UPDATE paper_index_status SET updated_at = '2026-01-01T00:00:00Z'"
        )
    commands.BM25Index.build([{"paper_id": "p1", "text": "graph research"}]).save(
        config.chroma_directory / commands.DEFAULT_BM25_INDEX_FILENAME
    )


@pytest.fixture
def services(monkeypatch, config):
    """Keep real disposable SQLite/BM25; replace provider-facing services only."""
    with SQLiteHistoryStore(config.database_path) as store:
        exchange_id = store.append_exchange("session", "Question café?", "Answer α")
        exchange = store.get_exchange(exchange_id, "session")
    seed_reset_targets(config)
    history = Mock()
    history.search.return_value = [exchange]
    papers = Mock()
    papers.count.return_value = 2
    papers.search.return_value = [PaperMatch("p1", "Café", "one.pdf", 2, "α", 0.5)]
    indexer = Mock()
    indexer.index_directory.return_value = PaperIndexReport(
        config.papers_directory,
        (
            PaperIndexOutcome("p1", "one.pdf", "skipped", 2),
            PaperIndexOutcome("p2", "two.pdf", "indexed", 3),
            PaperIndexOutcome("p3", "three.pdf", "failed", error="invalid PDF"),
        ),
    )
    client = Mock()
    mocks = {
        "history": history,
        "papers": papers,
        "indexer": indexer,
        "client": client,
        "history_factory": Mock(wraps=commands.build_history_store),
        "history_index_factory": Mock(return_value=history),
        "paper_factory": Mock(return_value=papers),
        "indexer_factory": Mock(return_value=indexer),
        "client_factory": Mock(return_value=client),
        "logger": Mock(),
        "close": Mock(),
        "clear_statuses": Mock(),
        "unlink": Mock(),
    }
    monkeypatch.setattr(commands, "chromadb_available", lambda: True)
    for name, key in (
        ("build_history_store", "history_factory"),
        ("build_index", "history_index_factory"),
        ("build_paper_search_index", "paper_factory"),
        ("build_paper_indexer", "indexer_factory"),
        ("create_client", "client_factory"),
        ("log_retrieval", "logger"),
    ):
        monkeypatch.setattr(commands, name, mocks[key])
    for owner, name, key in (
        (SQLiteHistoryStore, "close", "close"),
        (SQLiteHistoryStore, "clear_paper_statuses", "clear_statuses"),
        (Path, "unlink", "unlink"),
    ):
        original = getattr(owner, name)

        def record(self, *args, original=original, key=key, **kwargs):
            if key == "unlink":
                assert (
                    self
                    == config.chroma_directory / commands.DEFAULT_BM25_INDEX_FILENAME
                )
            mocks[key](*args, **kwargs)
            return original(self, *args, **kwargs)

        monkeypatch.setattr(owner, name, record)
    assert exchange_id == 1
    return mocks


def effect_calls(services):
    # Connections differ between runs; compare their ownership and every call,
    # not Python object identity. Other args (including paths) remain exact.
    return {
        key: [
            (
                name,
                tuple(
                    "sqlite-store" if isinstance(arg, SQLiteHistoryStore) else arg
                    for arg in args
                ),
                kwargs,
            )
            for name, args, kwargs in mock.mock_calls
        ]
        for key, mock in services.items()
    }


@pytest.mark.parametrize("command", COMMANDS)
def test_real_handler_canonical_json_and_effect_counts_match(
    monkeypatch, config, services, command
):
    request = {"command": command, "session_id": "session", "query": "research"}
    expected = commands.dispatch(command, config, request)
    expected_calls = effect_calls(services)
    if command == "reset_all_indices":
        seed_reset_targets(config)
    for mock in services.values():
        mock.reset_mock()
    code, output = main_response(monkeypatch, config, request)
    assert code == 0
    payload = expected if "ok" in expected else {"ok": True, **expected}
    assert output == json.dumps(payload) + "\n"
    assert effect_calls(services) == expected_calls
    if command == "search":
        services["history"].search.assert_called_once_with(
            "research", n_results=5, session_id="session"
        )
        services["close"].assert_called_once()
    elif command == "search_papers":
        services["papers"].count.assert_called_once()
        services["papers"].search.assert_called_once_with("research", n_results=5)
        services["logger"].assert_called_once()
    elif command == "index_papers":
        services["indexer"].index_directory.assert_called_once_with({"p1"})
        services["papers"].refresh_bm25_index.assert_called_once()
        services["close"].assert_called_once()
    elif command == "reset_all_indices":
        services["client"].delete_collection.assert_called_once_with(
            name=commands.DEFAULT_COLLECTION_NAME
        )
        services["unlink"].assert_called_once_with(missing_ok=True)
        services["clear_statuses"].assert_called_once()
        services["close"].assert_called_once()


@pytest.mark.parametrize(
    "command", ["sessions", "messages", "paper_statuses", "bm25_status"]
)
def test_absent_stores_stay_absent(monkeypatch, config, command):
    request = {"command": command, "session_id": "unknown"}
    commands.dispatch(command, config, request)
    assert list(config.database_path.parent.iterdir()) == []


@pytest.mark.parametrize(
    "command", ["messages", "search", "search_papers"]
)
@pytest.mark.parametrize("invalid", [None, "", " ", 1, False, [], {}])
def test_validation_precedes_any_store_work(monkeypatch, config, command, invalid):
    request = {"query": invalid, "session_id": invalid, "paper_id": invalid}
    for name in (
        "build_history_store",
        "build_index",
        "build_paper_indexer",
        "build_paper_search_index",
        "chromadb_available",
    ):
        monkeypatch.setattr(commands, name, Mock(side_effect=AssertionError("I/O")))
    with pytest.raises(ValueError) as expected:
        commands.dispatch(command, config, request)
    code, output = main_response(monkeypatch, config, {"command": command, **request})
    assert code == 1
    assert (
        output
        == json.dumps(
            {"ok": False, "error": str(expected.value), "error_type": "ValueError"}
        )
        + "\n"
    )
    assert list(config.database_path.parent.iterdir()) == []


@pytest.mark.parametrize("command", ["search", "search_papers", "index_papers"])
@pytest.mark.parametrize("mode", ["disabled", "unavailable", "empty"])
def test_disabled_unavailable_and_empty_contracts(
    monkeypatch, config, services, command, mode
):
    if mode == "disabled":
        monkeypatch.setattr(commands, "chromadb_available", lambda: False)
    if mode != "empty":
        services["history_index_factory"].return_value = None
        services["paper_factory"].return_value = None
        services["indexer_factory"].return_value = None
    else:
        services["history"].search.return_value = []
        services["papers"].count.return_value = 0
        services["indexer"].index_directory.return_value = PaperIndexReport(
            config.papers_directory, ()
        )
    request = {"query": "research"}
    expected = commands.dispatch(command, config, request)
    calls = effect_calls(services)
    for mock in services.values():
        mock.reset_mock()
    assert commands.dispatch(command, config, request) == expected
    assert effect_calls(services) == calls
    services["logger"].assert_not_called()


@pytest.mark.parametrize(
    "failure",
    [None, "disabled", "client_factory", "delete", "unlink", "clear_statuses"],
)
def test_reset_is_scoped_best_effort_once_and_preserves_history(
    monkeypatch, config, services, failure
):
    config.papers_directory.mkdir()
    sentinels = (
        config.papers_directory / "original.pdf",
        config.chroma_directory / "unrelated.index",
        config.database_path.parent / "paper_graph.db",
        config.log_path,
        config.retrieval_log_path,
    )
    for path in sentinels:
        path.write_bytes(b"preserve")
    error = RuntimeError("private reset failure")
    if failure == "disabled":
        monkeypatch.setattr(commands, "chromadb_available", lambda: False)
    elif failure == "delete":
        services["client"].delete_collection.side_effect = error
    elif failure:
        services[failure].side_effect = error
    code, output = main_response(monkeypatch, config, {"command": "reset_all_indices"})
    assert code == 0
    assert json.loads(output) == {
        "ok": True,
        "reset": {
            "vector": failure not in {"disabled", "client_factory"},
            "bm25": failure != "unlink",
            "statuses": failure != "clear_statuses",
        },
    }
    assert "private" not in output
    assert services["client_factory"].call_count == (0 if failure == "disabled" else 1)
    assert services["client"].delete_collection.call_count == (
        0 if failure in {"disabled", "client_factory"} else 1
    )
    services["unlink"].assert_called_once_with(missing_ok=True)
    services["clear_statuses"].assert_called_once()
    services["close"].assert_called_once()
    assert (
        config.chroma_directory / commands.DEFAULT_BM25_INDEX_FILENAME
    ).exists() == (failure == "unlink")
    with SQLiteHistoryStore(config.database_path) as store:
        assert store.get_exchange(1, "session").assistant_message.content == "Answer α"
        assert len(store.get_paper_statuses()) == (
            1 if failure == "clear_statuses" else 0
        )
    assert all(path.read_bytes() == b"preserve" for path in sentinels)
