"""Offline coverage for the Wave 1 graph boundary, not handler migrations."""

import io
import json
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

from src.cli import __main__ as cli_main
from src.cli import commands, dispatcher, runtime
from src.cli.config import CLIConfig

UNARY = (
    "health",
    "chat",
    "sessions",
    "messages",
    "search",
    "search_papers",
    "index_papers",
    "paper_statuses",
    "bm25_status",
    "reset_all_indices",
    "graph_query",
)
STREAMING = ("chat", "index_papers", "evaluate")
ROOT = Path(__file__).resolve().parents[1]


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


def test_compiled_graph_is_reusable_deterministic_and_ephemeral():
    graph = runtime.get_command_graph()
    assert runtime.get_command_graph() is graph
    assert graph.checkpointer is None
    assert graph.store is None
    assert graph.cache is None
    assert (
        graph.get_graph().to_json()
        == runtime.get_command_graph.__wrapped__().get_graph().to_json()
    )
    assert set(graph.nodes) == {"__start__", "validate", "unary", "stream"}
    assert tuple(name for name, spec in runtime.COMMANDS.items() if spec.unary) == UNARY
    assert (
        tuple(name for name, spec in runtime.COMMANDS.items() if spec.streaming)
        == STREAMING
    )
    with pytest.raises(TypeError):
        runtime.COMMANDS["extra"] = runtime.CommandSpec()


@pytest.mark.parametrize("command", UNARY)
def test_every_unary_dispatch_enters_compiled_graph_once(monkeypatch, config, command):
    calls = []
    visited = []
    request = {"command": command, "marker": object()}
    expected = {"value": object()}
    graph = runtime.get_command_graph()
    invoke = graph.invoke

    def spy(state, **kwargs):
        visited.append(state)
        return invoke(state, **kwargs)

    def handler(cfg, payload=None):
        calls.append((cfg, payload))
        return expected

    monkeypatch.setattr(graph, "invoke", spy)
    owner = dispatcher if command == "graph_query" else commands
    monkeypatch.setattr(owner, f"handle_{command}", handler)
    assert dispatcher.dispatch(command, config, request) is expected
    assert len(visited) == len(calls) == 1
    assert visited[0] == {"command": command, "request": request, "streaming": False}
    assert calls[0][0] is config
    assert calls[0][1] is None or calls[0][1] is request


@pytest.mark.parametrize("command", [None, "", "absent", "evaluate", 1, True, [], {}])
def test_unknown_and_malformed_commands_match_legacy(config, command):
    with pytest.raises(Exception) as legacy:
        commands.dispatch(command, config, {})
    with pytest.raises(type(legacy.value)) as actual:
        dispatcher.dispatch(command, config, {})
    assert str(actual.value) == str(legacy.value)


def test_handler_failure_is_not_retried_or_rewritten():
    calls = []
    error = ValueError("session_id is required and must be a non-empty string")

    def fail(*_args):
        calls.append(True)
        raise error

    with pytest.raises(ValueError) as actual:
        runtime.run_command("chat", {}, fail)
    assert actual.value is error
    assert calls == [True]


@pytest.mark.parametrize("failure_site", ["compile", "invoke", "output"])
def test_new_graph_failures_are_sanitized(monkeypatch, failure_site):
    def fail(*_args, **_kwargs):
        raise RuntimeError("private-runtime-detail /private/config")

    if failure_site == "compile":
        monkeypatch.setattr(runtime, "get_command_graph", fail)
    else:
        monkeypatch.setattr(
            runtime.get_command_graph(),
            "invoke",
            fail if failure_site == "invoke" else lambda *_a, **_kw: {},
        )
    with pytest.raises(runtime.CommandRuntimeError) as actual:
        runtime.run_command("health", {}, lambda *_args: {})
    assert str(actual.value) == "Command runtime failed. Check backend logs and retry."
    assert actual.value.__suppress_context__


def test_concurrent_invocations_do_not_share_context_or_state():
    def invoke(index):
        expected = {"index": index}
        return runtime.run_command(
            "health", expected, lambda _cmd, req: {"request": req, "context": index}
        )

    with ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(invoke, range(20)))
    assert results == [{"request": {"index": i}, "context": i} for i in range(20)]


@pytest.mark.parametrize("command", STREAMING)
def test_stream_adapter_is_lazy_pull_ordered_and_closes_without_replay(command):
    effects = []
    first = {"event": "progress"}
    terminal = {"done": True}

    def events():
        try:
            effects.append("start")
            yield first
            effects.append("next")
            yield terminal
            effects.append("after-terminal")
        finally:
            effects.append("closed")

    stream = runtime.stream_command(command, {}, events)
    assert effects == []
    assert next(stream) is first
    assert effects == ["start"]
    assert next(stream) is terminal
    assert effects == ["start", "next"]
    stream.close()
    assert effects == ["start", "next", "closed"]


@pytest.mark.parametrize("during_iteration", [False, True])
def test_stream_errors_keep_existing_boundary(during_iteration):
    error = ValueError("existing validation failure")
    calls = []

    def iterator():
        yield {"event": "progress"}
        raise error

    def factory():
        calls.append(True)
        if not during_iteration:
            raise error
        return iterator()

    stream = runtime.stream_command("chat", {}, factory)
    if during_iteration:
        assert next(stream) == {"event": "progress"}
    with pytest.raises(ValueError) as actual:
        next(stream)
    assert actual.value is error
    assert calls == [True]


@pytest.mark.parametrize("command", STREAMING)
def test_main_streams_through_graph_without_changing_frames(
    monkeypatch, config, command
):
    expected = [{"event": "progress", "value": 1}, {"done": True, "ok": True}]
    calls = []
    graph = runtime.get_command_graph()
    invoke = graph.invoke

    def spy(state, **kwargs):
        calls.append(state)
        return invoke(state, **kwargs)

    output = io.StringIO()
    monkeypatch.setattr(graph, "invoke", spy)
    monkeypatch.setattr(cli_main, "load_dotenv_if_present", lambda: None)
    monkeypatch.setattr(CLIConfig, "from_env", lambda: config)
    handler = (
        "handle_evaluation_stream"
        if command == "evaluate"
        else f"handle_{command}_stream"
    )
    monkeypatch.setattr(cli_main, handler, lambda *_args: iter(expected))
    monkeypatch.setattr(
        sys, "stdin", io.StringIO(json.dumps({"command": command, "stream": True}))
    )
    monkeypatch.setattr(sys, "stdout", output)
    assert cli_main.main() == 0
    assert [json.loads(line) for line in output.getvalue().splitlines()] == expected
    assert len(calls) == 1 and calls[0]["streaming"] is True


def test_environment_loading_still_precedes_config_and_dispatch(monkeypatch, config):
    calls = []
    monkeypatch.setattr(
        cli_main, "load_dotenv_if_present", lambda: calls.append("dotenv")
    )
    monkeypatch.setattr(CLIConfig, "from_env", lambda: calls.append("config") or config)
    monkeypatch.setattr(
        commands, "handle_health", lambda _cfg: calls.append("handler") or {"value": 1}
    )
    monkeypatch.setattr(sys, "stdin", io.StringIO('{"command":"health"}'))
    output = io.StringIO()
    monkeypatch.setattr(sys, "stdout", output)
    assert cli_main.main() == 0
    assert calls == ["dotenv", "config", "handler"]
    assert output.getvalue() == '{"ok": true, "value": 1}\n'


def subprocess_environment(tmp_path):
    return {
        "PATH": os.defpath,
        "HOME": str(tmp_path),
        "PYTHONPATH": str(ROOT),
        "PYTHON_DOTENV_DISABLED": "1",
        "LANGSMITH_TRACING": "false",
        "CAPSTONE_DB_PATH": str(tmp_path / "history.db"),
        "CAPSTONE_CHROMA_DIR": str(tmp_path / "chroma"),
        "CAPSTONE_PAPERS_DIR": str(tmp_path / "papers"),
        "CAPSTONE_LOG_PATH": str(tmp_path / "chat.log"),
        "CAPSTONE_RETRIEVAL_LOG_PATH": str(tmp_path / "retrieval.log"),
    }


@pytest.mark.parametrize(
    "raw,expected",
    [
        (
            "",
            {"ok": False, "error": "unknown command None", "error_type": "ValueError"},
        ),
        ("[]", {"ok": False, "error": "request must be a JSON object"}),
        (
            '{"command":"no-such-command"}',
            {
                "ok": False,
                "error": "unknown command 'no-such-command'",
                "error_type": "ValueError",
            },
        ),
        (
            '{"command":[]}',
            {
                "ok": False,
                "error": "unhashable type: 'list'",
                "error_type": "TypeError",
            },
        ),
        (
            '{"command":{}}',
            {
                "ok": False,
                "error": "unhashable type: 'dict'",
                "error_type": "TypeError",
            },
        ),
        (
            '{"command":"evaluate"}',
            {
                "ok": False,
                "error": "unknown command 'evaluate'",
                "error_type": "ValueError",
            },
        ),
        (
            '{"command":"health"}\n{}',
            {
                "ok": False,
                "error": "invalid JSON request: Extra data: line 2 column 1 (char 21)",
            },
        ),
    ],
)
def test_real_module_malformed_request_contract(tmp_path, raw, expected):
    result = subprocess.run(
        [sys.executable, "-m", "src.cli"],
        input=raw,
        text=True,
        capture_output=True,
        cwd=tmp_path,
        env=subprocess_environment(tmp_path),
        timeout=30,
    )
    assert result.returncode == 1
    assert result.stdout == json.dumps(expected) + "\n"
    assert result.stderr == ""
    assert not list(tmp_path.iterdir())


def test_import_has_no_config_graph_provider_store_or_network_work(tmp_path):
    script = """
import sys
def audit(event, args):
    if event in {'socket.connect', 'socket.bind', 'sqlite3.connect', 'os.mkdir'}:
        raise AssertionError('unexpected import side effect: ' + event)
sys.addaudithook(audit)
from src.cli import config
config.load_dotenv_if_present = lambda: (_ for _ in ()).throw(AssertionError('dotenv'))
config.CLIConfig.from_env = lambda: (_ for _ in ()).throw(AssertionError('config'))
from src.cli import __main__, dispatcher, runtime
assert runtime.get_command_graph.cache_info().currsize == 0
assert not any(name.startswith(('langgraph', 'langsmith', 'openai', 'chromadb', 'pycozo')) for name in sys.modules)
assert 'src.cli.commands' not in sys.modules
"""
    result = subprocess.run(
        [sys.executable, "-B", "-c", script],
        text=True,
        capture_output=True,
        cwd=tmp_path,
        env=subprocess_environment(tmp_path),
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout == result.stderr == ""
    assert not list(tmp_path.iterdir())


def test_graph_tracing_does_not_create_external_persistence(monkeypatch):
    import langsmith

    calls = []
    tracing_context = langsmith.tracing_context

    def record(**kwargs):
        calls.append(kwargs)
        return tracing_context(**kwargs)

    monkeypatch.setenv("LANGSMITH_TRACING", "true")
    monkeypatch.setattr(langsmith, "tracing_context", record)
    monkeypatch.setattr(
        langsmith.Client,
        "__init__",
        lambda *_a, **_kw: pytest.fail("external tracing client created"),
    )
    assert runtime.run_command("health", {}, lambda *_args: {"ok": True}) == {
        "ok": True
    }
    assert calls == [{"enabled": False}]


@pytest.mark.parametrize("consume_terminal", [False, True])
def test_evaluation_adapter_preserves_terminal_persistence_boundary(
    monkeypatch, tmp_path, consume_terminal
):
    saved = []
    terminal = {
        "event": "complete",
        "retrieval_type": "bm25",
        "aggregate_scores": {},
        "succeeded": 0,
        "failed": 0,
    }
    monkeypatch.setattr(
        "src.evaluation_results.persist_evaluation_results",
        lambda *args, **kwargs: saved.append((args, kwargs)),
    )
    stream = cli_main._persisted_evaluation_stream(
        runtime.stream_command("evaluate", {}, lambda: iter([terminal])),
        results_path=tmp_path / "results.json",
    )
    assert next(stream) is terminal
    assert saved == []
    if consume_terminal:
        with pytest.raises(StopIteration):
            next(stream)
        assert len(saved) == 1
        with pytest.raises(StopIteration):
            next(stream)
        assert len(saved) == 1
    else:
        stream.close()
        assert saved == []


def test_interrupt_is_not_retried_or_converted_into_runtime_failure():
    def interrupt(*_args):
        raise KeyboardInterrupt()

    with pytest.raises(KeyboardInterrupt):
        runtime.run_command("health", {}, interrupt)


@pytest.mark.parametrize("command", ["health", *STREAMING])
def test_main_sanitizes_new_graph_failures(monkeypatch, config, command):
    def fail():
        raise RuntimeError("private-runtime-detail /private/config")

    monkeypatch.setattr(runtime, "get_command_graph", fail)
    monkeypatch.setattr(cli_main, "load_dotenv_if_present", lambda: None)
    monkeypatch.setattr(CLIConfig, "from_env", lambda: config)
    monkeypatch.setattr(
        sys, "stdin", io.StringIO(json.dumps({"command": command, "stream": True}))
    )
    output = io.StringIO()
    monkeypatch.setattr(sys, "stdout", output)
    assert cli_main.main() == 1
    expected = (
        {
            "ok": False,
            "error": "Evaluation transport failed. Check backend logs and retry.",
        }
        if command == "evaluate"
        else {
            "ok": False,
            "error": "Command runtime failed. Check backend logs and retry.",
            "error_type": "CommandRuntimeError",
        }
    )
    assert json.loads(output.getvalue()) == expected


def test_real_module_unary_health_has_only_existing_response_fields(tmp_path):
    result = subprocess.run(
        [sys.executable, "-m", "src.cli"],
        input='{"command":"health"}',
        text=True,
        capture_output=True,
        cwd=tmp_path,
        env=subprocess_environment(tmp_path),
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    assert result.stderr == ""
    assert len(result.stdout.splitlines()) == 1
    assert json.loads(result.stdout) == {
        "ok": True,
        "chat_mode": "mock",
        "search_enabled": commands.chromadb_available(),
        "database_path": str(tmp_path / "history.db"),
        "chroma_directory": str(tmp_path / "chroma"),
        "papers_directory": str(tmp_path / "papers"),
    }
    assert not list(tmp_path.iterdir())
