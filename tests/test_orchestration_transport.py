"""Offline contract/recovery tests; every database belongs to pytest tmp_path."""

import json
from pathlib import Path
import sys
from types import SimpleNamespace

import pytest

from src.cli.orchestration import GRAPH_VERSION, control, stream
from src.orchestration.runtime import IteratorHandle, specialist_steps
from src.orchestration.store import SQLiteOrchestrationStore
from src.orchestration.wire import ERROR, StreamValidator, loads
from src.storage.sqlite_history import SQLiteHistoryStore

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
            return json.dumps(
                {"parts": [{"part": "Retrieval", "question": "What is retrieval?"}]}
            )
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


@pytest.mark.parametrize("failures", [1, 2])
@pytest.mark.parametrize(
    "boundary", [None, "question_failed", "agent_retried", "question_answered"]
)
def test_question_retry_resume_and_completed_replay(config, failures, boundary):
    class FailingModel(Model):
        remaining = failures

        def invoke(self, messages):
            if messages[0].content.startswith("QUESTION-HANDLER:") and self.remaining:
                self.remaining -= 1
                raise RuntimeError("Synthetic handler failure")
            return super().invoke(messages)

    executor = Executor(FailingModel())
    iterator = stream(config, REQUEST, executor=executor)
    first = []
    try:
        for event in iterator:
            first.append(event)
            if event["event"] == boundary:
                break
    finally:
        iterator.close()
    validator = StreamValidator("session", "run", "turn")
    for event in first:
        validator.accept(loads(json.dumps(event)))
    assert any(e["event"] == "question_failed" for e in first)
    resumed = collect(
        config, dict(REQUEST, command="orchestration_resume"), executor=executor
    )
    assert resumed[-1]["event"] == "done", resumed
    validator = StreamValidator("session", "run", "turn")
    for event in resumed:
        validator.accept(loads(json.dumps(event)))
    replay = collect(config)
    assert [e["event"] for e in replay] == ["snapshot", "done"]
    assert (
        replay[0]["snapshot"]["exchange_id"]
        == replay[-1]["exchange_id"]
        == resumed[-1]["exchange_id"]
    )
    with SQLiteHistoryStore(config.database_path) as history:
        assert len(history.get_messages("session")) == 2


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
    from src.orchestration.publication import CanonicalPublisher

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


def test_sources_keep_public_display_fields_and_canonical_order(config):
    from src.cli.orchestration_workers import DisplayRetriever
    from src.retrieval.paper_search import PaperChunkMatch

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
    from src.orchestration.publication import CanonicalPublisher
    from src.orchestration.request import RunRequest
    from src.orchestration.runtime import FinalAnswer, Discarded, Published
    from src.orchestration.tools import digest

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
