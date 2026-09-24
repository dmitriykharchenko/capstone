"""Offline contract/recovery tests; every database belongs to pytest tmp_path."""

from copy import deepcopy
import json
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
from types import SimpleNamespace

import pytest

from src.cli.orchestration import GRAPH_VERSION, control, stream
from src.orchestration_runtime import IteratorHandle, specialist_steps
from src.orchestration_store import SQLiteOrchestrationStore
from src.orchestration_wire import ERROR, StreamValidator, loads, validate_event
from src.sqlite_history import SQLiteHistoryStore

REQUEST = dict(
    command="orchestration_start",
    session_id="session",
    run_id="run",
    turn_id="turn",
    message="What is retrieval?",
)


class Model:
    def invoke(self, messages):
        if messages[0].content.startswith("INTAKE:"):
            return json.dumps({"queries": [], "questions": ["What is retrieval?"]})
        if messages[0].content.startswith("QUESTION-HANDLER:"):
            return json.dumps(
                {
                    "answer": "No evidence available.",
                    "evidence_ids": [],
                    "followups": [],
                }
            )
        return json.dumps(
            {"assistant_text": "Retrieval finds evidence. No sources were available."}
        )


class Executor:
    def __init__(self, model=None):
        self.handles = []
        self.model = model or Model()

    def submit(self, operation_id, dispatch):
        handle = IteratorHandle(specialist_steps(dispatch, self.model))
        self.handles.append(handle)
        return handle


@pytest.fixture
def config(tmp_path, monkeypatch):
    import socket

    def denied(*args, **kwargs):
        raise AssertionError("Network access is forbidden")

    monkeypatch.setattr(socket.socket, "connect", denied)
    return SimpleNamespace(
        database_path=tmp_path / "chat_history.sqlite3", has_api_key=False
    )


def collect(config, request=REQUEST, **kwargs):
    kwargs.setdefault("executor", Executor())
    return list(stream(config, request, **kwargs))


def test_fresh_stream_validated_and_canonical(config):
    events = collect(config)
    assert events[-1]["event"] == "done", events
    validator = StreamValidator("session", "run", "turn")
    for event in events:
        validator.accept(deepcopy(event))
    assert sum(e["event"] in ("done", "error") for e in events) == 1
    assert (
        "".join(e["delta"] for e in events if e["event"] == "final_delta")
        == events[-1]["reply"]
    )
    with SQLiteHistoryStore(config.database_path) as history:
        assert len(history.get_messages("session")) == 2
        assert history.get_messages("session")[-1].content == events[-1]["reply"]
    replay = collect(config)
    assert [e["event"] for e in replay] == ["snapshot", "done"]
    assert replay[-1]["exchange_id"] == events[-1]["exchange_id"]


def test_close_resume_and_trace(config):
    events = stream(config, REQUEST, executor=Executor())
    first = next(events)
    for event in events:
        if event["event"] == "question_answered":
            break
    events.close()
    pending = control(
        config, dict(command="orchestration_pending", session_id="session")
    )["pending"]
    assert pending["snapshot"]["status"] == "interrupted"
    resumed = collect(config, dict(REQUEST, command="orchestration_resume"))
    assert resumed[0]["event"] == "snapshot"
    assert resumed[0]["sequence"] > first["sequence"]
    assert all(e["sequence"] > resumed[0]["sequence"] for e in resumed[1:])
    assert not any(e["event"] == "question_answered" for e in resumed[1:])
    assert resumed[-1]["event"] == "done", resumed
    trace = control(
        config,
        dict(
            command="orchestration_trace",
            session_id="session",
            exchange_id=resumed[-1]["exchange_id"],
        ),
    )
    assert trace["snapshot"]["snapshot"]["status"] == "completed"
    assert control(
        config, dict(command="orchestration_pending", session_id="session")
    ) == {"pending": None}


def test_discard_idempotent_no_history_deletion(config):
    events = stream(config, REQUEST, executor=Executor())
    next(events)
    events.close()
    request = dict(REQUEST, command="orchestration_discard")
    first = control(config, request)
    assert first == control(config, request)
    assert first["snapshot"]["snapshot"]["status"] == "discarded"
    assert (
        collect(config, dict(REQUEST, command="orchestration_resume"))[-1]["event"]
        == "error"
    )
    with SQLiteHistoryStore(config.database_path) as history:
        assert history.get_messages("session") == []


@pytest.mark.parametrize(
    "mutation",
    [
        lambda e: e.update(version=2),
        lambda e: e.update(sequence=True),
        lambda e: e.update(run_id="bad space"),
        lambda e: e.update(raw_provider={}),
        lambda e: e.update(event="unknown"),
        lambda e: e.update(turn_id=""),
    ],
)
def test_malformed_envelopes(config, mutation):
    event = collect(config)[0]
    mutation(event)
    with pytest.raises(ValueError, match=ERROR):
        validate_event(event)


def test_duplicates_order_and_terminal(config):
    events = collect(config)
    validator = StreamValidator("session", "run", "turn")
    validator.accept(deepcopy(events[0]))
    with pytest.raises(ValueError):
        validator.accept(deepcopy(events[0]))
    for event in events[1:]:
        validator.accept(deepcopy(event))
    with pytest.raises(ValueError):
        validator.accept(deepcopy(events[-1]))


def test_duplicate_json_keys_and_oversize():
    for raw in ('{"event":"done","event":"error"}', '"' + "x" * 1048576 + '"'):
        with pytest.raises(ValueError, match=ERROR):
            loads(raw)


def test_shared_type_boundary_corpus():
    from src.orchestration_wire import SCHEMA, validate_type

    path = Path(__file__).parent / "fixtures" / "orchestration-types.json"
    for case in json.loads(path.read_text()):
        if case["valid"]:
            validate_type(case["value"], case["type"])
        else:
            with pytest.raises(ValueError, match=ERROR):
                validate_type(case["value"], case["type"])
    for name, rule in SCHEMA["types"].items():
        if rule["kind"] == "text":
            validate_type("é" * (rule["max"] // 2), name)
            with pytest.raises(ValueError, match=ERROR):
                validate_type("é" * (rule["max"] // 2 + 1), name)
        elif rule["kind"] == "array":
            with pytest.raises(ValueError, match=ERROR):
                validate_type([None] * (rule["max"] + 1), name)


def test_no_orchestration_history_authority(config):
    collect(config)
    with sqlite3.connect(config.database_path) as db:
        tables = {
            r[0]
            for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")
        }
    assert not tables & {"runs", "checkpoints", "events", "adapter_requests"}
    with SQLiteOrchestrationStore(
        config.database_path.parent / "chat_orchestration.sqlite3"
    ) as store:
        assert (
            store.request("run", "session", GRAPH_VERSION).query == REQUEST["message"]
        )
        assert store.load("run", "session", GRAPH_VERSION).run.exchange_id == 1


def golden():
    return [
        loads(line)
        for line in (Path(__file__).parent / "fixtures/orchestration.ndjson")
        .read_text()
        .splitlines()
    ]


def test_golden_ndjson(config):
    events = collect(config)
    for event in events:
        for key in ("question", "agent", "tool"):
            item = event.get(key)
            if item is None:
                continue
            if item.get("question_id"):
                item["question_id"] = "q"
            if str(item.get("agent_id", "")).startswith("handler-"):
                item["agent_id"] = "handler"
            if "tool_call_id" in item:
                item["tool_call_id"] = item["tool_name"]
    assert events == golden()


@pytest.mark.parametrize("boundary", range(19))
def test_sigkill_recovery_at_every_golden_event(config, boundary):
    script = """
import json,runpy,sys
from pathlib import Path
from types import SimpleNamespace
m=runpy.run_path('tests/test_orchestration_transport.py')
config=SimpleNamespace(database_path=Path(sys.argv[1]),has_api_key=False)
for event in m['stream'](config,m['REQUEST'],executor=m['Executor']()):
 print(json.dumps(event),flush=True)
 sys.stdin.readline()
"""
    child = subprocess.Popen(
        [sys.executable, "-B", "-c", script, str(config.database_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    observed = []
    try:
        for index in range(boundary + 1):
            assert select.select([child.stdout], [], [], 15)[0], (
                "Synthetic child stalled"
            )
            observed.append(loads(child.stdout.readline()))
            if index < boundary:
                child.stdin.write("next\n")
                child.stdin.flush()
    finally:
        child.kill()
        child.wait(timeout=10)
        child.stdin.close()
        child.stdout.close()
    resumed = collect(config, dict(REQUEST, command="orchestration_resume"))
    assert resumed[0]["event"] == "snapshot"
    assert (
        resumed[0]["sequence"] >= observed[-1]["sequence"]
        or observed[-1]["event"] == "done"
    )
    assert resumed[-1]["event"] == "done", resumed
    validator = StreamValidator("session", "run", "turn")
    for event in resumed:
        validator.accept(deepcopy(event))
    with SQLiteHistoryStore(config.database_path) as history:
        assert len(history.get_messages("session")) == 2
    assert (
        collect(config, dict(REQUEST, command="orchestration_resume"))[-1][
            "exchange_id"
        ]
        == resumed[-1]["exchange_id"]
    )


@pytest.mark.parametrize("position", range(19))
@pytest.mark.parametrize(
    "field,value",
    [("sequence", 0), ("run_id", "other"), ("version", 2), ("unexpected", "private")],
)
def test_golden_rejects_envelope_mutations(position, field, value):
    events = golden()
    validator = StreamValidator("session", "run", "turn")
    for event in events[:position]:
        validator.accept(event)
    events[position][field] = value
    with pytest.raises(ValueError, match=ERROR):
        validator.accept(events[position])


@pytest.mark.parametrize("position", range(19))
def test_golden_rejects_replayed_event(position):
    events = golden()
    validator = StreamValidator("session", "run", "turn")
    for event in events[: position + 1]:
        validator.accept(deepcopy(event))
    duplicate = deepcopy(events[position])
    with pytest.raises(ValueError, match=ERROR):
        validator.accept(duplicate)


def test_wrong_identity_and_pending_session_isolation(config):
    events = stream(config, REQUEST, executor=Executor())
    next(events)
    events.close()
    assert control(
        config, dict(command="orchestration_pending", session_id="other")
    ) == {"pending": None}
    for update in (
        {"turn_id": "other"},
        {"session_id": "other"},
        {"message": "Changed"},
    ):
        assert collect(config, dict(REQUEST, **update))[-1]["event"] == "error"
    with pytest.raises(ValueError, match=ERROR):
        control(config, dict(REQUEST, command="orchestration_discard", turn_id="other"))
    assert (
        collect(config, dict(REQUEST, run_id="other", turn_id="other"))[-1]["event"]
        == "error"
    )
    assert (
        collect(config, dict(REQUEST, command="orchestration_resume"))[-1]["event"]
        == "done"
    )


def test_published_exchange_wins_discard_and_lost_ack(config):
    from src.orchestration_publication import CanonicalPublisher

    class LostAck(CanonicalPublisher):
        def publish(self, *args):
            super().publish(*args)
            raise RuntimeError("Synthetic acknowledgement loss")

    events = collect(config, publisher_factory=LostAck)
    assert events[-1]["event"] == "error"
    discarded = control(config, dict(REQUEST, command="orchestration_discard"))
    assert discarded["snapshot"]["snapshot"]["status"] == "completed"
    assert discarded == control(config, dict(REQUEST, command="orchestration_discard"))
    resumed = collect(config, dict(REQUEST, command="orchestration_resume"))
    assert [e["event"] for e in resumed] == ["snapshot", "done"]
    with SQLiteHistoryStore(config.database_path) as history:
        assert len(history.get_messages("session")) == 2


def test_missing_and_deleted_trace_are_sanitized(config):
    with pytest.raises(ValueError, match=ERROR):
        control(
            config,
            dict(command="orchestration_trace", session_id="session", exchange_id=1),
        )
    events = collect(config)
    with pytest.raises(ValueError, match=ERROR):
        control(
            config,
            dict(
                command="orchestration_trace",
                session_id="other",
                exchange_id=events[-1]["exchange_id"],
            ),
        )
    with sqlite3.connect(config.database_path) as db:
        db.execute("PRAGMA foreign_keys=ON")
        db.execute("DELETE FROM exchanges WHERE id = ?", (events[-1]["exchange_id"],))
    with pytest.raises(ValueError, match=ERROR):
        control(
            config,
            dict(
                command="orchestration_trace",
                session_id="session",
                exchange_id=events[-1]["exchange_id"],
            ),
        )
    assert collect(config)[-1]["event"] == "error"


@pytest.mark.parametrize("version", [1, 2])
def test_exact_old_store_upgrade_preserves_journal(config, version):
    from src.orchestration_store import DATABASE_SCHEMA_VERSION

    path = config.database_path.parent / "chat_orchestration.sqlite3"
    with SQLiteOrchestrationStore(path) as store:
        checkpoint = store.create_run("session", GRAPH_VERSION, run_id="run")
        journal = store.events("run", "session", GRAPH_VERSION)
    with sqlite3.connect(path) as db:
        db.execute("DROP TABLE source_display")
        db.execute("DROP TABLE adapter_requests")
        if version == 1:
            db.execute("DROP TABLE noop_receipts")
        db.execute(f"PRAGMA user_version = {version}")
    with SQLiteOrchestrationStore(path) as store:
        assert store.load("run", "session", GRAPH_VERSION) == checkpoint
        assert store.events("run", "session", GRAPH_VERSION) == journal
    with sqlite3.connect(path) as db:
        assert (
            db.execute("PRAGMA user_version").fetchone()[0] == DATABASE_SCHEMA_VERSION
        )


def test_sources_keep_public_display_fields_and_canonical_order(config):
    from src.cli.orchestration_workers import DisplayRetriever
    from src.paper_search import PaperChunkMatch

    class EvidenceModel(Model):
        def invoke(self, messages):
            if messages[0].content.startswith("QUESTION-HANDLER:"):
                evidence = json.loads(messages[-1].content)["evidence"]
                return json.dumps(
                    dict(
                        answer="Supported answer.",
                        evidence_ids=[e["evidence_id"] for e in evidence],
                        followups=[],
                    )
                )
            return super().invoke(messages)

    class Retriever:
        def retrieve_chunks(self, query, n_results=5):
            return [
                PaperChunkMatch(
                    1, "Evidence text", 0.9, "paper", 2, "paper.pdf", "Paper title", 3
                )
            ]

    with SQLiteOrchestrationStore(
        config.database_path.parent / "chat_orchestration.sqlite3"
    ) as store:
        retriever = DisplayRetriever(
            store, ("run", "session", GRAPH_VERSION), Retriever()
        )
        events = collect(
            config, executor=Executor(EvidenceModel()), retriever=retriever
        )
    assert events[-1]["event"] == "done", events
    source = dict(
        rank=1,
        paper_id="paper",
        chunk_index=2,
        filename="paper.pdf",
        title="Paper title",
        page=3,
        score=0.9,
        snippet="Evidence text",
    )
    assert events[-1]["sources"] == [source]
    assert (
        next(e for e in events if e["event"] == "evidence")["evidence"][0]["source"]
        == source
    )
    assert collect(config)[-1]["sources"] == [source]
    with SQLiteHistoryStore(config.database_path) as history:
        assert history.get_messages("session")[-1].sources[0].title == "Paper title"


def _fake_worker(pipe, config, dispatch):
    import os

    root = Path.cwd().resolve()

    def deny(event, args):
        if event in ("socket.connect", "socket.bind", "socket.getaddrinfo"):
            raise RuntimeError("Network denied in synthetic worker")
        if event in ("open", "sqlite3.connect") and isinstance(
            args[0], (str, bytes, os.PathLike)
        ):
            path = Path(os.fsdecode(args[0])).resolve()
            if path == root / ".env" or any(
                path.is_relative_to(root / p) for p in ("data", "ResearchPapers")
            ):
                raise RuntimeError("User data denied in synthetic worker")

    sys.addaudithook(deny)
    from src.cli import models, orchestration_workers as workers

    workers.production_model = lambda config: Model()
    models.build_paper_search_index = lambda config: None
    workers._worker(pipe, config, dispatch)


def test_owned_process_workers_are_joined_on_success_and_close(config, monkeypatch):
    import multiprocessing
    import pickle
    from src.cli import orchestration_workers as workers

    monkeypatch.setattr(workers, "_worker", _fake_worker)

    class Executor(workers.ProcessExecutor):
        def submit(self, operation_id, dispatch):
            pickle.dumps(dispatch)
            return super().submit(operation_id, dispatch)

    before = set(multiprocessing.active_children())
    result = collect(config, executor=Executor(config))
    assert result[-1]["event"] == "done", result
    assert set(multiprocessing.active_children()) == before
    config.database_path = (
        config.database_path.parent / "another" / "chat_history.sqlite3"
    )
    events = stream(config, REQUEST, executor=Executor(config))
    for event in events:
        if event["event"] == "agent_started":
            break
    events.close()
    assert set(multiprocessing.active_children()) == before


@pytest.mark.parametrize("winner", ["publish", "discard"])
def test_canonical_publication_arbitration(config, winner):
    from concurrent.futures import ThreadPoolExecutor
    from src.orchestration_publication import CanonicalPublisher
    from src.orchestration_request import RunRequest
    from src.orchestration_runtime import FinalAnswer, Discarded, Published
    from src.orchestration_tools import digest

    request = RunRequest("turn", "Question", 0, digest([]))
    publisher = CanonicalPublisher(
        config.database_path, ("run", "session", GRAPH_VERSION), request, lambda ids: []
    )
    answer = FinalAnswer("Answer")
    first = (
        publisher.publish(publisher.key, answer)
        if winner == "publish"
        else publisher.discard(publisher.key)
    )

    def compete(index):
        if index % 2:
            return publisher.discard(publisher.key)
        try:
            return publisher.publish(publisher.key, answer)
        except ValueError:
            return publisher.lookup(publisher.key)

    with ThreadPoolExecutor(max_workers=8) as pool:
        results = list(pool.map(compete, range(24)))
    assert all(type(r) is type(first) for r in results)
    assert isinstance(first, Published if winner == "publish" else Discarded)
    with SQLiteHistoryStore(config.database_path) as history:
        assert len(history.get_messages("session")) == (2 if winner == "publish" else 0)
