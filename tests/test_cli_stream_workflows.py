"""Offline stream graph ordering, backpressure, cancellation, and durability."""

import io
import json
import os
import subprocess
import sys
import threading
from pathlib import Path

import pytest

from src.cli import __main__ as cli_main
from src.cli import commands, stream_workflows as workflows
from src.cli.runtime import CommandRuntimeError
from src.evaluation_runner import JUDGE_CRITERIA
from src.paper_indexer import (
    PaperIndexer,
    PaperIndexOutcome,
    PaperIndexProgress,
    PaperStart,
)


def counts():
    return {status: 0 for status in ("indexed", "empty", "failed", "skipped")}


def request(total=3):
    return {
        "retrieval_type": "hybrid",
        "test_cases": [
            {"question": str(index), "grading_notes": "grade", "sources": []}
            for index in range(total)
        ],
    }


class Runner:
    def __init__(self):
        self.calls = []

    def run(self, cases, strategy):
        self.calls.append(cases[0]["question"])
        return [
            {
                "case_index": 1,
                "input": cases[0],
                "retrieval_strategy": strategy,
                "retrieved_chunks": [],
                "generated_answer": "answer",
                "judge_scores": {
                    criterion: {
                        "score": 4,
                        "confidence": "high",
                        "reasoning": "ok",
                        "flags": [],
                    }
                    for criterion in JUDGE_CRITERIA
                },
                "error": None,
            }
        ]


@pytest.mark.parametrize(
    "factory", [workflows.get_index_graph, workflows.get_evaluation_graph]
)
def test_graphs_are_cached_ephemeral_and_deterministic(factory):
    graph = factory()
    assert graph is factory()
    assert graph.checkpointer is graph.store is graph.cache is None
    assert graph.get_graph().to_json() == factory.__wrapped__().get_graph().to_json()


@pytest.mark.parametrize("started_events", [False, True])
def test_index_graph_keeps_exact_interleaved_events_and_once_only_effects(
    started_events,
):
    updates = [
        PaperIndexProgress(0, 3),
        PaperIndexProgress(0, 3, started=PaperStart("a", "a.pdf")),
        PaperIndexProgress(0, 3, started=PaperStart("b", "b.pdf")),
        PaperIndexProgress(
            1, 3, outcome=PaperIndexOutcome("b", "b.pdf", "failed", error="failure")
        ),
        PaperIndexProgress(2, 3, outcome=PaperIndexOutcome("a", "a.pdf", "indexed", 2)),
        PaperIndexProgress(3, 3, outcome=PaperIndexOutcome("c", "c.pdf", "skipped")),
    ]
    persisted, refreshed, pulled = [], [], []

    def source():
        try:
            for update in updates:
                pulled.append(update)
                yield update
        finally:
            pulled.append("closed")

    events = workflows.index_events(
        source(),
        counts=counts(),
        started_events=started_events,
        persist=persisted.append,
        refresh=lambda: refreshed.append(True),
    )
    for index, update in enumerate(updates):
        actual = next(events)
        assert pulled == updates[: index + 1]
        if started_events and update.started:
            expected = {
                "event": "started",
                "completed": update.completed,
                "total": 3,
                "paper_id": update.started.paper_id,
                "filename": update.started.filename,
            }
        elif update.outcome is None:
            expected = {"event": "start", "completed": 0, "total": 3}
        else:
            outcome = update.outcome
            expected = {
                "event": "progress",
                "completed": update.completed,
                "total": 3,
                "paper_id": outcome.paper_id,
                "filename": outcome.filename,
                "status": outcome.status,
                "chunk_count": outcome.chunk_count,
                "error": outcome.error,
            }
        assert json.dumps(actual) == json.dumps(expected)
        assert refreshed == []
    assert [outcome.paper_id for outcome in persisted] == ["b", "a"]
    assert next(events) == {
        "ok": True,
        "event": "done",
        "completed": 3,
        "total": 3,
        "indexed": 1,
        "empty": 0,
        "failed": 1,
        "skipped": 1,
    }
    assert refreshed == [True]
    assert pulled[-1] == "closed"
    assert list(events) == list(events) == []
    assert refreshed == [True]


@pytest.mark.parametrize("cut", range(1, 4))
@pytest.mark.parametrize("interrupt", [False, True])
def test_index_close_and_interrupt_do_not_advance_or_refresh(cut, interrupt):
    effects = []

    def source():
        try:
            yield PaperIndexProgress(0, 1)
            effects.append("started")
            yield PaperIndexProgress(0, 1, started=PaperStart("a", "a.pdf"))
            effects.append("indexed")
            yield PaperIndexProgress(
                1, 1, outcome=PaperIndexOutcome("a", "a.pdf", "indexed")
            )
        finally:
            effects.append("closed")

    events = workflows.index_events(
        source(),
        counts=counts(),
        started_events=True,
        persist=lambda _outcome: effects.append("persist"),
        refresh=lambda: effects.append("refresh"),
    )
    for _ in range(cut):
        next(events)
    before = list(effects)
    if interrupt:
        with pytest.raises(KeyboardInterrupt):
            events.throw(KeyboardInterrupt())
    else:
        events.close()
    assert effects == [*before, "closed"]
    assert list(events) == []


@pytest.mark.parametrize("updates", [[], [PaperIndexProgress(0, 1)]])
def test_truncated_index_source_fails_closed(updates):
    refresh = []
    events = workflows.index_events(
        iter(updates), counts=counts(), refresh=lambda: refresh.append(True)
    )
    with pytest.raises(CommandRuntimeError):
        list(events)
    assert refresh == []


def test_index_exception_is_not_retried_and_closes_source():
    calls = []
    error = OSError("existing index failure")

    def source():
        try:
            yield PaperIndexProgress(0, 1)
            calls.append("failed")
            raise error
        finally:
            calls.append("closed")

    events = workflows.index_events(source(), counts=counts())
    next(events)
    with pytest.raises(OSError) as caught:
        next(events)
    assert caught.value is error
    assert calls == ["failed", "closed"]


def test_real_indexer_keeps_its_bounded_pool_and_drains_on_close(tmp_path, monkeypatch):
    for name in ("a", "b", "c", "d"):
        (tmp_path / f"{name}.pdf").touch()
    indexer = PaperIndexer(tmp_path, collection=object(), max_workers=2)
    barrier = threading.Barrier(2, timeout=5)
    active, peak = 0, 0
    lock = threading.Lock()
    workers = []

    def index(path):
        nonlocal active, peak
        with lock:
            active += 1
            peak = max(peak, active)
            workers.append(threading.current_thread())
        barrier.wait()
        with lock:
            active -= 1
        return PaperIndexOutcome(path.stem, path.name, "indexed", 1)

    monkeypatch.setattr(indexer, "index_paper", index)
    events = workflows.index_events(
        indexer.iter_directory(), counts=counts(), started_events=True
    )
    assert next(events)["event"] == "start"
    assert next(events)["event"] == "started"
    events.close()
    assert active == 0 and peak == 2
    assert len(workers) == 4
    assert all(not worker.is_alive() for worker in workers)


def test_evaluation_visits_real_graph_once_per_frame_without_prefetch(monkeypatch):
    graph = workflows.get_evaluation_graph()
    invoke = graph.invoke
    phases = []

    def spy(state, **kwargs):
        phases.append(state["phase"])
        return invoke(state, **kwargs)

    monkeypatch.setattr(graph, "invoke", spy)
    runner = Runner()
    events = commands.handle_evaluation_stream(None, request(2), runner)
    assert phases == runner.calls == []
    expected = [
        "run_start",
        "case_start",
        "case_success",
        "case_start",
        "case_success",
        "complete",
    ]
    for index, name in enumerate(expected):
        assert next(events)["event"] == name
        assert len(phases) == index + 1
        assert len(runner.calls) == min(index // 2, 2)
    assert list(events) == []
    assert phases == [
        "prepare",
        "case_start",
        "case_result",
        "case_start",
        "case_result",
        "complete",
    ]


@pytest.mark.parametrize("cut", range(9))
def test_evaluation_close_keeps_previous_artifact_without_later_cases(tmp_path, cut):
    path = tmp_path / "results.json"
    path.write_text("previous", encoding="utf-8")
    runner = Runner()
    source = commands.handle_evaluation_stream(None, request(), runner)
    events = cli_main._persisted_evaluation_stream(source, results_path=path)
    for _ in range(cut):
        next(events)
    calls = list(runner.calls)
    events.close()
    if cut:
        assert source.gi_frame is None
    assert runner.calls == calls
    assert len(calls) == min(max((cut - 1) // 2, 0), 3)
    assert path.read_text(encoding="utf-8") == "previous"


def test_graph_evaluation_atomic_replace_failure_does_not_replay_cases(
    tmp_path, monkeypatch
):
    path = tmp_path / "results.json"
    path.write_text("previous", encoding="utf-8")
    calls = []

    def fail_replace(*_args):
        calls.append(True)
        raise OSError("replace failed")

    monkeypatch.setattr(os, "replace", fail_replace)
    runner = Runner()
    events = cli_main._persisted_evaluation_stream(
        commands.handle_evaluation_stream(None, request(1), runner),
        results_path=path,
    )
    assert [next(events)["event"] for _ in range(4)][-1] == "complete"
    assert calls == []
    with pytest.raises(OSError, match="replace failed"):
        next(events)
    assert list(events) == []
    assert runner.calls == ["0"] and calls == [True]
    assert path.read_text(encoding="utf-8") == "previous"
    assert list(tmp_path.iterdir()) == [path]


@pytest.mark.parametrize("command", ["index_papers", "evaluate"])
@pytest.mark.parametrize("failure", ["compile", "invoke", "output"])
def test_graph_failures_are_fixed_and_never_emit_success(monkeypatch, command, failure):
    factory_name = (
        "get_index_graph" if command == "index_papers" else "get_evaluation_graph"
    )
    factory = getattr(workflows, factory_name)

    def fail(*_args, **_kwargs):
        raise RuntimeError("private implementation detail")

    if failure == "compile":
        monkeypatch.setattr(workflows, factory_name, fail)
    else:
        monkeypatch.setattr(
            factory(), "invoke", fail if failure == "invoke" else lambda *_a, **_k: {}
        )
    events = (
        workflows.index_events(iter([PaperIndexProgress(0, 0)]), counts=counts())
        if command == "index_papers"
        else commands.handle_evaluation_stream(None, request(), Runner())
    )
    output = io.StringIO()
    monkeypatch.setattr(cli_main.sys, "stdout", output)
    assert cli_main._run_stream(events) == 1
    assert json.loads(output.getvalue()) == {
        "ok": False,
        "error": "Command runtime failed. Check backend logs and retry.",
        "error_type": "CommandRuntimeError",
    }


def test_graphs_disable_external_tracing(monkeypatch):
    import langsmith

    monkeypatch.setenv("LANGSMITH_TRACING", "true")
    monkeypatch.setattr(
        langsmith.Client, "__init__", lambda *_a, **_k: pytest.fail("external trace")
    )
    assert (
        list(commands.handle_evaluation_stream(None, request(1), Runner()))[-1]["event"]
        == "complete"
    )
    assert (
        list(workflows.index_events(iter([PaperIndexProgress(0, 0)]), counts=counts()))[
            -1
        ]["event"]
        == "done"
    )


def test_evaluation_persists_once_and_new_run_has_no_checkpoint_replay(
    tmp_path, monkeypatch
):
    from src import evaluation_results

    path = tmp_path / "results.json"
    saved = []
    persist = evaluation_results.persist_evaluation_results

    def record(*args, **kwargs):
        saved.append(kwargs)
        return persist(*args, **kwargs)

    monkeypatch.setattr(evaluation_results, "persist_evaluation_results", record)
    for _ in range(2):
        runner = Runner()
        events = cli_main._persisted_evaluation_stream(
            commands.handle_evaluation_stream(None, request(2), runner),
            results_path=path,
        )
        assert list(events)[-1]["event"] == "complete"
        assert list(events) == []
        assert runner.calls == ["0", "1"]
    assert len(saved) == 2
    artifact = json.loads(path.read_text(encoding="utf-8"))
    assert artifact["summary"] == {"total": 2, "succeeded": 2, "failed": 0}
    assert len(artifact["results"]) == 2
    assert list(tmp_path.iterdir()) == [path]


def test_keyboard_interrupt_during_evaluation_is_not_a_case_failure(tmp_path):
    class InterruptedRunner:
        def run(self, *_args):
            raise KeyboardInterrupt()

    path = tmp_path / "results.json"
    path.write_text("previous", encoding="utf-8")
    events = cli_main._persisted_evaluation_stream(
        commands.handle_evaluation_stream(None, request(), InterruptedRunner()),
        results_path=path,
    )
    assert next(events)["event"] == "run_start"
    assert next(events)["event"] == "case_start"
    with pytest.raises(KeyboardInterrupt):
        next(events)
    assert list(events) == []
    assert path.read_text(encoding="utf-8") == "previous"


def test_importing_stream_workflows_does_not_construct_graph_or_open_resources(
    tmp_path,
):
    script = """
import sys
def audit(event, args):
    if event in {'socket.connect', 'socket.bind', 'sqlite3.connect', 'os.mkdir'}:
        raise AssertionError('unexpected import side effect: ' + event)
sys.addaudithook(audit)
from src.cli import stream_workflows
assert stream_workflows.get_index_graph.cache_info().currsize == 0
assert stream_workflows.get_evaluation_graph.cache_info().currsize == 0
assert not any(name.startswith(('langgraph', 'langsmith', 'openai', 'chromadb', 'pycozo')) for name in sys.modules)
"""
    result = subprocess.run(
        [sys.executable, "-B", "-c", script],
        cwd=tmp_path,
        env={
            "PATH": os.defpath,
            "HOME": str(tmp_path),
            "PYTHONPATH": str(Path(__file__).resolve().parents[1]),
        },
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout == result.stderr == ""
    assert not list(tmp_path.iterdir())


def test_many_cases_do_not_hit_graph_recursion_limit_or_share_run_state():
    from concurrent.futures import ThreadPoolExecutor

    def run(total):
        runner = Runner()
        events = list(commands.handle_evaluation_stream(None, request(total), runner))
        assert runner.calls == [str(index) for index in range(total)]
        assert len(events) == total * 2 + 2
        return events[-1]["succeeded"]

    with ThreadPoolExecutor(max_workers=3) as pool:
        assert list(pool.map(run, [30, 40, 50])) == [30, 40, 50]


@pytest.mark.parametrize(
    "payload",
    [
        {},
        {"retrieval_type": "unsupported", "test_cases": []},
        {"retrieval_type": "hybrid", "test_cases": []},
        {"retrieval_type": "hybrid", "test_cases": [{}]},
    ],
)
def test_evaluation_validates_before_backend_construction(monkeypatch, payload):
    monkeypatch.setattr(
        commands,
        "build_evaluation_runner",
        lambda _config: pytest.fail("backend built"),
    )
    with pytest.raises(ValueError):
        list(commands.handle_evaluation_stream(None, payload))
