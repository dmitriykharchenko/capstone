"""Offline, disposable-database durability and closed-schema regression tests."""

import hashlib
import json
import os
import sqlite3
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from dataclasses import replace
from pathlib import Path

import pytest

from src.orchestration_state import (
    MAX_CHECKPOINT_BYTES,
    AgentKind,
    AgentState,
    Checkpoint,
    ErrorCode,
    EventKind,
    EvidenceState,
    OrchestrationEvent,
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
from src.orchestration_store import (
    APPLICATION_ID,
    CorruptCheckpointError,
    GraphVersionError,
    OrchestrationStoreError,
    RunNotFoundError,
    SchemaVersionError,
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


def child(tmp_path, code, *args):
    # Do not inherit credentials, tracing, provider configuration or dotenv.
    return subprocess.run(
        [sys.executable, "-B", "-c", code, *map(str, args)],
        cwd=Path(__file__).resolve().parents[1],
        env={
            "PATH": os.defpath,
            "HOME": str(tmp_path),
            "TMPDIR": str(tmp_path),
            "PYTHONHASHSEED": "0",
            "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1",
            "ANONYMIZED_TELEMETRY": "False",
        },
        capture_output=True,
        text=True,
        timeout=20,
    )


def test_schema_initialization_is_idempotent_and_separate(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    with SQLiteOrchestrationStore() as first:
        snapshot = first.create_run("session", "graph-v1", run_id="run")
        path = first.database_path
    before = path.read_bytes()
    with SQLiteOrchestrationStore() as reopened:
        assert reopened.load(*ARGS) == snapshot
        assert reopened.create_run("session", "graph-v1", run_id="run") == snapshot
    assert path.read_bytes() == before
    assert path == tmp_path / "data/chat_orchestration.sqlite3"
    assert not (tmp_path / "data/chat_history.sqlite3").exists()
    assert scalar(path, "PRAGMA user_version") == 1
    assert scalar(path, "PRAGMA application_id") == APPLICATION_ID


def test_history_is_untouched_and_wrong_database_rejected(tmp_path):
    # A disposable canonical-shaped fixture, never the user's history database.
    path = tmp_path / "chat_history.sqlite3"
    execute(path, "CREATE TABLE messages (id INTEGER PRIMARY KEY, content TEXT)")
    execute(path, "INSERT INTO messages VALUES (1, 'fixture')")
    before = path.read_bytes()
    with pytest.raises(StateValidationError):
        SQLiteOrchestrationStore(path)
    alias = tmp_path / "alias.db"
    alias.symlink_to(path)
    with pytest.raises(StateValidationError):
        SQLiteOrchestrationStore(alias)
    hardlink = tmp_path / "hardlink.db"
    os.link(path, hardlink)
    with pytest.raises(SchemaVersionError):
        SQLiteOrchestrationStore(hardlink)
    with SQLiteOrchestrationStore(tmp_path / "orchestration.db") as other:
        other.create_run("session", "graph-v1", run_id="run")
        other.discard(*ARGS, operation_id="discard")
    assert path.read_bytes() == before


@pytest.mark.parametrize(
    "mutation",
    [
        "PRAGMA user_version = 0",
        "PRAGMA user_version = 2",
        "PRAGMA application_id = 0",
        "ALTER TABLE runs ADD COLUMN unexpected TEXT",
        "DROP INDEX runs_session",
        "CREATE TABLE unrelated (id INTEGER)",
        "CREATE TRIGGER unexpected AFTER INSERT ON runs BEGIN SELECT 1; END",
    ],
)
def test_unknown_schema_is_rejected_without_migration(store, mutation):
    execute(store.database_path, mutation)
    before = store.database_path.read_bytes()
    with pytest.raises(SchemaVersionError):
        SQLiteOrchestrationStore(store.database_path)
    with pytest.raises(SchemaVersionError):
        store.load(*ARGS)
    assert store.database_path.read_bytes() == before


def test_initialization_rolls_back_on_error(tmp_path, monkeypatch):
    import src.orchestration_store as module

    path = tmp_path / "new.db"
    original = module._SCHEMA
    monkeypatch.setattr(module, "_SCHEMA", original + ("INVALID SQL",))
    with pytest.raises(OrchestrationStoreError):
        SQLiteOrchestrationStore(path)
    assert scalar(path, "PRAGMA user_version") == 0
    assert scalar(path, "SELECT count(*) FROM sqlite_master") == 0
    monkeypatch.setattr(module, "_SCHEMA", original)
    with SQLiteOrchestrationStore(path) as reopened:
        assert reopened.create_run("session", "graph-v1").sequence == 1


def test_runs_sessions_graph_versions_and_pending_are_isolated(store):
    running(store)
    second = store.create_run("other-session", "graph-v1", run_id="other")
    third = store.create_run("session", "graph-v1", run_id="third")
    assert {s.run.run_id for s in store.list_pending("session", "graph-v1")} == {
        "run",
        "third",
    }
    assert store.list_pending("other-session", "graph-v1") == (second,)
    with pytest.raises(RunNotFoundError):
        store.load("run", "other-session", "graph-v1")
    with pytest.raises(RunNotFoundError):
        store.create_run("other-session", "graph-v1", run_id="run")
    with pytest.raises(GraphVersionError):
        store.resume("run", "session", "graph-v2", operation_id="resume")
    with pytest.raises(GraphVersionError):
        store.list_pending("session", "graph-v2")
    assert store.load("third", "session", "graph-v1") == third


def test_atomic_tool_receipt_checkpoint_and_replay(store):
    candidate, committed = retrieved(store)
    assert committed.sequence == 4
    assert store.commit(candidate, operation_id="retrieve") == committed
    with pytest.raises(TransitionConflictError):
        store.commit(
            replace(
                candidate, evidence=(replace(candidate.evidence[0], snippet="Changed"),)
            ),
            operation_id="retrieve",
        )
    events = store.events(*ARGS)
    assert [event.sequence for event in events] == [1, 2, 3, 4]
    assert events[-1].kind is EventKind.TOOL_COMMITTED
    assert events[-1].tool_call_id == "retrieve"
    assert events[-1].evidence_ids == ("e",)
    assert store.events(*ARGS, after=1, limit=2) == events[1:3]
    for sequence in range(1, 5):
        assert store.load(*ARGS, sequence=sequence).sequence == sequence
    with pytest.raises(StateValidationError):
        replace(committed, tool_results=committed.tool_results * 2)


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


def test_exchange_can_be_attached_after_completion_and_is_unique(store):
    store.complete(*ARGS, operation_id="complete")
    completed = store.attach_exchange(*ARGS, 7, operation_id="attach")
    assert completed.run.status is RunStatus.COMPLETED
    store.create_run("session", "graph-v1", run_id="second")
    with pytest.raises(TransitionConflictError):
        store.complete(
            "second", "session", "graph-v1", exchange_id=7, operation_id="complete"
        )
    assert store.load("second", "session", "graph-v1").sequence == 1


@pytest.mark.parametrize("method", ["complete", "attach_exchange"])
def test_final_publication_rejects_unsettled_work(store, method):
    before = running(store)
    kwargs = {"operation_id": "publish", "exchange_id": 7}
    with pytest.raises(TransitionConflictError):
        getattr(store, method)(*ARGS, **kwargs)
    assert store.load(*ARGS) == before


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


def test_resume_requeues_only_interrupted_work_and_fences_attempts(store):
    candidate, old = retrieved(store)
    interrupted = store.interrupt(*ARGS, operation_id="interrupt")
    assert interrupted.agents == old.agents
    with SQLiteOrchestrationStore(store.database_path) as reopened:
        assert reopened.load(*ARGS) == interrupted  # No automatic resume on open.
        resumed = reopened.resume(*ARGS, operation_id="resume")
        assert resumed.agents[0].status is WorkStatus.QUEUED
        assert resumed.agents[0].attempt == 1
        assert resumed.agents[0].retry_count == 0
        assert resumed.run.retry_count == 1
        assert resumed.questions[0].status is WorkStatus.QUEUED
        assert resumed.evidence == old.evidence
        assert resumed.tool_results == old.tool_results
        assert reopened.resume(*ARGS, operation_id="resume-again") == resumed
        assert reopened.commit(candidate, operation_id="retrieve") == old
        assert reopened.load(*ARGS) == resumed
        with pytest.raises(TransitionConflictError):
            reopened.commit(old, operation_id="stale-result")
        started = reopened.commit(
            replace(
                resumed, agents=(replace(resumed.agents[0], status=WorkStatus.RUNNING),)
            ),
            operation_id="start-again",
        )
        late_tool = ToolResult("late", "a", 0, ToolName.RETRIEVE_CONTEXT)
        with pytest.raises(TransitionConflictError):
            reopened.commit(
                replace(started, tool_results=started.tool_results + (late_tool,)),
                operation_id="late-tool",
            )


def test_resume_preserves_completed_agents_and_requeues_waiting_finalizer(store):
    old = answered(store)
    finalizer = AgentState("final", AgentKind.FINALIZER, parent_id="a", depth=1)
    queued_snapshot = store.commit(
        replace(old, agents=old.agents + (finalizer,)), operation_id="queue-final"
    )
    started = store.commit(
        replace(
            queued_snapshot,
            agents=(old.agents[0], replace(finalizer, status=WorkStatus.RUNNING)),
        ),
        operation_id="start-final",
    )
    waiting = store.commit(
        replace(
            started,
            agents=(old.agents[0], replace(finalizer, status=WorkStatus.WAITING)),
        ),
        operation_id="wait-final",
    )
    resumed = store.resume(*ARGS, operation_id="resume")
    assert resumed.agents[0] == old.agents[0]
    assert resumed.questions == old.questions
    assert resumed.agents[1].status is WorkStatus.QUEUED
    assert resumed.agents[1].attempt == waiting.agents[1].attempt + 1


def test_retry_is_bounded_and_increments_attempt_only_with_failed_work(store):
    old = running(store)
    failed = store.commit(
        replace(
            old,
            questions=(replace(old.questions[0], status=WorkStatus.FAILED),),
            agents=(
                replace(
                    old.agents[0],
                    status=WorkStatus.FAILED,
                    error_code=ErrorCode.AGENT_FAILED,
                ),
            ),
        ),
        operation_id="failure",
    )
    retry_agent = replace(
        failed.agents[0],
        status=WorkStatus.QUEUED,
        retry_count=1,
        attempt=1,
        error_code=None,
    )
    retried = store.commit(
        replace(
            failed,
            agents=(retry_agent,),
            questions=(replace(failed.questions[0], status=WorkStatus.QUEUED),),
        ),
        operation_id="retry",
    )
    assert retried.agents[0].retry_count == 1
    with pytest.raises(StateValidationError):
        replace(retry_agent, retry_count=2)
    with pytest.raises(TransitionConflictError):
        store.commit(
            replace(retried, agents=(replace(retry_agent, attempt=2),)),
            operation_id="invalid-attempt",
        )


@pytest.mark.parametrize(
    "mutation",
    [
        lambda s: replace(s, questions=()),
        lambda s: replace(s, questions=(replace(s.questions[0], text="Rewrite"),)),
        lambda s: replace(
            s, agents=(replace(s.agents[0], status=WorkStatus.COMPLETED),)
        ),
        lambda s: replace(s, agents=(replace(s.agents[0], retry_count=1),)),
        lambda s: replace(s, run=replace(s.run, retry_count=1)),
    ],
)
def test_invalid_transitions_are_rejected_atomically(store, mutation):
    old = queued(store)
    with pytest.raises((StateValidationError, TransitionConflictError)):
        store.commit(mutation(old), operation_id="bad")
    assert store.load(*ARGS) == old


@pytest.mark.parametrize("field", ["questions", "agents", "evidence", "tool_results"])
def test_committed_terminal_and_evidence_records_are_immutable(store, field):
    old = answered(store)
    with pytest.raises((StateValidationError, TransitionConflictError)):
        store.commit(replace(old, **{field: ()}), operation_id="erase")
    assert store.load(*ARGS) == old


def test_add_question_receipt_commits_parent_metadata(store):
    old = running(store)
    followup = QuestionState("child", "Follow-up?", parent_id="q", depth=1)
    receipt = ToolResult("add", "a", 0, ToolName.ADD_QUESTION, "child")
    new = store.commit(
        replace(old, questions=old.questions + (followup,), tool_results=(receipt,)),
        operation_id="add",
    )
    assert new.questions[-1] == followup
    assert new.tool_results == (receipt,)
    with pytest.raises(TransitionConflictError):
        store.commit(
            replace(
                new,
                tool_results=new.tool_results
                + (replace(receipt, tool_call_id="duplicate"),),
            ),
            operation_id="duplicate",
        )


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


def test_aggregate_json_byte_budgets_are_enforced(store):
    snapshot = queued(store)
    evidence = tuple(
        EvidenceState(f"e{i}", "q", "p", i, "\x01" * 4096, 1.0) for i in range(128)
    )
    with pytest.raises(StateValidationError, match="byte budget"):
        store.commit(replace(snapshot, evidence=evidence), operation_id="too-big")
    assert store.load(*ARGS) == snapshot
    event = OrchestrationEvent(
        "run",
        "session",
        1,
        "op",
        EventKind.TRANSITION,
        RunStatus.RUNNING,
        evidence_ids=tuple(f"{i:03d}" + "a" * 125 for i in range(128)),
    )
    with pytest.raises(StateValidationError, match="byte budget"):
        encode(event)


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


@pytest.mark.parametrize(
    "value", ["{}", "[]", "null", "{" * 2000, "[NaN]", '{"sequence":1,"sequence":1}']
)
def test_decoder_rejects_malformed_payloads(value):
    with pytest.raises(StateValidationError):
        decode(value)


@pytest.mark.parametrize("table", ["events", "checkpoints"])
@pytest.mark.parametrize(
    "payload", ["{}", "not json", "[NaN]", "x" * (MAX_CHECKPOINT_BYTES + 1)]
)
def test_corrupt_or_oversized_payloads_fail_closed(store, table, payload):
    execute(
        store.database_path,
        f"UPDATE {table} SET payload = ?, payload_hash = ?",
        (payload, hashlib.sha256(payload.encode()).hexdigest()),
    )
    with pytest.raises(CorruptCheckpointError):
        store.load(*ARGS)
    with pytest.raises(CorruptCheckpointError):
        store.resume(*ARGS, operation_id="resume")


@pytest.mark.parametrize(
    "mutation",
    [
        "UPDATE checkpoints SET payload_hash = 'invalid'",
        "UPDATE runs SET sequence = 2",
        "UPDATE runs SET status = 'completed'",
        "UPDATE runs SET exchange_id = 2",
        "DELETE FROM checkpoints",
        "DELETE FROM events",
    ],
)
def test_journal_corruption_is_not_silently_repaired(store, mutation):
    execute(store.database_path, mutation)
    before = store.database_path.read_bytes()
    with pytest.raises(CorruptCheckpointError):
        store.load(*ARGS)
    assert store.database_path.read_bytes() == before


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


def test_multiple_writers_initialize_and_commit_monotonic_sequences(tmp_path):
    path = tmp_path / "concurrent.db"

    def writer(worker):
        with SQLiteOrchestrationStore(path) as connection:
            connection.create_run("session", "graph-v1", run_id="run")
            for step in range(6):
                for _ in range(100):
                    snapshot = connection.load(*ARGS)
                    try:
                        committed = connection.commit(
                            snapshot, operation_id=f"worker-{worker}-{step}"
                        )
                        assert (
                            connection.commit(
                                snapshot, operation_id=f"worker-{worker}-{step}"
                            )
                            == committed
                        )
                        break
                    except TransitionConflictError:
                        continue
                else:
                    pytest.fail("Writer did not make progress")

    with ThreadPoolExecutor(max_workers=6) as pool:
        list(pool.map(writer, range(6)))
    with SQLiteOrchestrationStore(path) as reopened:
        assert reopened.load(*ARGS).sequence == 37
        assert [event.sequence for event in reopened.events(*ARGS)] == list(
            range(1, 38)
        )
        for sequence in range(1, 38):
            assert reopened.load(*ARGS, sequence=sequence).sequence == sequence


@pytest.mark.parametrize(
    "crash_point",
    ["UPDATE runs", "INSERT INTO events", "INSERT INTO checkpoints", "commit"],
)
def test_process_crash_never_exposes_partial_transition(store, tmp_path, crash_point):
    before = running(store)
    script = """
import os, sqlite3, sys
from dataclasses import replace
from src.orchestration_store import SQLiteOrchestrationStore
from src.orchestration_state import EvidenceState, ToolName, ToolResult
store = SQLiteOrchestrationStore(sys.argv[1])
old = store.load('run', 'session', 'graph-v1')
original = sqlite3.connect
class CrashConnection(sqlite3.Connection):
    def execute(self, sql, parameters=()):
        result = super().execute(sql, parameters)
        if sql.startswith(sys.argv[2]):
            os._exit(71)
        return result
    def commit(self):
        super().commit()
        if sys.argv[2] == 'commit':
            os._exit(72)
sqlite3.connect = lambda *args, **kwargs: original(*args, **kwargs, factory=CrashConnection)
candidate = replace(old, evidence=(EvidenceState('e', 'q', 'paper', 0, 'Evidence', 1.0),), tool_results=(ToolResult('tool', 'a', 0, ToolName.RETRIEVE_CONTEXT, 'q', ('e',)),))
store.commit(candidate, operation_id='crash-operation')
"""
    result = child(tmp_path, script, store.database_path, crash_point)
    assert result.returncode == (72 if crash_point == "commit" else 71), result.stderr
    with SQLiteOrchestrationStore(store.database_path) as reopened:
        after = reopened.load(*ARGS)
        if crash_point == "commit":
            assert after.sequence == before.sequence + 1
            assert after.tool_results[0].tool_call_id == "tool"
            assert len(after.evidence) == 1
            candidate = replace(
                before, evidence=after.evidence, tool_results=after.tool_results
            )
            assert reopened.commit(candidate, operation_id="crash-operation") == after
        else:
            assert after == before
        assert len(reopened.events(*ARGS)) == after.sequence
        assert scalar(store.database_path, "PRAGMA integrity_check") == "ok"


def test_fresh_process_resume_reads_committed_receipt_without_replaying(
    store, tmp_path
):
    _, before = retrieved(store)
    script = """
import sys
from src.orchestration_store import SQLiteOrchestrationStore
from src.orchestration_state import WorkStatus
with SQLiteOrchestrationStore(sys.argv[1]) as store:
    old = store.load('run', 'session', 'graph-v1')
    assert old.agents[0].status is WorkStatus.RUNNING
    resumed = store.resume('run', 'session', 'graph-v1', operation_id='fresh-resume')
    assert resumed.tool_results == old.tool_results
    assert resumed.evidence == old.evidence
    assert resumed.agents[0].status is WorkStatus.QUEUED
    assert resumed.agents[0].attempt == 1
"""
    result = child(tmp_path, script, store.database_path)
    assert result.returncode == 0, result.stderr
    assert store.load(*ARGS).sequence == before.sequence + 1
    assert [e.kind for e in store.events(*ARGS)].count(EventKind.TOOL_COMMITTED) == 1


@pytest.mark.parametrize(
    "stage",
    [
        "created",
        "queued",
        "running",
        "retrieved",
        "answered",
        "attached",
        "completed",
        "failed",
        "discarded",
    ],
)
def test_restart_matrix_preserves_every_committed_stage(store, tmp_path, stage):
    if stage == "queued":
        queued(store)
    elif stage == "running":
        running(store)
    elif stage == "retrieved":
        retrieved(store)
    elif stage in ("answered", "attached", "completed"):
        answered(store)
        if stage == "attached":
            store.attach_exchange(*ARGS, 9, operation_id="attach")
        elif stage == "completed":
            store.complete(*ARGS, exchange_id=9, operation_id="complete")
    elif stage in ("failed", "discarded"):
        running(store)
        getattr(store, "fail" if stage == "failed" else "discard")(
            *ARGS, operation_id="stop"
        )
    expected = hashlib.sha256(encode(store.load(*ARGS)).encode()).hexdigest()
    script = """
import hashlib, sys
from src.orchestration_store import SQLiteOrchestrationStore
from src.orchestration_state import encode
with SQLiteOrchestrationStore(sys.argv[1]) as store:
    snapshot = store.load('run', 'session', 'graph-v1')
    assert hashlib.sha256(encode(snapshot).encode()).hexdigest() == sys.argv[2]
    assert len(store.events('run', 'session', 'graph-v1')) == snapshot.sequence
"""
    result = child(tmp_path, script, store.database_path, expected)
    assert result.returncode == 0, result.stderr


def test_concurrent_process_writers_share_replay_keys(store, tmp_path):
    script = """
import sys
from src.orchestration_store import SQLiteOrchestrationStore, TransitionConflictError
with SQLiteOrchestrationStore(sys.argv[1]) as store:
    for step in range(4):
        for retry in range(100):
            snapshot = store.load('run', 'session', 'graph-v1')
            try:
                store.commit(snapshot, operation_id='process-' + sys.argv[2] + '-' + str(step))
                break
            except TransitionConflictError:
                pass
        else:
            raise AssertionError('no progress')
"""
    with ThreadPoolExecutor(max_workers=4) as pool:
        results = list(
            pool.map(
                lambda worker: child(tmp_path, script, store.database_path, worker),
                range(4),
            )
        )
    assert all(result.returncode == 0 for result in results)
    assert store.load(*ARGS).sequence == 17
    assert [e.sequence for e in store.events(*ARGS)] == list(range(1, 18))


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


@pytest.mark.parametrize(
    "effect", ["evidence", "answer", "wrong-receipt", "wrong-evidence"]
)
def test_tool_effects_require_matching_atomic_receipts(store, effect):
    old = running(store)
    evidence = EvidenceState("e", "q", "paper", 0, "Snippet", 1.0)
    answer = replace(old.questions[0], status=WorkStatus.COMPLETED, answer="Answer")
    if effect == "evidence":
        candidate = replace(old, evidence=(evidence,))
    elif effect == "answer":
        candidate = replace(old, questions=(answer,))
    elif effect == "wrong-receipt":
        candidate = replace(
            old,
            questions=(answer,),
            tool_results=(ToolResult("tool", "a", 0, ToolName.RETRIEVE_CONTEXT),),
        )
    else:
        candidate = replace(
            old,
            evidence=(evidence,),
            tool_results=(ToolResult("tool", "a", 0, ToolName.RETRIEVE_CONTEXT),),
        )
    with pytest.raises(TransitionConflictError):
        store.commit(candidate, operation_id="bad-effect")
    assert store.load(*ARGS) == old


def test_maximum_depth_and_parent_cycles_are_rejected():
    run = RunState("r", "s", "v1")
    root = QuestionState("q", "Root")
    child_question = QuestionState("c", "Child", parent_id="q", depth=1)
    leaf = QuestionState("l", "Leaf", parent_id="c", depth=2)
    assert (
        len(Checkpoint(run, 1, questions=(root, child_question, leaf)).questions) == 3
    )
    with pytest.raises(StateValidationError):
        QuestionState("deep", "Too deep", parent_id="l", depth=3)
    with pytest.raises(StateValidationError):
        Checkpoint(
            run, 1, questions=(replace(root, parent_id="c", depth=2), child_question)
        )
    with pytest.raises(StateValidationError):
        EvidenceState("e", "q", "p", 0, "Snippet", 10**1000)


def test_raised_write_error_rolls_back_and_releases_connection(store, monkeypatch):
    old = running(store)
    original = store._write

    def failed_write(*args, **kwargs):
        original(*args, **kwargs)
        raise RuntimeError("Injected failure")

    with monkeypatch.context() as patch:
        patch.setattr(store, "_write", failed_write)
        with pytest.raises(RuntimeError, match="Injected failure"):
            store.commit(old, operation_id="failure")
    assert store.load(*ARGS) == old
    assert store.commit(old, operation_id="failure").sequence == old.sequence + 1


def test_same_operation_concurrent_replay_writes_exactly_once(store):
    snapshot = store.load(*ARGS)
    with ThreadPoolExecutor(max_workers=8) as pool:
        results = list(
            pool.map(lambda _: store.commit(snapshot, operation_id="same"), range(8))
        )
    assert all(result == results[0] for result in results)
    assert store.load(*ARGS).sequence == 2
