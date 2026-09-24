"""Application-owned, bounded orchestration records, not conversation history.

Only public question/answer text and compact evidence belong here. Callers must
never put secrets, hidden prompts or reasoning in these public text fields. No
arbitrary metadata, provider responses, executable objects or raw context are
accepted. Serialization uses a closed schema, not pickle or provider serializers.
"""

from __future__ import annotations

import json
import math
import re
from dataclasses import asdict, dataclass, fields
from enum import Enum

SCHEMA_VERSION = 1
MAX_CHECKPOINT_BYTES = 1_048_576
MAX_EVENT_BYTES = 16_384
MAX_QUESTIONS = 8
MAX_DEPTH = 2
MAX_RETRIES = 1
MAX_AGENTS = 32
MAX_EVIDENCE = 128
MAX_TOOL_RESULTS = 256
MAX_EVENTS = 4096


class StateValidationError(ValueError):
    """Invalid or unsafe durable state (messages never include input values)."""


class RunStatus(str, Enum):
    RUNNING = "running"
    INTERRUPTED = "interrupted"
    COMPLETED = "completed"
    FAILED = "failed"
    DISCARDED = "discarded"


class WorkStatus(str, Enum):
    QUEUED = "queued"
    RUNNING = "running"
    WAITING = "waiting"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"


class AgentKind(str, Enum):
    INTAKE = "intake"
    QUESTION = "question"
    FINALIZER = "finalizer"


class ToolName(str, Enum):
    RETRIEVE_CONTEXT = "retrieve_context"
    ADD_QUESTION = "add_question"
    ANSWER_QUESTION = "answer_question"


class ErrorCode(str, Enum):
    AGENT_FAILED = "agent_failed"
    RUN_FAILED = "run_failed"
    INTERRUPTED = "interrupted"
    CANCELLED = "cancelled"


class EventKind(str, Enum):
    CREATED = "created"
    TRANSITION = "transition"
    TOOL_COMMITTED = "tool_committed"
    RESUMED = "resumed"
    EXCHANGE_ATTACHED = "exchange_attached"
    COMPLETED = "completed"
    FAILED = "failed"
    DISCARDED = "discarded"


TERMINAL_RUNS = frozenset({RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.DISCARDED})
TERMINAL_WORK = frozenset(
    {WorkStatus.COMPLETED, WorkStatus.FAILED, WorkStatus.CANCELLED}
)


def identifier(value: str) -> None:
    if type(value) is not str or not re.fullmatch(
        r"[A-Za-z0-9][A-Za-z0-9_.:/-]{0,127}", value
    ):
        raise StateValidationError("Invalid bounded identifier")


def integer(value: int, low: int, high: int) -> None:
    if type(value) is not int or not low <= value <= high:
        raise StateValidationError("Integer outside allowed range")


def text(value: str, limit: int, *, optional: bool = False) -> None:
    if type(value) is not str or len(value) > limit:
        raise StateValidationError("Expected public text")
    try:
        valid = len(value.encode("utf-8")) <= limit
    except UnicodeError:
        valid = False
    if not valid or (not optional and not value.strip()) or "\x00" in value:
        raise StateValidationError("Invalid or oversized public text")


def enum_value(value: object, cls: type[Enum]) -> None:
    if type(value) is not cls:
        raise StateValidationError("Invalid state enumeration")


def identifiers(values: tuple[str, ...], limit: int) -> None:
    if type(values) is not tuple or len(values) > limit:
        raise StateValidationError("Invalid bounded identifier list")
    for value in values:
        identifier(value)
    if len(set(values)) != len(values):
        raise StateValidationError("Duplicate identifiers")


@dataclass(frozen=True)
class RunState:
    run_id: str
    session_id: str
    graph_version: str
    status: RunStatus = RunStatus.RUNNING
    retry_count: int = 0
    exchange_id: int | None = None
    error_code: ErrorCode | None = None

    def __post_init__(self) -> None:
        for value in (self.run_id, self.session_id, self.graph_version):
            identifier(value)
        enum_value(self.status, RunStatus)
        integer(self.retry_count, 0, MAX_EVENTS)
        if self.exchange_id is not None:
            integer(self.exchange_id, 1, 2**63 - 1)
        if self.error_code is not None:
            enum_value(self.error_code, ErrorCode)


@dataclass(frozen=True)
class QuestionState:
    question_id: str
    text: str
    parent_id: str | None = None
    depth: int = 0
    status: WorkStatus = WorkStatus.QUEUED
    answer: str = ""
    evidence_ids: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        identifier(self.question_id)
        if self.parent_id is not None:
            identifier(self.parent_id)
        text(self.text, 4096)
        text(self.answer, 8192, optional=True)
        integer(self.depth, 0, MAX_DEPTH)
        enum_value(self.status, WorkStatus)
        identifiers(self.evidence_ids, MAX_EVIDENCE)
        if (self.status is WorkStatus.COMPLETED) != bool(self.answer.strip()):
            raise StateValidationError("Only completed questions have answers")


@dataclass(frozen=True)
class AgentState:
    agent_id: str
    kind: AgentKind
    question_id: str | None = None
    parent_id: str | None = None
    depth: int = 0
    status: WorkStatus = WorkStatus.QUEUED
    retry_count: int = 0
    attempt: int = 0
    error_code: ErrorCode | None = None

    def __post_init__(self) -> None:
        identifier(self.agent_id)
        for value in (self.question_id, self.parent_id):
            if value is not None:
                identifier(value)
        enum_value(self.kind, AgentKind)
        enum_value(self.status, WorkStatus)
        integer(self.depth, 0, MAX_DEPTH + 2)
        integer(self.retry_count, 0, MAX_RETRIES)
        integer(self.attempt, 0, MAX_EVENTS)
        if self.error_code is not None:
            enum_value(self.error_code, ErrorCode)
        if (self.kind is AgentKind.QUESTION) != (self.question_id is not None):
            raise StateValidationError("Question agents require a question reference")


@dataclass(frozen=True)
class EvidenceState:
    evidence_id: str
    question_id: str
    paper_id: str
    chunk_index: int
    snippet: str
    score: float
    page: int | None = None

    def __post_init__(self) -> None:
        for value in (self.evidence_id, self.question_id, self.paper_id):
            identifier(value)
        integer(self.chunk_index, 0, 2**31 - 1)
        if self.page is not None:
            integer(self.page, 1, 2**31 - 1)
        text(self.snippet, 4096)
        if (
            type(self.score) not in (int, float)
            or not -1e308 <= self.score <= 1e308
            or not math.isfinite(self.score)
        ):
            raise StateValidationError("Evidence score must be finite")


@dataclass(frozen=True)
class ToolResult:
    """Receipt for an already committed effect; never tool inputs or raw output."""

    tool_call_id: str
    agent_id: str
    attempt: int
    tool_name: ToolName
    question_id: str | None = None
    evidence_ids: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        identifier(self.tool_call_id)
        identifier(self.agent_id)
        integer(self.attempt, 0, MAX_EVENTS)
        enum_value(self.tool_name, ToolName)
        if self.question_id is not None:
            identifier(self.question_id)
        identifiers(self.evidence_ids, MAX_EVIDENCE)


def records(values: tuple, cls: type, key: str, limit: int) -> dict:
    if type(values) is not tuple or len(values) > limit:
        raise StateValidationError("Invalid bounded record list")
    result = {}
    for value in values:
        if type(value) is not cls:
            raise StateValidationError("Unexpected record type")
        value.__post_init__()
        record_id = getattr(value, key)
        if record_id in result:
            raise StateValidationError("Duplicate record identifier")
        result[record_id] = value
    return result


def parents(values: dict) -> None:
    for item_id, item in values.items():
        if item.parent_id is None:
            if item.depth != 0:
                raise StateValidationError("Root depth must be zero")
        elif (
            item.parent_id not in values
            or item.parent_id == item_id
            or item.depth != values[item.parent_id].depth + 1
        ):
            raise StateValidationError("Invalid parent/depth relationship")


@dataclass(frozen=True)
class Checkpoint:
    run: RunState
    sequence: int
    questions: tuple[QuestionState, ...] = ()
    agents: tuple[AgentState, ...] = ()
    evidence: tuple[EvidenceState, ...] = ()
    tool_results: tuple[ToolResult, ...] = ()
    schema_version: int = SCHEMA_VERSION

    def __post_init__(self) -> None:
        if type(self.run) is not RunState:
            raise StateValidationError("Expected typed run")
        self.run.__post_init__()
        integer(self.schema_version, SCHEMA_VERSION, SCHEMA_VERSION)
        integer(self.sequence, 0, MAX_EVENTS)
        questions = records(self.questions, QuestionState, "question_id", MAX_QUESTIONS)
        agents = records(self.agents, AgentState, "agent_id", MAX_AGENTS)
        evidence = records(self.evidence, EvidenceState, "evidence_id", MAX_EVIDENCE)
        tools = records(self.tool_results, ToolResult, "tool_call_id", MAX_TOOL_RESULTS)
        parents(questions)
        parents(agents)
        for question in questions.values():
            if not set(question.evidence_ids) <= evidence.keys():
                raise StateValidationError("Unknown question evidence")
        for agent in agents.values():
            if agent.question_id is not None and agent.question_id not in questions:
                raise StateValidationError("Unknown agent question")
        for item in evidence.values():
            if item.question_id not in questions:
                raise StateValidationError("Unknown evidence question")
        for tool in tools.values():
            if (
                tool.agent_id not in agents
                or tool.attempt > agents[tool.agent_id].attempt
                or (tool.question_id is not None and tool.question_id not in questions)
                or not set(tool.evidence_ids) <= evidence.keys()
            ):
                raise StateValidationError("Unknown tool result reference")
        if sum(a.status is WorkStatus.RUNNING for a in agents.values()) > 4:
            raise StateValidationError("Too many running agents")
        if self.run.status is RunStatus.COMPLETED and (
            any(q.status not in TERMINAL_WORK for q in self.questions)
            or any(a.status not in TERMINAL_WORK for a in self.agents)
        ):
            raise StateValidationError("Completed runs cannot contain pending work")


@dataclass(frozen=True)
class OrchestrationEvent:
    run_id: str
    session_id: str
    sequence: int
    operation_id: str
    kind: EventKind
    status: RunStatus
    question_ids: tuple[str, ...] = ()
    agent_ids: tuple[str, ...] = ()
    evidence_ids: tuple[str, ...] = ()
    tool_call_id: str | None = None

    def __post_init__(self) -> None:
        for value in (self.run_id, self.session_id, self.operation_id):
            identifier(value)
        integer(self.sequence, 1, MAX_EVENTS)
        enum_value(self.kind, EventKind)
        enum_value(self.status, RunStatus)
        identifiers(self.question_ids, MAX_QUESTIONS)
        identifiers(self.agent_ids, MAX_AGENTS)
        identifiers(self.evidence_ids, MAX_EVIDENCE)
        if self.tool_call_id is not None:
            identifier(self.tool_call_id)


def encode(record: Checkpoint | OrchestrationEvent) -> str:
    if type(record) not in (Checkpoint, OrchestrationEvent):
        raise StateValidationError("Expected a closed durable record")
    record.__post_init__()
    value = json.dumps(
        asdict(record),
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    )
    limit = MAX_CHECKPOINT_BYTES if type(record) is Checkpoint else MAX_EVENT_BYTES
    if len(value.encode("utf-8")) > limit:
        raise StateValidationError("Durable payload exceeds byte budget")
    return value


def _record(cls: type, value: dict, enums: dict | None = None, tuples: tuple = ()):
    if type(value) is not dict or set(value) != {f.name for f in fields(cls)}:
        raise StateValidationError("Unexpected durable record fields")
    data = dict(value)
    for name, enum in (enums or {}).items():
        if data[name] is not None:
            data[name] = enum(data[name])
    for name in tuples:
        if type(data[name]) is not list:
            raise StateValidationError("Expected durable list")
        data[name] = tuple(data[name])
    return cls(**data)


def decode(value: str, *, event: bool = False) -> Checkpoint | OrchestrationEvent:
    """Reject unknown fields, duplicate keys, invalid types and noncanonical JSON."""
    limit = MAX_EVENT_BYTES if event else MAX_CHECKPOINT_BYTES
    try:
        if (
            type(value) is not str
            or len(value) > limit
            or len(value.encode("utf-8")) > limit
        ):
            raise StateValidationError("Invalid durable payload size")
        data = json.loads(value)
        if event:
            result = _record(
                OrchestrationEvent,
                data,
                {"kind": EventKind, "status": RunStatus},
                ("question_ids", "agent_ids", "evidence_ids"),
            )
        else:
            if type(data) is not dict or set(data) != {
                f.name for f in fields(Checkpoint)
            }:
                raise StateValidationError("Unexpected checkpoint fields")
            data["run"] = _record(
                RunState, data["run"], {"status": RunStatus, "error_code": ErrorCode}
            )
            for name, cls, enums, tuples in (
                ("questions", QuestionState, {"status": WorkStatus}, ("evidence_ids",)),
                (
                    "agents",
                    AgentState,
                    {"kind": AgentKind, "status": WorkStatus, "error_code": ErrorCode},
                    (),
                ),
                ("evidence", EvidenceState, {}, ()),
                (
                    "tool_results",
                    ToolResult,
                    {"tool_name": ToolName},
                    ("evidence_ids",),
                ),
            ):
                if type(data[name]) is not list:
                    raise StateValidationError("Expected checkpoint list")
                data[name] = tuple(
                    _record(cls, item, enums, tuples) for item in data[name]
                )
            result = Checkpoint(**data)
        if encode(result) != value:
            raise StateValidationError("Noncanonical durable payload")
        return result
    except (ValueError, TypeError, KeyError, RecursionError, UnicodeError):
        raise StateValidationError("Invalid durable payload") from None
