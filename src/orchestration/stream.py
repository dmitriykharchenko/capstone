"""Snapshot-first deterministic projections of the committed orchestration journal.

Wire sequence = checkpoint sequence * 1024 + projection ordinal. Snapshot uses
ordinal 900, final delta 1000, terminal 1001. Reconnect snapshots supersede all
prior trace records; only later checkpoints are projected. Tool start/completion
pairs describe committed logical calls, never uncommitted provider activity.
"""

from copy import deepcopy
from dataclasses import asdict
import json

from .runtime import WARNINGS
from .state import WorkStatus
from .tools import evidence_key
from .wire import ERROR, StreamValidator, validate_type


def record(item):
    result = json.loads(json.dumps(asdict(item)))
    result.pop("error_code", None)
    for key, value in result.items():
        if isinstance(value, tuple):
            result[key] = list(value)
    return result


class Projection:
    def __init__(self, store, args, turn_id):
        self.store, self.args, self.turn_id = store, args, turn_id

    def envelope(self, sequence, event_type, **payload):
        return dict(
            version=1,
            run_id=self.args[0],
            session_id=self.args[1],
            turn_id=self.turn_id,
            sequence=sequence,
            event=event_type,
            **payload,
        )

    def evidence(self, snapshot):
        metadata = self.store.source_display(*self.args)
        result = []
        for rank, evidence in enumerate(sorted(snapshot.evidence, key=evidence_key), 1):
            filename, title = metadata.get(
                (evidence.paper_id, evidence.chunk_index), ("", "")
            )
            result.append(
                dict(
                    evidence_id=evidence.evidence_id,
                    question_id=evidence.question_id,
                    source=dict(
                        rank=rank,
                        paper_id=evidence.paper_id,
                        chunk_index=evidence.chunk_index,
                        filename=filename,
                        title=title,
                        page=evidence.page or 0,
                        score=evidence.score,
                        snippet=evidence.snippet,
                    ),
                )
            )
        validate_type(result, "evidence_list")
        return result

    def sources(self, evidence_ids):
        snapshot = self.store.load(*self.args)
        if not set(evidence_ids) <= {e.evidence_id for e in snapshot.evidence}:
            raise ValueError(ERROR)
        selected, seen = [], set()
        for item in self.evidence(snapshot):
            source = item["source"]
            key = (source["paper_id"], source["chunk_index"])
            if item["evidence_id"] in evidence_ids and key not in seen:
                seen.add(key)
                selected.append(dict(source, rank=len(selected) + 1))
        validate_type(selected, "sources")
        return selected

    def snapshot(self, snapshot):
        value = dict(
            checkpoint_sequence=snapshot.sequence,
            status=snapshot.run.status.value,
            exchange_id=snapshot.run.exchange_id,
            questions=[record(q) for q in snapshot.questions],
            agents=[record(a) for a in snapshot.agents],
            tools=[record(t) for t in snapshot.tool_results],
            evidence=self.evidence(snapshot),
        )
        return self.envelope(snapshot.sequence * 1024 + 900, "snapshot", snapshot=value)

    def changes(self, before, after, journal):
        events = []
        old_questions = {q.question_id: q for q in before.questions}
        for question in after.questions:
            if question.question_id not in old_questions:
                events.append(("question_added", {"question": record(question)}))
        if after.evidence != before.evidence:
            events.append(("evidence", {"evidence": self.evidence(after)}))
        for question in after.questions:
            old = old_questions.get(question.question_id)
            if old is not None and old != question:
                kind = {
                    WorkStatus.COMPLETED: "question_answered",
                    WorkStatus.FAILED: "question_failed",
                }.get(question.status, "question_status")
                events.append((kind, {"question": record(question)}))
        old_tools = {t.tool_call_id for t in before.tool_results}
        for tool in after.tool_results:
            if tool.tool_call_id not in old_tools:
                events.extend(
                    (kind, {"tool": record(tool)})
                    for kind in ("tool_started", "tool_completed")
                )
        old_agents = {a.agent_id: a for a in before.agents}
        for agent in after.agents:
            old = old_agents.get(agent.agent_id)
            if old == agent:
                continue
            if old is not None and agent.attempt > old.attempt:
                events.append(("agent_retried", {"agent": record(agent)}))
            elif agent.status is WorkStatus.RUNNING:
                events.append(("agent_started", {"agent": record(agent)}))
            elif old is not None and agent.status in (
                WorkStatus.COMPLETED,
                WorkStatus.FAILED,
                WorkStatus.CANCELLED,
            ):
                events.append(("agent_completed", {"agent": record(agent)}))
        for code in WARNINGS:
            if f":warning:{code}:" in journal.operation_id:
                events.append(("warning", {"code": code}))
        if len(events) >= 900:
            raise ValueError(ERROR)
        return [
            self.envelope(after.sequence * 1024 + index, kind, **data)
            for index, (kind, data) in enumerate(events, 1)
        ]


class StreamSink:
    def __init__(self, projection, publisher, mode, indexed):
        self.projection, self.publisher = projection, publisher
        self.mode, self.indexed = mode, indexed
        self.outputs = {}
        self.validator = StreamValidator(
            projection.args[1], projection.args[0], projection.turn_id
        )
        self.cursor = 0

    def emit(self, output):
        if output.sequence <= self.cursor and output.kind not in (
            "done",
            "failed",
            "discarded",
        ):
            return
        key = (output.sequence, output.kind)
        if key in self.outputs and self.outputs[key] != output:
            raise ValueError(ERROR)
        self.outputs[key] = output

    def begin(self, snapshot):
        self.cursor = snapshot.sequence
        return self.validate(self.projection.snapshot(snapshot))

    def validate(self, event):
        self.validator.accept(deepcopy(event))
        return event

    def drain(self):
        p = self.projection
        events = p.store.events(*p.args, after=self.cursor, limit=4096)
        for journal in events:
            before = p.store.load(*p.args, sequence=journal.sequence - 1)
            after = p.store.load(*p.args, sequence=journal.sequence)
            for event in p.changes(before, after, journal):
                yield self.validate(event)
            output = self.outputs.pop((journal.sequence, "delta"), None)
            if output is not None:
                agent = next(
                    a
                    for a in after.agents
                    if a.kind == "finalizer" and a.status == "running"
                )
                yield self.validate(
                    p.envelope(
                        journal.sequence * 1024 + 1000,
                        "final_delta",
                        agent_id=agent.agent_id,
                        kind="finalizer",
                        delta=output.text,
                    )
                )
            self.cursor = journal.sequence
        for key, output in list(self.outputs.items()):
            del self.outputs[key]
            if output.kind == "done":
                yield self.validate(
                    p.envelope(
                        output.sequence * 1024 + 1001,
                        "done",
                        exchange_id=output.exchange_id,
                        reply=output.text,
                        chat_mode=self.mode,
                        indexed=self.indexed,
                        sources=self.publisher.saved_sources(output.exchange_id),
                    )
                )
            elif output.kind in ("failed", "discarded"):
                yield self.error(output.kind, False)

    def error(self, code="interrupted", resumable=True):
        return self.validate(
            self.projection.envelope(
                self.validator.sequence + 1,
                "error",
                code=code,
                message=ERROR,
                resumable=resumable,
            )
        )
