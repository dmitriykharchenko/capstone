"""Bounded capabilities over orchestration state, never canonical chat history.

The scheduler constructs a scope; model outputs cannot select run/agent/attempt.
Only typed public records cross this boundary. Events contain identifiers, not
queries, prompts, provider responses or exception details. No provider is opened.
"""

from __future__ import annotations

import hashlib
import json
import re
import unicodedata
from dataclasses import dataclass, replace
from itertools import islice
from typing import Protocol

from .orchestration_state import (
    AgentKind,
    AgentState,
    Checkpoint,
    EventKind,
    EvidenceState,
    QuestionState,
    RunStatus,
    ToolName,
    ToolResult,
    WorkStatus,
    identifier,
    identifiers,
    integer,
    text,
)
from .orchestration_store import SQLiteOrchestrationStore
from .paper_search import PaperChunkMatch


class ToolError(ValueError):
    """Fixed, display-safe failure; never embed untrusted input."""


class AuthorizationError(ToolError):
    pass


class DuplicateQuestion(ToolError):
    pass


class LimitError(ToolError):
    pass


@dataclass(frozen=True)
class Limits:
    questions: int = 8
    depth: int = 2
    concurrency: int = 4
    chunks_per_query: int = 5
    retries: int = 1
    queries_per_attempt: int = 2
    followups: int = 2

    def __post_init__(self):
        for value, low, high in (
            (self.questions, 1, 8),
            (self.depth, 0, 2),
            (self.concurrency, 1, 4),
            (self.chunks_per_query, 1, 5),
            (self.retries, 0, 1),
            (self.queries_per_attempt, 1, 2),
            (self.followups, 0, 2),
        ):
            integer(value, low, high)


QUERY_BYTES = 2048
ANSWER_BYTES = 4096
SNIPPET_BYTES = 1024
PAYLOAD_BYTES = 65536
# One mandatory Intake call, additional to the optional per-attempt allowance.
# Its receipt is run-scoped, so a resumed Intake does not search again.
ORIGINAL_QUERY_CALL_ID = "intake-original-query"

# Defense in depth for recognizable accidental secrets/private-output markers;
# not a semantic secret classifier. Credentials/hidden prompts are never inputs.
_PRIVATE = re.compile(
    r"(?i)(?:bearer\s+\S+|\bsk-[a-z0-9_-]{8,}|-----BEGIN .*PRIVATE KEY|"
    r"(?:api[_ -]?key|password|access[_ -]?token|secret)\s*[:=]\s*\S+|"
    r"</?(?:think|analysis|system|developer)>|chain.of.thought|hidden prompt)"
)


def public_text(value: str, limit: int) -> str:
    text(value, limit)
    result = " ".join(unicodedata.normalize("NFKC", value).split())
    text(result, limit)
    if _PRIVATE.search(result) or any(
        unicodedata.category(c).startswith("C") for c in result
    ):
        raise ToolError("Unsafe public text")
    return result


def normalized(value: str) -> str:
    return " ".join(unicodedata.normalize("NFKC", value).casefold().split()).rstrip(
        "?.! "
    )


def bounded_json(value: object, limit: int = PAYLOAD_BYTES) -> str:
    try:
        result = json.dumps(
            value,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
            allow_nan=False,
        )
        text(result, limit)
        return result
    except (ValueError, TypeError, UnicodeError, RecursionError):
        raise ToolError("Invalid or oversized payload") from None


def digest(value: object) -> str:
    return hashlib.sha256(bounded_json(value).encode()).hexdigest()


def evidence_key(evidence: EvidenceState) -> tuple:
    return (
        -evidence.score,
        evidence.paper_id,
        evidence.chunk_index,
        evidence.snippet,
        evidence.evidence_id,
    )


def evidence_groups(
    values: tuple[EvidenceState, ...],
) -> tuple[tuple[EvidenceState, ...], ...]:
    """Connected duplicate groups, best representative first, in stable rank order."""
    if type(values) is not tuple or len(values) > 128:
        raise ToolError("Invalid evidence list")
    for value in values:
        if type(value) is not EvidenceState:
            raise ToolError("Expected typed evidence")
        value.__post_init__()
    groups = []
    for item in sorted(values, key=evidence_key):
        keys = {(item.paper_id, item.chunk_index), normalized(item.snippet)}
        members, remaining = [item], []
        for existing, members_before in groups:
            if keys & existing:
                keys |= existing
                members.extend(members_before)
            else:
                remaining.append((existing, members_before))
        groups = remaining + [(keys, tuple(sorted(members, key=evidence_key)))]
    return tuple(
        members for _, members in sorted(groups, key=lambda g: evidence_key(g[1][0]))
    )


def rank_evidence(values: tuple[EvidenceState, ...]) -> tuple[EvidenceState, ...]:
    """Stable best-first ranking; identical chunks and normalized text appear once."""
    return tuple(group[0] for group in evidence_groups(values))


def intake_evidence_ids(snapshot: Checkpoint) -> set[str]:
    intake = {a.agent_id for a in snapshot.agents if a.kind is AgentKind.INTAKE}
    return {
        i
        for r in snapshot.tool_results
        if r.agent_id in intake
        and r.tool_name is ToolName.RETRIEVE_CONTEXT
        and r.question_id is None
        for i in r.evidence_ids
    }


def final_evidence_ids(snapshot: Checkpoint) -> set[str]:
    completed = {
        q.question_id for q in snapshot.questions if q.status is WorkStatus.COMPLETED
    }
    retrieved = {
        i
        for r in snapshot.tool_results
        if r.tool_name is ToolName.RETRIEVE_CONTEXT
        for i in r.evidence_ids
    }
    return intake_evidence_ids(snapshot) | {
        e.evidence_id
        for e in snapshot.evidence
        if e.question_id in completed and e.evidence_id in retrieved
    }


def handler_evidence_ids(snapshot: Checkpoint, agent_id: str) -> set[str]:
    return intake_evidence_ids(snapshot) | {
        i
        for r in snapshot.tool_results
        if r.agent_id == agent_id and r.tool_name is ToolName.RETRIEVE_CONTEXT
        for i in r.evidence_ids
    }


@dataclass(frozen=True)
class ToolScope:
    run_id: str
    session_id: str
    graph_version: str
    agent_id: str
    attempt: int = 0

    def __post_init__(self):
        for value in (self.run_id, self.session_id, self.graph_version, self.agent_id):
            identifier(value)
        integer(self.attempt, 0, 4096)


@dataclass(frozen=True)
class ToolEvent:
    """Semantic projection of one committed event (not a second event journal)."""

    kind: str
    run_id: str
    agent_id: str
    sequence: int
    tool_call_id: str
    question_id: str | None
    evidence_ids: tuple[str, ...]


@dataclass(frozen=True)
class ToolOutcome:
    event: ToolEvent
    question: QuestionState | None = None
    evidence: tuple[EvidenceState, ...] = ()
    warning: str | None = None

    def __post_init__(self):
        if self.warning not in (None, "retrieval_failed"):
            raise ToolError("Invalid retrieval outcome")


def _call_prefix(agent_id: str, attempt: int, call_id: str) -> str:
    return "t-" + digest([agent_id, attempt, call_id])[:32] + "-"


def _failure_operation(receipt: ToolResult) -> str:
    return "tool:retrieval_failed:" + receipt.tool_call_id


def _receipt_warning(receipt, event):
    if (
        event.kind is not EventKind.TOOL_COMMITTED
        or event.tool_call_id != receipt.tool_call_id
    ):
        raise ToolError("Invalid tool outcome event")
    if event.operation_id == receipt.tool_call_id:
        return None
    if (
        receipt.tool_name is ToolName.RETRIEVE_CONTEXT
        and not receipt.evidence_ids
        and event.operation_id == _failure_operation(receipt)
    ):
        return "retrieval_failed"
    raise ToolError("Invalid tool outcome event")


class Retriever(Protocol):
    def retrieve_chunks(self, query: str, n_results: int = 5): ...


class OrchestrationTools:
    """One trusted, immutable capability binding per active agent attempt.

    Call IDs are hashed with canonical arguments in durable receipts. Replays
    return the original committed outcome; changing arguments for an ID fails.
    Concurrent snapshot conflicts propagate to the scheduler, never retrying a
    provider automatically. Handler retry/concurrency policy is metadata only.
    """

    def __init__(
        self,
        store: SQLiteOrchestrationStore,
        scope: ToolScope,
        retriever: Retriever,
        *,
        limits: Limits = Limits(),
    ):
        if type(scope) is not ToolScope or type(limits) is not Limits:
            raise ToolError("Invalid tool binding")
        self._store, self.scope, self._retriever, self.limits = (
            store,
            scope,
            retriever,
            limits,
        )

    def snapshot(self) -> Checkpoint:
        return self._store.load(
            self.scope.run_id, self.scope.session_id, self.scope.graph_version
        )

    def authorize(
        self, *kinds: AgentKind, active: bool = True
    ) -> tuple[Checkpoint, AgentState]:
        snapshot = self.snapshot()
        agent = next(
            (a for a in snapshot.agents if a.agent_id == self.scope.agent_id), None
        )
        if (
            agent is None
            or agent.kind not in kinds
            or agent.attempt != self.scope.attempt
        ):
            raise AuthorizationError("Tool scope is not authorized")
        if active and (
            snapshot.run.status is not RunStatus.RUNNING
            or snapshot.run.exchange_id is not None
            or agent.status is not WorkStatus.RUNNING
        ):
            raise AuthorizationError("Agent attempt is not active")
        return snapshot, agent

    def _prepare(self, name, call_id, arguments, *kinds):
        identifier(call_id)
        snapshot, agent = self.authorize(*kinds, active=False)
        attempt = (
            0
            if name is ToolName.RETRIEVE_CONTEXT and call_id == ORIGINAL_QUERY_CALL_ID
            else self.scope.attempt
        )
        prefix = _call_prefix(self.scope.agent_id, attempt, call_id)
        receipt_id = prefix + digest([name.value, arguments])
        existing = next(
            (r for r in snapshot.tool_results if r.tool_call_id.startswith(prefix)),
            None,
        )
        if existing is not None:
            if (
                existing.tool_call_id != receipt_id
                or existing.agent_id != agent.agent_id
                or existing.tool_name is not name
            ):
                raise ToolError("Tool call ID conflicts with committed input")
            return snapshot, agent, receipt_id, self._outcome(existing)
        self.authorize(*kinds)
        if len(snapshot.tool_results) >= 256:
            raise LimitError("Tool receipt limit reached")
        return snapshot, agent, receipt_id, None

    def _outcome(self, receipt: ToolResult) -> ToolOutcome:
        events = self._store.events(
            self.scope.run_id,
            self.scope.session_id,
            self.scope.graph_version,
            limit=4096,
        )
        matching = [e for e in events if e.tool_call_id == receipt.tool_call_id]
        if len(matching) != 1:
            raise ToolError("Invalid tool outcome event")
        event = matching[0]
        warning = _receipt_warning(receipt, event)
        original = self._store.load(
            self.scope.run_id,
            self.scope.session_id,
            self.scope.graph_version,
            sequence=event.sequence,
        )
        evidence = {e.evidence_id: e for e in original.evidence}
        kind = {
            ToolName.ADD_QUESTION: "question_added",
            ToolName.ANSWER_QUESTION: "question_answered",
            ToolName.RETRIEVE_CONTEXT: "context_retrieved",
        }[receipt.tool_name]
        return ToolOutcome(
            ToolEvent(
                kind,
                self.scope.run_id,
                receipt.agent_id,
                event.sequence,
                receipt.tool_call_id,
                receipt.question_id,
                receipt.evidence_ids,
            ),
            next(
                (q for q in original.questions if q.question_id == receipt.question_id),
                None,
            ),
            rank_evidence(tuple(evidence[i] for i in receipt.evidence_ids))
            if receipt.tool_name is ToolName.RETRIEVE_CONTEXT
            else tuple(evidence[i] for i in receipt.evidence_ids),
            warning,
        )

    def retrieval_warnings(self) -> tuple[str, ...]:
        snapshot = self.snapshot()
        events = self._store.events(
            self.scope.run_id,
            self.scope.session_id,
            self.scope.graph_version,
            limit=4096,
        )
        warnings = set()
        for receipt in snapshot.tool_results:
            if receipt.tool_name is not ToolName.RETRIEVE_CONTEXT:
                continue
            matching = [e for e in events if e.tool_call_id == receipt.tool_call_id]
            if len(matching) != 1:
                raise ToolError("Invalid tool outcome event")
            warning = _receipt_warning(receipt, matching[0])
            if warning:
                warnings.add(warning)
            elif not receipt.evidence_ids:
                warnings.add("empty_evidence")
        return tuple(sorted(warnings))

    def _commit(self, snapshot, receipt, *, retrieval_failed=False, **updates):
        self._store.commit(
            replace(
                snapshot, tool_results=snapshot.tool_results + (receipt,), **updates
            ),
            operation_id=_failure_operation(receipt)
            if retrieval_failed
            else receipt.tool_call_id,
        )
        return self._outcome(receipt)

    def add_question(
        self, question: str, *, call_id: str, parent_id: str | None = None
    ) -> ToolOutcome:
        question = public_text(question, QUERY_BYTES)
        if parent_id is not None:
            identifier(parent_id)
        old, agent, tool_id, replay = self._prepare(
            ToolName.ADD_QUESTION,
            call_id,
            [question, parent_id],
            AgentKind.INTAKE,
            AgentKind.QUESTION,
        )
        if replay is not None:
            return replay
        if (agent.kind is AgentKind.INTAKE and parent_id is not None) or (
            agent.kind is AgentKind.QUESTION and parent_id != agent.question_id
        ):
            raise AuthorizationError("Question parent is not authorized")
        parent = next((q for q in old.questions if q.question_id == parent_id), None)
        depth = parent.depth + 1 if parent is not None else 0
        if any(normalized(q.text) == normalized(question) for q in old.questions):
            raise DuplicateQuestion("Question already exists")
        if len(old.questions) >= self.limits.questions or depth > self.limits.depth:
            raise LimitError("Question limit reached")
        if (
            parent is not None
            and sum(q.parent_id == parent_id for q in old.questions)
            >= self.limits.followups
        ):
            raise LimitError("Follow-up limit reached")
        item = QuestionState(
            "q-" + digest(normalized(question)), question, parent_id, depth
        )
        receipt = ToolResult(
            tool_id,
            agent.agent_id,
            agent.attempt,
            ToolName.ADD_QUESTION,
            item.question_id,
        )
        return self._commit(old, receipt, questions=old.questions + (item,))

    def retrieve_context(
        self, query: str, *, call_id: str, best_effort: bool = False
    ) -> ToolOutcome:
        if type(best_effort) is not bool:
            raise ToolError("Invalid retrieval policy")
        canonical_query = public_text(query, QUERY_BYTES)
        original = call_id == ORIGINAL_QUERY_CALL_ID
        if not original:
            query = canonical_query
        else:
            self.authorize(AgentKind.INTAKE, active=False)
        old, agent, tool_id, replay = self._prepare(
            ToolName.RETRIEVE_CONTEXT,
            call_id,
            query,
            AgentKind.INTAKE,
            AgentKind.QUESTION,
        )
        if replay is not None:
            if replay.warning and not best_effort:
                raise ToolError("Context retrieval failed")
            return replay
        original_prefix = _call_prefix(agent.agent_id, 0, ORIGINAL_QUERY_CALL_ID)
        count = sum(
            r.agent_id == agent.agent_id
            and r.attempt == agent.attempt
            and r.tool_name is ToolName.RETRIEVE_CONTEXT
            and not r.tool_call_id.startswith(original_prefix)
            for r in old.tool_results
        )
        if not original and count >= self.limits.queries_per_attempt:
            raise LimitError("Retrieval limit reached")
        iterator = None
        failed = False
        try:
            iterator = iter(
                self._retriever.retrieve_chunks(
                    query, n_results=self.limits.chunks_per_query
                )
            )
            raw = tuple(islice(iterator, self.limits.chunks_per_query))
            evidence = []
            for chunk in raw:
                if type(chunk) is not PaperChunkMatch:
                    raise ToolError("Invalid retrieved chunk")
                public_text(chunk.paper_id, 128)
                text(chunk.text, 32768)
                compact = " ".join(chunk.text.split())
                snippet = compact.encode("utf-8")[:SNIPPET_BYTES].decode(
                    "utf-8", errors="ignore"
                )
                try:
                    snippet = public_text(snippet, SNIPPET_BYTES)
                except ToolError:
                    snippet = "[Unsafe source content omitted]"
                item = EvidenceState(
                    "e-"
                    + digest(
                        [
                            agent.question_id,
                            chunk.paper_id,
                            chunk.chunk_index,
                            chunk.score,
                            snippet,
                        ]
                    ),
                    agent.question_id,
                    chunk.paper_id,
                    chunk.chunk_index,
                    snippet,
                    chunk.score,
                    chunk.page,
                )
                evidence.append(item)
            # Retain bounded duplicate variants in receipts so a later query can
            # bridge their chunk/text identities. Consumers receive only winners.
            ranked = tuple(
                sorted({e.evidence_id: e for e in evidence}.values(), key=evidence_key)
            )
        except Exception:
            if not best_effort:
                raise ToolError("Context retrieval failed") from None
            ranked, failed = (), True
        finally:
            if iterator is not None and hasattr(iterator, "close"):
                try:
                    iterator.close()
                except Exception:
                    pass
        known = {e.evidence_id for e in old.evidence}
        added = tuple(e for e in ranked if e.evidence_id not in known)
        if len(old.evidence) + len(added) > 128:
            raise LimitError("Evidence limit reached")
        receipt = ToolResult(
            tool_id,
            agent.agent_id,
            agent.attempt,
            ToolName.RETRIEVE_CONTEXT,
            agent.question_id,
            tuple(e.evidence_id for e in ranked),
        )
        return self._commit(
            old, receipt, evidence=old.evidence + added, retrieval_failed=failed
        )

    def answer_question(
        self,
        question_id: str,
        answer: str,
        evidence_ids: tuple[str, ...],
        *,
        call_id: str,
    ) -> ToolOutcome:
        identifier(question_id)
        answer = public_text(answer, ANSWER_BYTES)
        identifiers(evidence_ids, 10)
        old, agent, tool_id, replay = self._prepare(
            ToolName.ANSWER_QUESTION,
            call_id,
            [question_id, answer, evidence_ids],
            AgentKind.QUESTION,
        )
        if replay is not None:
            return replay
        if question_id != agent.question_id:
            raise AuthorizationError("Question is not assigned to this agent")
        question = next(q for q in old.questions if q.question_id == question_id)
        if question.status not in (WorkStatus.QUEUED, WorkStatus.RUNNING):
            raise ToolError("Question cannot be answered")
        permitted = handler_evidence_ids(old, agent.agent_id)
        if not set(evidence_ids) <= permitted:
            raise AuthorizationError("Evidence is not assigned to this agent")
        evidence = rank_evidence(
            tuple(e for e in old.evidence if e.evidence_id in evidence_ids)
        )
        if len(evidence) != len(evidence_ids):
            raise ToolError("Answer evidence must be unique and assigned")
        ranked_ids = tuple(e.evidence_id for e in evidence)
        answered = replace(
            question,
            status=WorkStatus.COMPLETED,
            answer=answer,
            evidence_ids=ranked_ids,
        )
        receipt = ToolResult(
            tool_id,
            agent.agent_id,
            agent.attempt,
            ToolName.ANSWER_QUESTION,
            question_id,
            ranked_ids,
        )
        return self._commit(
            old,
            receipt,
            questions=tuple(
                answered if q.question_id == question_id else q for q in old.questions
            ),
        )
