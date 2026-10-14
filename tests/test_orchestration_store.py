"""Offline, disposable-database durability and closed-schema regression tests."""

import json
import sqlite3
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from dataclasses import replace

import pytest

from src.orchestration.state import (
    AgentKind,
    AgentState,
    Checkpoint,
    EvidenceState,
    QuestionState,
    RunState,
    RunStatus,
    StateValidationError,
    ToolName,
    ToolResult,
    WorkStatus,
    decode,
    encode,
)
from src.orchestration.store import (
    DATABASE_SCHEMA_VERSION,
    CorruptCheckpointError,
    OrchestrationStoreError,
    RunNotFoundError,
    SQLiteOrchestrationStore,
    TransitionConflictError,
)

ARGS = ("run", "session", "graph-v1")


@pytest.fixture
def store(tmp_path):
    with SQLiteOrchestrationStore(tmp_path / "nested" / "orchestration.db") as result:
        result.create_run("session", "graph-v1", run_id="run")
        yield result


def queued(store):
    snapshot = store.load(*ARGS)
    return store.commit(
        replace(
            snapshot,
            questions=(QuestionState("q", "Public question?"),),
            agents=(AgentState("a", AgentKind.QUESTION, question_id="q"),),
        ),
        operation_id="queue",
    )


def running(store):
    snapshot = queued(store)
    return store.commit(
        replace(
            snapshot,
            questions=(replace(snapshot.questions[0], status=WorkStatus.RUNNING),),
            agents=(replace(snapshot.agents[0], status=WorkStatus.RUNNING),),
        ),
        operation_id="start",
    )


def retrieved(store):
    snapshot = running(store)
    tool = ToolResult("retrieve", "a", 0, ToolName.RETRIEVE_CONTEXT, "q", ("e",))
    candidate = replace(
        snapshot,
        evidence=(
            EvidenceState("e", "q", "paper", 0, "Compact public evidence", 0.9, 1),
        ),
        tool_results=(tool,),
    )
    return candidate, store.commit(candidate, operation_id="retrieve")


def answered(store):
    _, snapshot = retrieved(store)
    return store.commit(
        replace(
            snapshot,
            questions=(
                replace(
                    snapshot.questions[0],
                    status=WorkStatus.COMPLETED,
                    answer="Public answer",
                    evidence_ids=("e",),
                ),
            ),
            agents=(replace(snapshot.agents[0], status=WorkStatus.COMPLETED),),
            tool_results=snapshot.tool_results
            + (ToolResult("answer", "a", 0, ToolName.ANSWER_QUESTION, "q", ("e",)),),
        ),
        operation_id="answer",
    )


def execute(path, statement, params=()):
    with closing(sqlite3.connect(path)) as connection, connection:
        connection.execute(statement, params)


def scalar(path, statement):
    with closing(sqlite3.connect(path)) as connection:
        return connection.execute(statement).fetchone()[0]


def test_completed_trace_attachment_and_idempotent_terminal_operations(store):
    settled = answered(store)
    attached = store.attach_exchange(*ARGS, 42, operation_id="attach")
    assert attached.tool_results == settled.tool_results
    assert store.trace_for_exchange(42, "session", "graph-v1") == attached
    with pytest.raises(TransitionConflictError):
        store.resume(*ARGS, operation_id="resume")
    with pytest.raises(TransitionConflictError):
        store.commit(attached, operation_id="late")
    with pytest.raises(TransitionConflictError):
        store.discard(*ARGS, operation_id="discard")
    completed = store.complete(*ARGS, operation_id="complete")
    assert completed.run.status is RunStatus.COMPLETED
    assert store.complete(*ARGS, operation_id="complete-again") == completed
    assert store.attach_exchange(*ARGS, 42, operation_id="attach-again") == completed
    assert not store.list_pending("session", "graph-v1")
    with pytest.raises(TransitionConflictError):
        store.attach_exchange(*ARGS, 43, operation_id="other-exchange")
    with pytest.raises(RunNotFoundError):
        store.trace_for_exchange(42, "other-session", "graph-v1")
    with SQLiteOrchestrationStore(store.database_path) as reopened:
        assert reopened.trace_for_exchange(42, "session", "graph-v1") == completed
        assert len(reopened.events(*ARGS)) == completed.sequence


@pytest.mark.parametrize(
    "method,status", [("fail", RunStatus.FAILED), ("discard", RunStatus.DISCARDED)]
)
def test_failure_discard_stop_work_and_block_late_results(store, method, status):
    candidate, before = retrieved(store)
    terminal = getattr(store, method)(*ARGS, operation_id="stop")
    assert terminal.run.status is status
    assert terminal.agents[0].status is WorkStatus.CANCELLED
    assert terminal.questions[0].status is WorkStatus.CANCELLED
    assert terminal.evidence == before.evidence
    assert not store.list_pending("session", "graph-v1")
    assert getattr(store, method)(*ARGS, operation_id="stop") == terminal
    if method == "discard":
        assert store.discard(*ARGS, operation_id="discard-again") == terminal
    with pytest.raises(TransitionConflictError):
        store.resume(*ARGS, operation_id="late-resume")
    with pytest.raises(TransitionConflictError):
        store.commit(terminal, operation_id="late-result")
    assert store.commit(candidate, operation_id="retrieve") == before
    assert store.load(*ARGS) == terminal


@pytest.mark.parametrize("field", ["questions", "agents", "evidence", "tool_results"])
def test_committed_terminal_and_evidence_records_are_immutable(store, field):
    old = answered(store)
    with pytest.raises((StateValidationError, TransitionConflictError)):
        store.commit(replace(old, **{field: ()}), operation_id="erase")
    assert store.load(*ARGS) == old


@pytest.mark.parametrize(
    "factory",
    [
        lambda: RunState("", "session", "v1"),
        lambda: RunState("r", "s", "v1", exchange_id=True),
        lambda: RunState("r", "s", "v1", status="running"),
        lambda: QuestionState("q", "🙂" * 1025),
        lambda: QuestionState("q", "\ud800"),
        lambda: QuestionState("q", iter(["not text"])),
        lambda: QuestionState("q", "Question", answer="Uncommitted answer"),
        lambda: QuestionState("q", "Question", status=WorkStatus.COMPLETED),
        lambda: AgentState("a", AgentKind.QUESTION),
        lambda: EvidenceState("e", "q", "p", 0, "e", float("nan")),
        lambda: EvidenceState("e", "q", "p", True, "e", 1.0),
        lambda: ToolResult("t", "a", 0, "retrieve_context"),
        lambda: Checkpoint(RunState("r", "s", "v1"), 1, questions=[]),
        lambda: Checkpoint(RunState("r", "s", "v1"), True),
        lambda: Checkpoint(
            RunState("r", "s", "v1"),
            1,
            questions=(QuestionState("q", "Q", parent_id="missing", depth=1),),
        ),
        lambda: Checkpoint(
            RunState("r", "s", "v1"),
            1,
            questions=(QuestionState("q", "Q", parent_id="q", depth=1),),
        ),
        lambda: Checkpoint(
            RunState("r", "s", "v1"), 1, questions=(QuestionState("q", "Q", depth=1),)
        ),
        lambda: Checkpoint(
            RunState("r", "s", "v1"),
            1,
            questions=tuple(QuestionState(f"q{i}", "Q") for i in range(9)),
        ),
        lambda: Checkpoint(
            RunState("r", "s", "v1"),
            1,
            agents=tuple(
                AgentState(f"a{i}", AgentKind.INTAKE, status=WorkStatus.RUNNING)
                for i in range(5)
            ),
        ),
    ],
)
def test_closed_typed_models_reject_unsafe_invalid_or_unbounded_state(factory):
    with pytest.raises(StateValidationError):
        factory()


@pytest.mark.parametrize(
    "mutation",
    [
        lambda d: d.update(provider={"object": "not allowed"}),
        lambda d: d["run"].update(prompt="not allowed"),
        lambda d: d["run"].update(credentials="not allowed"),
        lambda d: d["run"].update(chain_of_thought="not allowed"),
        lambda d: d.update(schema_version=2),
        lambda d: d.update(sequence=True),
        lambda d: d.update(agents={}),
    ],
)
def test_decoder_rejects_unknown_fields_and_corrupt_types(store, mutation):
    data = json.loads(encode(store.load(*ARGS)))
    mutation(data)
    with pytest.raises(StateValidationError):
        decode(json.dumps(data))


def test_non_sqlite_database_and_closed_store_fail_safely(tmp_path, store):
    path = tmp_path / "invalid.db"
    path.write_bytes(b"not a database")
    with pytest.raises(OrchestrationStoreError, match="operation failed"):
        SQLiteOrchestrationStore(path)
    assert path.read_bytes() == b"not a database"
    store.close()
    store.close()
    with pytest.raises(OrchestrationStoreError, match="closed"):
        store.load(*ARGS)


@pytest.mark.parametrize(
    "mutation",
    [
        "UPDATE events SET operation_id = 'changed' WHERE sequence = 1",
        "UPDATE events SET request_hash = 'invalid' WHERE sequence = 1",
        "UPDATE events SET payload_hash = 'invalid' WHERE sequence = 1",
    ],
)
def test_historical_replay_metadata_cannot_bypass_validation(store, mutation):
    running(store)
    execute(store.database_path, mutation)
    with pytest.raises(CorruptCheckpointError):
        store.events(*ARGS)
    with pytest.raises(CorruptCheckpointError):
        store.create_run("session", "graph-v1", run_id="run")


def test_exact_v1_upgrade_preserves_journal_and_enables_noop_receipts(store):
    original = queued(store)
    events = store.events(*ARGS)
    execute(store.database_path, "DROP TABLE source_display")
    execute(store.database_path, "DROP TABLE adapter_requests")
    execute(store.database_path, "DROP TABLE noop_receipts")
    execute(store.database_path, "PRAGMA user_version = 1")
    store.close()

    def open_store(_):
        with SQLiteOrchestrationStore(store.database_path) as upgraded:
            assert upgraded.load(*ARGS) == original
            assert upgraded.events(*ARGS) == events
            assert upgraded.resume(*ARGS, operation_id="upgraded-noop") == original

    with ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(open_store, range(4)))
    assert scalar(store.database_path, "PRAGMA user_version") == DATABASE_SCHEMA_VERSION
    assert scalar(store.database_path, "SELECT count(*) FROM noop_receipts") == 1
    with SQLiteOrchestrationStore(store.database_path) as upgraded:
        latest = upgraded.interrupt(*ARGS, operation_id="later")
        assert upgraded.resume(*ARGS, operation_id="upgraded-noop") == original
        assert upgraded.load(*ARGS) == latest
