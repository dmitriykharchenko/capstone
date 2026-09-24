"""Separate SQLite trace/checkpoint store with atomic, replay-safe transitions.

This module never opens canonical history or publishes an exchange. The caller
owns that separate publication protocol and supplies an existing exchange ID.
Recovery must be explicitly requested after stopping the old worker. Each
operation owns a short-lived connection; instances can be used across threads.
"""

from __future__ import annotations

import hashlib
import sqlite3
from contextlib import contextmanager
from dataclasses import replace
from pathlib import Path
from uuid import uuid4

from .orchestration_state import (
    MAX_CHECKPOINT_BYTES,
    MAX_EVENT_BYTES,
    MAX_EVENTS,
    TERMINAL_RUNS,
    TERMINAL_WORK,
    AgentKind,
    Checkpoint,
    ErrorCode,
    EventKind,
    OrchestrationEvent,
    RunState,
    RunStatus,
    StateValidationError,
    ToolName,
    WorkStatus,
    decode,
    encode,
    identifier,
    integer,
)

DEFAULT_DATABASE_PATH = Path("data/chat_orchestration.sqlite3")
APPLICATION_ID = 0x4F524348
DATABASE_SCHEMA_VERSION = 2  # Checkpoint/event serialization remains version 1.
_SCHEMA = (
    """CREATE TABLE runs (
        run_id TEXT PRIMARY KEY NOT NULL,
        session_id TEXT NOT NULL,
        graph_version TEXT NOT NULL,
        sequence INTEGER NOT NULL CHECK (sequence >= 1),
        status TEXT NOT NULL,
        exchange_id INTEGER UNIQUE
    )""",
    """CREATE INDEX runs_session ON runs (session_id, status)""",
    """CREATE TABLE events (
        run_id TEXT NOT NULL REFERENCES runs(run_id),
        sequence INTEGER NOT NULL CHECK (sequence >= 1),
        operation_id TEXT NOT NULL,
        request_hash TEXT NOT NULL,
        payload TEXT NOT NULL,
        payload_hash TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        PRIMARY KEY (run_id, sequence),
        UNIQUE (run_id, operation_id)
    )""",
    """CREATE TABLE checkpoints (
        run_id TEXT NOT NULL,
        sequence INTEGER NOT NULL,
        payload TEXT NOT NULL,
        payload_hash TEXT NOT NULL,
        PRIMARY KEY (run_id, sequence),
        FOREIGN KEY (run_id, sequence) REFERENCES events(run_id, sequence)
    )""",
    """CREATE TABLE noop_receipts (
        run_id TEXT NOT NULL,
        operation_id TEXT NOT NULL,
        sequence INTEGER NOT NULL CHECK (sequence >= 1),
        request_hash TEXT NOT NULL,
        receipt_hash TEXT NOT NULL,
        PRIMARY KEY (run_id, operation_id),
        FOREIGN KEY (run_id, sequence) REFERENCES checkpoints(run_id, sequence)
    )""",
)


class OrchestrationStoreError(RuntimeError):
    """Sanitized storage error; no database contents in the message."""


class SchemaVersionError(OrchestrationStoreError):
    pass


class CorruptCheckpointError(OrchestrationStoreError):
    pass


class GraphVersionError(OrchestrationStoreError):
    pass


class RunNotFoundError(OrchestrationStoreError):
    pass


class TransitionConflictError(OrchestrationStoreError):
    pass


def _digest(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def _receipt_hash(run_id: str, operation_id: str, sequence: int, request_hash: str):
    return _digest(f"{run_id}\n{operation_id}\n{sequence}\n{request_hash}")


def _normalized(sql: str) -> str:
    return " ".join(sql.split())


def _layout(connection: sqlite3.Connection) -> set[str]:
    return {
        _normalized(row[0])
        for row in connection.execute(
            "SELECT sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'"
        )
        if row[0] is not None
    }


class SQLiteOrchestrationStore:
    """Application-owned schema v2; only the exact v1 layout is upgraded on open.

    ``commit`` accepts a replacement snapshot at its *current* sequence. It
    atomically compares that revision, validates the transition, increments the
    sequence, and writes an event/checkpoint pair. A stable operation_id must be
    retained across retries. Replaying identical input returns the original
    checkpoint; reusing the key for different input fails. Successful lifecycle
    no-ops reserve their IDs without advancing the event/checkpoint sequence.
    No work executes here.
    """

    def __init__(self, database_path: str | Path = DEFAULT_DATABASE_PATH) -> None:
        if not isinstance(database_path, (str, Path)) or not str(database_path).strip():
            raise StateValidationError("Invalid orchestration database path")
        if str(database_path) == ":memory:":
            raise StateValidationError("A durable database file is required")
        self.database_path = Path(database_path).resolve()
        if self.database_path.name.lower() == "chat_history.sqlite3":
            raise StateValidationError("Canonical history cannot hold orchestration")
        self._closed = False
        self.database_path.parent.mkdir(parents=True, exist_ok=True)
        with self._transaction(write=True, initialize=True):
            pass

    def close(self) -> None:
        self._closed = True

    def __enter__(self) -> SQLiteOrchestrationStore:
        if self._closed:
            raise OrchestrationStoreError("Store is closed")
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    @contextmanager
    def _transaction(self, *, write: bool = False, initialize: bool = False):
        if self._closed:
            raise OrchestrationStoreError("Store is closed")
        connection = None
        try:
            connection = sqlite3.connect(
                self.database_path, timeout=10, isolation_level=None
            )
            connection.row_factory = sqlite3.Row
            connection.execute("PRAGMA foreign_keys = ON")
            connection.execute("PRAGMA synchronous = FULL")
            connection.execute("BEGIN IMMEDIATE" if write else "BEGIN")
            version = connection.execute("PRAGMA user_version").fetchone()[0]
            application = connection.execute("PRAGMA application_id").fetchone()[0]
            layout = _layout(connection)
            if initialize and not layout and version == 0 and application == 0:
                for statement in _SCHEMA:
                    connection.execute(statement)
                connection.execute(f"PRAGMA user_version = {DATABASE_SCHEMA_VERSION}")
                connection.execute(f"PRAGMA application_id = {APPLICATION_ID}")
            elif (
                initialize
                and version == 1
                and application == APPLICATION_ID
                and layout == {_normalized(sql) for sql in _SCHEMA[:-1]}
            ):
                connection.execute(_SCHEMA[-1])
                connection.execute(f"PRAGMA user_version = {DATABASE_SCHEMA_VERSION}")
            elif (
                version != DATABASE_SCHEMA_VERSION
                or application != APPLICATION_ID
                or layout != {_normalized(sql) for sql in _SCHEMA}
            ):
                raise SchemaVersionError("Unrecognized orchestration database schema")
            yield connection
            connection.commit()
        except sqlite3.IntegrityError:
            raise TransitionConflictError("Conflicting orchestration record") from None
        except sqlite3.DatabaseError:
            raise OrchestrationStoreError(
                "Orchestration database operation failed"
            ) from None
        finally:
            if connection is not None:
                if connection.in_transaction:
                    connection.rollback()
                connection.close()

    def _payload(self, connection, table: str, run_id: str, sequence: int):
        limit = MAX_EVENT_BYTES if table == "events" else MAX_CHECKPOINT_BYTES
        metadata = ", operation_id, request_hash" if table == "events" else ""
        row = connection.execute(
            f"SELECT CASE WHEN length(CAST(payload AS BLOB)) <= ? THEN payload END AS payload, "
            f"payload_hash{metadata} FROM {table} WHERE run_id = ? AND sequence = ?",
            (limit, run_id, sequence),
        ).fetchone()
        try:
            if (
                row is None
                or row["payload"] is None
                or _digest(row["payload"]) != row["payload_hash"]
            ):
                raise StateValidationError("Invalid stored checksum")
            result = decode(row["payload"], event=table == "events")
            stored_run_id = result.run_id if table == "events" else result.run.run_id
            if result.sequence != sequence or stored_run_id != run_id:
                raise StateValidationError("Invalid stored identity")
            if table == "events" and (
                result.operation_id != row["operation_id"]
                or type(row["request_hash"]) is not str
                or len(row["request_hash"]) != 64
                or any(c not in "0123456789abcdef" for c in row["request_hash"])
            ):
                raise StateValidationError("Invalid replay metadata")
            return result
        except (StateValidationError, TypeError, AttributeError):
            raise CorruptCheckpointError(
                "Invalid orchestration checkpoint or event"
            ) from None

    def _load(
        self, connection, run_id: str, session_id: str, graph_version: str
    ) -> Checkpoint:
        for value in (run_id, session_id, graph_version):
            identifier(value)
        row = connection.execute(
            "SELECT * FROM runs WHERE run_id = ? AND session_id = ?",
            (run_id, session_id),
        ).fetchone()
        if row is None:
            raise RunNotFoundError("Orchestration run not found in this session")
        if row["graph_version"] != graph_version:
            raise GraphVersionError("Orchestration graph version is incompatible")
        sequence = row["sequence"]
        if type(sequence) is not int or not 1 <= sequence <= MAX_EVENTS:
            raise CorruptCheckpointError("Invalid run revision")
        for table in ("events", "checkpoints"):
            count, first, last = connection.execute(
                f"SELECT count(*), min(sequence), max(sequence) FROM {table} WHERE run_id = ?",
                (run_id,),
            ).fetchone()
            if (count, first, last) != (sequence, 1, sequence):
                raise CorruptCheckpointError("Incomplete orchestration journal")
        snapshot = self._payload(connection, "checkpoints", run_id, sequence)
        event = self._payload(connection, "events", run_id, sequence)
        if (
            snapshot.run.session_id != session_id
            or snapshot.run.graph_version != graph_version
            or snapshot.run.status.value != row["status"]
            or snapshot.run.exchange_id != row["exchange_id"]
            or event.session_id != session_id
            or event.status != snapshot.run.status
        ):
            raise CorruptCheckpointError("Inconsistent orchestration journal")
        return snapshot

    def load(
        self,
        run_id: str,
        session_id: str,
        graph_version: str,
        *,
        sequence: int | None = None,
    ) -> Checkpoint:
        with self._transaction() as connection:
            current = self._load(connection, run_id, session_id, graph_version)
            if sequence is None:
                return current
            integer(sequence, 1, current.sequence)
            snapshot = self._payload(connection, "checkpoints", run_id, sequence)
            if (
                snapshot.run.session_id != session_id
                or snapshot.run.graph_version != graph_version
            ):
                raise CorruptCheckpointError("Inconsistent historical checkpoint")
            return snapshot

    def list_pending(
        self, session_id: str, graph_version: str
    ) -> tuple[Checkpoint, ...]:
        identifier(session_id)
        identifier(graph_version)
        with self._transaction() as connection:
            rows = connection.execute(
                "SELECT run_id FROM runs WHERE session_id = ? AND status IN ('running', 'interrupted') ORDER BY run_id",
                (session_id,),
            ).fetchall()
            return tuple(
                self._load(connection, r[0], session_id, graph_version) for r in rows
            )

    def events(
        self,
        run_id: str,
        session_id: str,
        graph_version: str,
        *,
        after: int = 0,
        limit: int = 256,
    ) -> tuple[OrchestrationEvent, ...]:
        integer(after, 0, MAX_EVENTS)
        integer(limit, 1, MAX_EVENTS)
        with self._transaction() as connection:
            snapshot = self._load(connection, run_id, session_id, graph_version)
            result = tuple(
                self._payload(connection, "events", run_id, sequence)
                for sequence in range(
                    after + 1, min(snapshot.sequence, after + limit) + 1
                )
            )
            if any(event.session_id != session_id for event in result):
                raise CorruptCheckpointError("Inconsistent event session")
            return result

    def trace_for_exchange(
        self, exchange_id: int, session_id: str, graph_version: str
    ) -> Checkpoint:
        integer(exchange_id, 1, 2**63 - 1)
        identifier(session_id)
        with self._transaction() as connection:
            row = connection.execute(
                "SELECT run_id FROM runs WHERE exchange_id = ? AND session_id = ?",
                (exchange_id, session_id),
            ).fetchone()
            if row is None:
                raise RunNotFoundError("Orchestration trace not found in this session")
            return self._load(connection, row[0], session_id, graph_version)

    def _replay(
        self, connection, current: Checkpoint, operation_id: str, request_hash: str
    ):
        identifier(operation_id)
        rows = connection.execute(
            "SELECT sequence, request_hash, NULL AS receipt_hash FROM events WHERE run_id = ? AND operation_id = ? "
            "UNION ALL SELECT sequence, request_hash, receipt_hash FROM noop_receipts WHERE run_id = ? AND operation_id = ?",
            (current.run.run_id, operation_id) * 2,
        ).fetchall()
        if not rows:
            return None
        if len(rows) != 1:
            raise CorruptCheckpointError("Conflicting operation receipts")
        row = rows[0]
        if (
            type(row["sequence"]) is not int
            or not 1 <= row["sequence"] <= current.sequence
            or type(row["request_hash"]) is not str
            or len(row["request_hash"]) != 64
            or any(c not in "0123456789abcdef" for c in row["request_hash"])
            or (
                row["receipt_hash"] is not None
                and row["receipt_hash"]
                != _receipt_hash(
                    current.run.run_id,
                    operation_id,
                    row["sequence"],
                    row["request_hash"],
                )
            )
        ):
            raise CorruptCheckpointError("Invalid operation receipt")
        event = self._payload(connection, "events", current.run.run_id, row["sequence"])
        if event.session_id != current.run.session_id:
            raise CorruptCheckpointError("Inconsistent replay event")
        if row["request_hash"] != request_hash:
            raise TransitionConflictError(
                "Operation ID already committed with different input"
            )
        snapshot = self._payload(
            connection, "checkpoints", current.run.run_id, row["sequence"]
        )
        if (
            snapshot.run.session_id != current.run.session_id
            or snapshot.run.graph_version != current.run.graph_version
        ):
            raise CorruptCheckpointError("Inconsistent replay checkpoint")
        return snapshot

    def _reserve_noop(self, connection, snapshot, operation_id, request_hash):
        # Called under the same write transaction as _replay. Acknowledgement
        # occurs only after _transaction commits this bounded receipt.
        count = connection.execute(
            "SELECT count(*) FROM noop_receipts WHERE run_id = ?",
            (snapshot.run.run_id,),
        ).fetchone()[0]
        if count >= MAX_EVENTS:
            raise TransitionConflictError("No-op operation receipt limit reached")
        connection.execute(
            "INSERT INTO noop_receipts VALUES (?, ?, ?, ?, ?)",
            (
                snapshot.run.run_id,
                operation_id,
                snapshot.sequence,
                request_hash,
                _receipt_hash(
                    snapshot.run.run_id, operation_id, snapshot.sequence, request_hash
                ),
            ),
        )
        return snapshot

    def _write(
        self,
        connection,
        snapshot: Checkpoint,
        operation_id: str,
        request_hash: str,
        kind: EventKind,
        previous: Checkpoint | None = None,
    ) -> Checkpoint:
        identifier(operation_id)
        snapshot = replace(snapshot, sequence=snapshot.sequence + 1)
        payload = encode(snapshot)
        new_tools = snapshot.tool_results[
            len(previous.tool_results) if previous else 0 :
        ]
        event = OrchestrationEvent(
            snapshot.run.run_id,
            snapshot.run.session_id,
            snapshot.sequence,
            operation_id,
            kind,
            snapshot.run.status,
            tuple(
                q.question_id
                for q in snapshot.questions
                if previous is None or q not in previous.questions
            ),
            tuple(
                a.agent_id
                for a in snapshot.agents
                if previous is None or a not in previous.agents
            ),
            tuple(
                e.evidence_id
                for e in snapshot.evidence
                if previous is None or e not in previous.evidence
            ),
            new_tools[0].tool_call_id if new_tools else None,
        )
        event_payload = encode(event)
        run = snapshot.run
        if previous is None:
            connection.execute(
                "INSERT INTO runs VALUES (?, ?, ?, ?, ?, ?)",
                (
                    run.run_id,
                    run.session_id,
                    run.graph_version,
                    snapshot.sequence,
                    run.status.value,
                    run.exchange_id,
                ),
            )
        else:
            connection.execute(
                "UPDATE runs SET sequence = ?, status = ?, exchange_id = ? WHERE run_id = ?",
                (snapshot.sequence, run.status.value, run.exchange_id, run.run_id),
            )
        connection.execute(
            "INSERT INTO events (run_id, sequence, operation_id, request_hash, payload, payload_hash) VALUES (?, ?, ?, ?, ?, ?)",
            (
                run.run_id,
                snapshot.sequence,
                operation_id,
                request_hash,
                event_payload,
                _digest(event_payload),
            ),
        )
        connection.execute(
            "INSERT INTO checkpoints VALUES (?, ?, ?, ?)",
            (run.run_id, snapshot.sequence, payload, _digest(payload)),
        )
        return snapshot

    def create_run(
        self, session_id: str, graph_version: str, *, run_id: str | None = None
    ) -> Checkpoint:
        snapshot = Checkpoint(
            RunState(
                run_id if run_id is not None else str(uuid4()),
                session_id,
                graph_version,
            ),
            0,
        )
        request_hash = _digest(encode(snapshot))
        with self._transaction(write=True) as connection:
            if connection.execute(
                "SELECT 1 FROM runs WHERE run_id = ?", (snapshot.run.run_id,)
            ).fetchone():
                current = self._load(
                    connection, snapshot.run.run_id, session_id, graph_version
                )
                replay = self._replay(connection, current, "create", request_hash)
                if replay is None:
                    raise CorruptCheckpointError("Missing run creation receipt")
                return replay
            return self._write(
                connection, snapshot, "create", request_hash, EventKind.CREATED
            )

    def commit(self, snapshot: Checkpoint, *, operation_id: str) -> Checkpoint:
        """Commit typed work changes and at most one tool receipt atomically.

        Run fields may only change through the lifecycle methods. Records are
        append-only; completed work/evidence/receipts cannot be rewritten. Store
        the receipt in the same candidate as its effect, not in a later commit.
        """
        request_hash = _digest("commit:" + encode(snapshot))
        with self._transaction(write=True) as connection:
            run = snapshot.run
            old = self._load(connection, run.run_id, run.session_id, run.graph_version)
            replay = self._replay(connection, old, operation_id, request_hash)
            if replay is not None:
                return replay
            if snapshot.sequence != old.sequence:
                raise TransitionConflictError("Stale orchestration revision")
            if (
                old.run.status is not RunStatus.RUNNING
                or old.run.exchange_id is not None
            ):
                raise TransitionConflictError("Run no longer accepts work")
            _validate_transition(old, snapshot)
            kind = (
                EventKind.TOOL_COMMITTED
                if len(snapshot.tool_results) > len(old.tool_results)
                else EventKind.TRANSITION
            )
            return self._write(
                connection, snapshot, operation_id, request_hash, kind, old
            )

    def _control(
        self, run_id, session_id, graph_version, operation_id, kind, *, exchange_id=None
    ):
        identifier(operation_id)
        if exchange_id is not None:
            integer(exchange_id, 1, 2**63 - 1)
        request_hash = _digest(f"{kind.value}:{exchange_id}")
        with self._transaction(write=True) as connection:
            old = self._load(connection, run_id, session_id, graph_version)
            replay = self._replay(connection, old, operation_id, request_hash)
            if replay is not None:
                return replay
            run = old.run
            if kind is EventKind.DISCARDED and run.status is RunStatus.DISCARDED:
                return self._reserve_noop(connection, old, operation_id, request_hash)
            if kind is EventKind.EXCHANGE_ATTACHED and run.exchange_id == exchange_id:
                return self._reserve_noop(connection, old, operation_id, request_hash)
            if (
                kind is EventKind.COMPLETED
                and run.status is RunStatus.COMPLETED
                and exchange_id in (None, run.exchange_id)
            ):
                return self._reserve_noop(connection, old, operation_id, request_hash)
            if run.status in TERMINAL_RUNS and not (
                kind is EventKind.EXCHANGE_ATTACHED
                and run.status is RunStatus.COMPLETED
            ):
                raise TransitionConflictError("Terminal run cannot be changed")
            if exchange_id is not None and run.exchange_id not in (None, exchange_id):
                raise TransitionConflictError("Run already has a canonical exchange")
            candidate = old
            if kind is EventKind.RESUMED:
                if run.exchange_id is not None:
                    raise TransitionConflictError(
                        "Attached final exchange cannot be replayed"
                    )
                agents = tuple(
                    replace(
                        a,
                        status=WorkStatus.QUEUED,
                        attempt=a.attempt + 1,
                        error_code=None,
                    )
                    if a.status in (WorkStatus.RUNNING, WorkStatus.WAITING)
                    else a
                    for a in old.agents
                )
                questions = tuple(
                    replace(q, status=WorkStatus.QUEUED)
                    if q.status in (WorkStatus.RUNNING, WorkStatus.WAITING)
                    else q
                    for q in old.questions
                )
                if (
                    agents == old.agents
                    and questions == old.questions
                    and run.status is RunStatus.RUNNING
                ):
                    return self._reserve_noop(
                        connection, old, operation_id, request_hash
                    )
                candidate = replace(
                    old,
                    agents=agents,
                    questions=questions,
                    run=replace(
                        run,
                        status=RunStatus.RUNNING,
                        retry_count=run.retry_count + 1,
                        error_code=None,
                    ),
                )
            elif kind in (EventKind.FAILED, EventKind.DISCARDED):
                if run.exchange_id is not None:
                    raise TransitionConflictError(
                        "Attached final exchange must be retained"
                    )
                candidate = replace(
                    old,
                    agents=tuple(
                        replace(a, status=WorkStatus.CANCELLED)
                        if a.status not in TERMINAL_WORK
                        else a
                        for a in old.agents
                    ),
                    questions=tuple(
                        replace(q, status=WorkStatus.CANCELLED)
                        if q.status not in TERMINAL_WORK
                        else q
                        for q in old.questions
                    ),
                    run=replace(
                        run,
                        status=RunStatus.FAILED
                        if kind is EventKind.FAILED
                        else RunStatus.DISCARDED,
                        error_code=ErrorCode.RUN_FAILED
                        if kind is EventKind.FAILED
                        else ErrorCode.CANCELLED,
                    ),
                )
            elif kind in (EventKind.COMPLETED, EventKind.EXCHANGE_ATTACHED):
                if any(q.status not in TERMINAL_WORK for q in old.questions) or any(
                    a.status not in TERMINAL_WORK for a in old.agents
                ):
                    raise TransitionConflictError(
                        "Final publication requires settled work"
                    )
                candidate = replace(
                    old,
                    run=replace(
                        run,
                        status=RunStatus.COMPLETED
                        if kind is EventKind.COMPLETED
                        else run.status,
                        exchange_id=exchange_id
                        if exchange_id is not None
                        else run.exchange_id,
                        error_code=None,
                    ),
                )
            else:
                if run.exchange_id is not None:
                    raise TransitionConflictError(
                        "Attached final exchange cannot be interrupted"
                    )
                candidate = replace(
                    old,
                    run=replace(
                        run,
                        status=RunStatus.INTERRUPTED,
                        error_code=ErrorCode.INTERRUPTED,
                    ),
                )
            return self._write(
                connection, candidate, operation_id, request_hash, kind, old
            )

    def resume(
        self, run_id: str, session_id: str, graph_version: str, *, operation_id: str
    ) -> Checkpoint:
        """Explicit recovery only; fence old attempts and retain all tool receipts."""
        return self._control(
            run_id, session_id, graph_version, operation_id, EventKind.RESUMED
        )

    def interrupt(
        self, run_id: str, session_id: str, graph_version: str, *, operation_id: str
    ) -> Checkpoint:
        return self._control(
            run_id, session_id, graph_version, operation_id, EventKind.TRANSITION
        )

    def attach_exchange(
        self,
        run_id: str,
        session_id: str,
        graph_version: str,
        exchange_id: int,
        *,
        operation_id: str,
    ) -> Checkpoint:
        integer(exchange_id, 1, 2**63 - 1)
        return self._control(
            run_id,
            session_id,
            graph_version,
            operation_id,
            EventKind.EXCHANGE_ATTACHED,
            exchange_id=exchange_id,
        )

    def complete(
        self,
        run_id: str,
        session_id: str,
        graph_version: str,
        *,
        operation_id: str,
        exchange_id: int | None = None,
    ) -> Checkpoint:
        return self._control(
            run_id,
            session_id,
            graph_version,
            operation_id,
            EventKind.COMPLETED,
            exchange_id=exchange_id,
        )

    def fail(
        self, run_id: str, session_id: str, graph_version: str, *, operation_id: str
    ) -> Checkpoint:
        return self._control(
            run_id, session_id, graph_version, operation_id, EventKind.FAILED
        )

    def discard(
        self, run_id: str, session_id: str, graph_version: str, *, operation_id: str
    ) -> Checkpoint:
        """Retain a terminal tombstone/trace; never delete canonical history."""
        return self._control(
            run_id, session_id, graph_version, operation_id, EventKind.DISCARDED
        )


_WORK_TRANSITIONS = {
    WorkStatus.QUEUED: {WorkStatus.QUEUED, WorkStatus.RUNNING, WorkStatus.CANCELLED},
    WorkStatus.RUNNING: {
        WorkStatus.RUNNING,
        WorkStatus.WAITING,
        WorkStatus.COMPLETED,
        WorkStatus.FAILED,
        WorkStatus.CANCELLED,
    },
    WorkStatus.WAITING: {
        WorkStatus.WAITING,
        WorkStatus.RUNNING,
        WorkStatus.COMPLETED,
        WorkStatus.FAILED,
        WorkStatus.CANCELLED,
    },
    WorkStatus.FAILED: {WorkStatus.FAILED, WorkStatus.QUEUED},
    WorkStatus.COMPLETED: {WorkStatus.COMPLETED},
    WorkStatus.CANCELLED: {WorkStatus.CANCELLED},
}


def _validate_transition(old: Checkpoint, new: Checkpoint) -> None:
    if new.run != old.run:
        raise TransitionConflictError("Run fields require a lifecycle operation")
    for name in ("evidence", "tool_results"):
        before, after = getattr(old, name), getattr(new, name)
        if after[: len(before)] != before:
            raise TransitionConflictError(
                "Committed evidence and receipts are immutable"
            )
    for name, key, immutable in (
        ("questions", "question_id", ("text", "parent_id", "depth")),
        ("agents", "agent_id", ("kind", "question_id", "parent_id", "depth")),
    ):
        before, after = getattr(old, name), getattr(new, name)
        if tuple(getattr(x, key) for x in after[: len(before)]) != tuple(
            getattr(x, key) for x in before
        ):
            raise TransitionConflictError(
                "Committed records cannot be removed or reordered"
            )
        for index, item in enumerate(after):
            if index >= len(before):
                if item.status is not WorkStatus.QUEUED or (
                    name == "agents" and (item.retry_count != 0 or item.attempt != 0)
                ):
                    raise TransitionConflictError("New work must be queued")
                continue
            previous = before[index]
            if any(
                getattr(previous, field) != getattr(item, field) for field in immutable
            ) or (
                item.status not in _WORK_TRANSITIONS[previous.status]
                and not (
                    name == "questions"
                    and previous.status is WorkStatus.QUEUED
                    and item.status is WorkStatus.COMPLETED
                )
            ):
                raise TransitionConflictError("Invalid work transition")
            retry = (
                previous.status is WorkStatus.FAILED
                and item.status is WorkStatus.QUEUED
            )
            if previous.status in TERMINAL_WORK and not retry and item != previous:
                raise TransitionConflictError("Terminal work is immutable")
            if name == "agents":
                if item.retry_count != previous.retry_count + int(
                    retry
                ) or item.attempt != previous.attempt + int(retry):
                    raise TransitionConflictError("Invalid agent retry or attempt")
                if retry and item.error_code is not None:
                    raise TransitionConflictError("Retried agent must clear its error")
            elif retry:
                if not any(
                    a.question_id == item.question_id
                    and a.status is WorkStatus.QUEUED
                    and a.retry_count > b.retry_count
                    for a, b in zip(new.agents, old.agents)
                ):
                    raise TransitionConflictError(
                        "Question retry requires an agent retry"
                    )
    added_tools = new.tool_results[len(old.tool_results) :]
    if len(added_tools) > 1:
        raise TransitionConflictError("Commit one tool effect per transition")
    new_evidence = {e.evidence_id for e in new.evidence[len(old.evidence) :]}
    new_answers = {
        q.question_id
        for q, previous in zip(new.questions, old.questions)
        if q.status is WorkStatus.COMPLETED
        and previous.status is not WorkStatus.COMPLETED
    }
    if (new_evidence or new_answers) and not added_tools:
        raise TransitionConflictError("Tool effects require an atomic receipt")
    for tool in added_tools:
        agent = next((a for a in old.agents if a.agent_id == tool.agent_id), None)
        if (
            agent is None
            or agent.status is not WorkStatus.RUNNING
            or agent.attempt != tool.attempt
        ):
            raise TransitionConflictError("Tool result belongs to an inactive attempt")
        questions = {q.question_id: q for q in new.questions}
        old_questions = {q.question_id: q for q in old.questions}
        if tool.tool_name is ToolName.ADD_QUESTION and (
            set(questions) - set(old_questions) != {tool.question_id}
            or tool.evidence_ids
        ):
            raise TransitionConflictError(
                "Add-question receipt requires a new question"
            )
        if tool.tool_name is ToolName.ANSWER_QUESTION and (
            tool.question_id != agent.question_id
            or tool.question_id not in old_questions
            or old_questions[tool.question_id].status in TERMINAL_WORK
            or questions[tool.question_id].status is not WorkStatus.COMPLETED
            or questions[tool.question_id].evidence_ids != tool.evidence_ids
        ):
            raise TransitionConflictError(
                "Answer receipt requires its committed question answer"
            )
        if new_answers and (
            tool.tool_name is not ToolName.ANSWER_QUESTION
            or new_answers != {tool.question_id}
        ):
            raise TransitionConflictError("Answer effect does not match its receipt")
        if new_evidence and (
            tool.tool_name is not ToolName.RETRIEVE_CONTEXT
            or not new_evidence <= set(tool.evidence_ids)
        ):
            raise TransitionConflictError("Evidence effect does not match its receipt")
        if any(
            e.evidence_id in new_evidence and e.question_id is None
            for e in new.evidence
        ) and (agent.kind is not AgentKind.INTAKE or tool.question_id is not None):
            raise TransitionConflictError(
                "Unassigned evidence requires intake retrieval"
            )
