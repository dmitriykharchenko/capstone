"""Offline parity and exactly-once ownership for the named unary graph nodes."""

import io
import json
import sqlite3
import sys
from concurrent.futures import ThreadPoolExecutor
from dataclasses import replace
from pathlib import Path
from unittest.mock import Mock

import pytest

from src.cli import __main__ as cli_main
from src.cli import commands, dispatcher, graph_queries, runtime, unary
from src.cli.config import CLIConfig
from src.graph_store import (
    CozoGraphStore,
    GraphStoreConfigurationError,
    GraphStoreLockedError,
    stable_node_id,
)
from src.paper_indexer import PaperIndexOutcome, PaperIndexReport
from src.paper_search import PaperMatch
from src.sqlite_history import SQLiteHistoryStore

COMMANDS = tuple(unary.UNARY_COMMANDS)


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


def direct(command, config, request):
    if command == "graph_query":
        return dispatcher.handle_graph_query(config, request)
    return commands.dispatch(command, config, request)


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
    graph = Mock()
    graph.__enter__ = Mock(return_value=graph)
    graph.__exit__ = Mock(return_value=False)
    graph.get_paper.return_value = None
    graph_factory = Mock(return_value=graph)
    client = Mock()
    mocks = {
        "history": history,
        "papers": papers,
        "indexer": indexer,
        "graph": graph,
        "graph_factory": graph_factory,
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
    monkeypatch.setattr(
        dispatcher,
        "handle_graph_query",
        lambda cfg, req: graph_queries.handle_graph_query(
            cfg, req, store_factory=graph_factory
        ),
    )
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


def test_unary_graph_has_only_named_ephemeral_nodes():
    graph = unary.get_unary_graph()
    assert graph is unary.get_unary_graph()
    assert graph.checkpointer is graph.store is graph.cache is None
    assert set(graph.nodes) == {"__start__", *COMMANDS}
    assert set(COMMANDS) == set(runtime.COMMANDS) - {"chat", "evaluate"}
    assert graph.get_graph().to_json() == (
        unary.get_unary_graph.__wrapped__().get_graph().to_json()
    )
    with pytest.raises(TypeError):
        unary.UNARY_COMMANDS["chat"] = True


@pytest.mark.parametrize("command", COMMANDS)
def test_dispatch_executes_selected_node_once_without_legacy_router(
    monkeypatch, config, command
):
    expected = {"marker": object()}
    handler = Mock(return_value=expected)
    owner = dispatcher if command == "graph_query" else commands
    monkeypatch.setattr(owner, f"handle_{command}", handler)
    monkeypatch.setattr(
        commands, "dispatch", Mock(side_effect=AssertionError("legacy"))
    )
    graph = unary.get_unary_graph()
    invoke = Mock(wraps=graph.invoke)
    monkeypatch.setattr(graph, "invoke", invoke)
    request = {"command": command, "marker": object()}
    assert dispatcher.dispatch(command, config, request) is expected
    invoke.assert_called_once()
    assert invoke.call_args.args == ({"command": command, "request": request},)
    handler.assert_called_once_with(
        config, *([request] if unary.UNARY_COMMANDS[command] else [])
    )
    handler.reset_mock()
    updates = list(
        graph.stream(
            {"command": command, "request": request},
            context=unary.UnaryInvocation(config, dispatcher.handle_graph_query),
            stream_mode="updates",
        )
    )
    assert updates == [{command: {"result": expected}}]
    assert handler.call_count == 1


@pytest.mark.parametrize("command", COMMANDS)
def test_real_handler_canonical_json_and_effect_counts_match(
    monkeypatch, config, services, command
):
    request = {"command": command, "session_id": "session", "query": "research"}
    if command == "graph_query":
        request.update(query="paper_details", paper_id="p1")
    expected = direct(command, config, request)
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
    elif command == "graph_query":
        services["graph_factory"].assert_called_once()
        services["graph"].get_paper.assert_called_once_with("p1")
        services["graph"].__exit__.assert_called_once()
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
    expected = direct(command, config, request)
    assert dispatcher.dispatch(command, config, request) == expected
    assert list(config.database_path.parent.iterdir()) == []


@pytest.mark.parametrize(
    "command", ["messages", "search", "search_papers", "graph_query"]
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
        direct(command, config, request)
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


@pytest.mark.parametrize("command", ["search", "search_papers"])
@pytest.mark.parametrize("limit", [None, True, 0, -1, "3", 2.5, 3])
def test_search_defaults_and_filter_order_are_unchanged(
    monkeypatch, config, services, command, limit
):
    request = {"query": " research ", "session_id": [], "limit": limit}
    dispatcher.dispatch(command, config, request)
    expected = {"n_results": 3 if limit == 3 else 5}
    if command == "search":
        expected["session_id"] = None
    services[
        "history" if command == "search" else "papers"
    ].search.assert_called_once_with(" research ", **expected)


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
    expected = direct(command, config, request)
    calls = effect_calls(services)
    for mock in services.values():
        mock.reset_mock()
    assert dispatcher.dispatch(command, config, request) == expected
    assert effect_calls(services) == calls
    services["logger"].assert_not_called()


@pytest.mark.parametrize("error", [GraphStoreConfigurationError, GraphStoreLockedError])
def test_graph_store_errors_keep_sanitized_json(monkeypatch, config, services, error):
    services["graph_factory"].side_effect = error("private store detail")
    request = {"command": "graph_query", "query": "paper_details", "paper_id": "p1"}
    expected = direct("graph_query", config, request)
    services["graph_factory"].reset_mock()
    code, output = main_response(monkeypatch, config, request)
    assert code == 0
    assert output == json.dumps({"ok": True, **expected}) + "\n"
    assert "private" not in output
    services["graph_factory"].assert_called_once()


@pytest.mark.parametrize("site", ["compile", "invoke", "output"])
def test_unary_graph_failures_are_sanitized_without_domain_work(
    monkeypatch, config, site
):
    fail = Mock(side_effect=RuntimeError("private graph detail"))
    if site == "compile":
        monkeypatch.setattr(unary, "get_unary_graph", fail)
    else:
        monkeypatch.setattr(
            unary.get_unary_graph(),
            "invoke",
            fail if site == "invoke" else Mock(return_value={}),
        )
    handler = Mock()
    monkeypatch.setattr(commands, "handle_reset_all_indices", handler)
    code, output = main_response(monkeypatch, config, {"command": "reset_all_indices"})
    assert code == 1
    assert json.loads(output) == {
        "ok": False,
        "error": "Command runtime failed. Check backend logs and retry.",
        "error_type": "CommandRuntimeError",
    }
    handler.assert_not_called()


@pytest.mark.parametrize("command", COMMANDS)
def test_handler_failure_keeps_identity_and_is_never_retried(
    monkeypatch, config, command
):
    error = ValueError("existing fixed handler error")
    handler = Mock(side_effect=error)
    owner = dispatcher if command == "graph_query" else commands
    monkeypatch.setattr(owner, f"handle_{command}", handler)
    with pytest.raises(ValueError) as actual:
        dispatcher.dispatch(command, config, {})
    assert actual.value is error
    assert handler.call_count == 1


def test_concurrent_unary_invocations_keep_config_and_payload_local(
    monkeypatch, config
):
    monkeypatch.setattr(
        commands,
        "handle_messages",
        lambda cfg, req: {"path": str(cfg.database_path), **req},
    )

    def run(index):
        cfg = replace(config, database_path=config.database_path.parent / str(index))
        return dispatcher.dispatch("messages", cfg, {"session_id": index})

    with ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(run, range(12)))
    assert results == [
        {"path": str(config.database_path.parent / str(i)), "session_id": i}
        for i in range(12)
    ]


def test_unary_tracing_remains_disabled(monkeypatch, config):
    import langsmith

    monkeypatch.setenv("LANGSMITH_TRACING", "true")
    monkeypatch.setattr(
        langsmith.Client,
        "__init__",
        Mock(side_effect=AssertionError("external tracing")),
    )
    monkeypatch.setattr(commands, "chromadb_available", lambda: False)
    assert dispatcher.dispatch("health", config, {})["chat_mode"] == "mock"


@pytest.mark.parametrize(
    "query,key,identifier",
    [
        ("paper_details", "paper_id", "p1"),
        ("papers_by_author", "author_id", stable_node_id("author", "Ada")),
        ("papers_by_topic", "topic_id", stable_node_id("topic", "Graphs")),
        ("topics_by_author", "author_id", stable_node_id("author", "Ada")),
        ("coauthors", "author_id", stable_node_id("author", "Ada")),
        ("neighborhood", "node_id", stable_node_id("author", "Ada")),
    ],
)
def test_all_graph_reads_match_canonical_json(
    monkeypatch, config, query, key, identifier
):
    with CozoGraphStore(config.chroma_directory.parent / "paper_graph.db") as store:
        store.replace_paper("p1", "First", ["Ada", "Bob"], ["Graphs"])
        store.replace_paper("p2", "Second", ["Ada", "Carol"], ["Graphs", "Logic"])
    request = {
        "command": "graph_query",
        "query": query,
        key: identifier,
        "max_depth": 2,
    }
    expected = graph_queries.handle_graph_query(config, request)
    code, output = main_response(monkeypatch, config, request)
    assert code == 0
    assert output == json.dumps({"ok": True, **expected}) + "\n"


@pytest.mark.parametrize("command", ["search", "search_papers", "index_papers"])
def test_domain_failure_preserves_cleanup_and_no_followup_effects(
    config, services, command
):
    error = RuntimeError("existing domain failure")
    if command == "search":
        services["history"].search.side_effect = error
    elif command == "search_papers":
        services["papers"].search.side_effect = error
    else:
        services["indexer"].index_directory.side_effect = error
    request = {"query": "research"}
    if command == "search_papers":
        assert dispatcher.dispatch(command, config, request) == {
            "enabled": True,
            "matches": [],
            "reason": str(error),
        }
        services["papers"].search.assert_called_once()
    else:
        with pytest.raises(RuntimeError) as actual:
            dispatcher.dispatch(command, config, request)
        assert actual.value is error
        services["close"].assert_called_once()
    services["logger"].assert_not_called()
    services["papers"].refresh_bm25_index.assert_not_called()


def test_missing_paper_directory_keeps_legacy_reason_and_closes_store(config, services):
    services["indexer"].index_directory.side_effect = NotADirectoryError(
        "missing directory"
    )
    assert dispatcher.dispatch("index_papers", config, {}) == {
        "enabled": True,
        "indexed": 0,
        "papers": [],
        "reason": "missing directory",
    }
    services["indexer"].index_directory.assert_called_once_with({"p1"})
    services["close"].assert_called_once()
    services["papers"].refresh_bm25_index.assert_not_called()


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


def test_reset_status_transaction_rolls_back_as_one_unit(config, services):
    with SQLiteHistoryStore(config.database_path) as store:
        store.record_paper_status("p2", "two.pdf", "indexed")
    with sqlite3.connect(config.database_path) as connection:
        connection.execute(
            "CREATE TRIGGER reject_delete BEFORE DELETE ON paper_index_status "
            "WHEN OLD.paper_id = 'p2' BEGIN SELECT RAISE(ABORT, 'blocked'); END"
        )
    result = dispatcher.dispatch("reset_all_indices", config, {})
    assert result["reset"] == {"vector": True, "bm25": True, "statuses": False}
    services["clear_statuses"].assert_called_once()
    with SQLiteHistoryStore(config.database_path) as store:
        assert [record.paper_id for record in store.get_paper_statuses()] == [
            "p1",
            "p2",
        ]
        assert len(store.get_messages("session")) == 2


def test_bm25_corruption_and_sorted_unique_ids_do_not_trigger_a_rebuild(
    config, services
):
    path = config.chroma_directory / commands.DEFAULT_BM25_INDEX_FILENAME
    path.write_text("not json")
    assert dispatcher.dispatch("bm25_status", config, {}) == {
        "built": False,
        "chunk_count": 0,
        "paper_ids": [],
    }
    commands.BM25Index.build(
        [
            {"paper_id": identifier, "text": "research"}
            for identifier in ("b", "a", "b", "")
        ]
    ).save(path)
    assert dispatcher.dispatch("bm25_status", config, {}) == {
        "built": True,
        "chunk_count": 4,
        "paper_ids": ["a", "b"],
    }
    services["indexer_factory"].assert_not_called()
    services["paper_factory"].assert_not_called()


def test_interrupt_is_not_converted_or_retried(monkeypatch, config):
    handler = Mock(side_effect=KeyboardInterrupt)
    monkeypatch.setattr(commands, "handle_index_papers", handler)
    with pytest.raises(KeyboardInterrupt):
        dispatcher.dispatch("index_papers", config, {})
    handler.assert_called_once_with(config)
