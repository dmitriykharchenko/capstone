"""Closed, versioned public transport contract shared with the Rust boundary.

Validation rejects rather than echoing unsafe input. It never normalizes the
visible reply. The deliberately conservative marker check is defense in depth,
not a semantic classifier; only explicitly public fields may enter this module.
"""

from __future__ import annotations

import json
import math
from pathlib import Path

SCHEMA = json.loads(Path(__file__).with_suffix(".json").read_text())
ERROR = SCHEMA["error_message"]


def safe_text(value: str) -> bool:
    folded = "".join(
        chr(ord(c) - 0xFEE0) if 0xFF01 <= ord(c) <= 0xFF5E else c for c in value
    )
    folded = " ".join(folded.split()).lower()
    return not any(
        marker in folded for marker in SCHEMA["private_markers"]
    ) and not any(
        (ord(c) < 32 and c not in "\n\r\t")
        or 127 <= ord(c) <= 159
        or 0x200B <= ord(c) <= 0x200F
        or 0x202A <= ord(c) <= 0x202E
        or 0x2060 <= ord(c) <= 0x206F
        or ord(c) in (0xFEFF, 0xFFFC)
        for c in value
    )


def validate_type(value, name):
    rule = SCHEMA["types"][name]
    kind = rule["kind"]
    valid = False
    if kind == "nullable":
        if value is not None:
            validate_type(value, rule["item"])
        return
    if kind == "object":
        validate_fields(value, rule["fields"])
        return
    if kind == "array":
        valid = type(value) is list and len(value) <= rule["max"]
        if valid:
            for item in value:
                validate_type(item, rule["item"])
            keys = [item[rule["key"]] for item in value] if "key" in rule else value
            if "key" in rule or rule.get("unique"):
                valid = len(set(keys)) == len(keys)
    elif kind == "id":
        valid = (
            type(value) is str
            and 1 <= len(value) <= 128
            and value[0].isascii()
            and value[0].isalnum()
            and all(c.isascii() and (c.isalnum() or c in "_.:/-") for c in value)
            and safe_text(value)
        )
    elif kind == "int":
        valid = type(value) is int and rule["min"] <= value <= rule["max"]
    elif kind == "number":
        valid = (
            type(value) in (int, float)
            and math.isfinite(value)
            and rule["min"] <= value <= rule["max"]
        )
    elif kind == "bool":
        valid = type(value) is bool
    elif kind == "enum":
        valid = type(value) is str and value in rule["values"]
    elif kind == "text":
        valid = (
            type(value) is str
            and rule["min"] <= len(value.encode("utf-8")) <= rule["max"]
            and (not rule["min"] or bool(value.strip()))
            and safe_text(value)
        )
    if not valid:
        raise ValueError(ERROR)


def validate_fields(value, fields):
    if type(value) is not dict or value.keys() != fields.keys():
        raise ValueError(ERROR)
    for key, name in fields.items():
        validate_type(value[key], name)


def loads(line: str):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError(ERROR)
            result[key] = value
        return result

    try:
        if len(line.encode("utf-8")) > SCHEMA["max_bytes"]:
            raise ValueError(ERROR)
        return json.loads(line, object_pairs_hook=pairs)
    except (ValueError, TypeError, RecursionError, UnicodeError):
        raise ValueError(ERROR) from None


def validate_event(event):
    try:
        if type(event) is not dict or event.get("event") not in SCHEMA["events"]:
            raise ValueError(ERROR)
        fields = SCHEMA["envelope"] | SCHEMA["events"][event["event"]]
        validate_fields({k: v for k, v in event.items() if k != "event"}, fields)
        encoded = json.dumps(
            event, ensure_ascii=False, allow_nan=False, separators=(",", ":")
        )
        if len(encoded.encode("utf-8")) > SCHEMA["max_bytes"]:
            raise ValueError(ERROR)
        return event
    except (
        ValueError,
        TypeError,
        KeyError,
        OverflowError,
        RecursionError,
        UnicodeError,
    ):
        raise ValueError(ERROR) from None


class StreamValidator:
    """One transport connection, with a canonical snapshot as its only baseline."""

    def __init__(self, session_id, run_id, turn_id):
        for value in (session_id, run_id, turn_id):
            validate_type(value, "id")
        self.identity = (session_id, run_id, turn_id)
        self.sequence = 0
        self.terminal = False
        self.snapshot = None
        self.started_tools = {}
        self.completed_tools = set()
        self.count = 0
        self.final_text = ""

    def accept(self, event):
        validate_event(event)
        if (
            self.terminal
            or self.count >= 16384
            or tuple(event[k] for k in ("session_id", "run_id", "turn_id"))
            != self.identity
            or event["sequence"] <= self.sequence
        ):
            raise ValueError(ERROR)
        kind = event["event"]
        if kind != "error" and event["sequence"] >= SCHEMA["max_sequence"]:
            raise ValueError(ERROR)
        if kind == "snapshot":
            if self.snapshot is not None or self.sequence:
                raise ValueError(ERROR)
            self.snapshot = event["snapshot"]
            if event["sequence"] != self.snapshot["checkpoint_sequence"] * 1024 + 900:
                raise ValueError(ERROR)
            self._snapshot()
            self.completed_tools = {t["tool_call_id"] for t in self.snapshot["tools"]}
        elif kind != "error":
            if self.snapshot is None:
                raise ValueError(ERROR)
            self._transition(event)
        self.sequence = event["sequence"]
        self.count += 1
        self.terminal = kind in ("done", "error")
        return event

    def _snapshot(self):
        state = self.snapshot
        for name, key in (("questions", "question_id"), ("agents", "agent_id")):
            items = {item[key]: item for item in state[name]}
            for item in items.values():
                parent = items.get(item["parent_id"])
                if item["depth"] != (parent["depth"] + 1 if parent else 0) or (
                    item["parent_id"] is not None and parent is None
                ):
                    raise ValueError(ERROR)
        questions = {q["question_id"] for q in state["questions"]}
        evidence = {e["evidence_id"] for e in state["evidence"]}
        agents = {a["agent_id"]: a for a in state["agents"]}
        if sum(a["status"] == "running" for a in agents.values()) > 4:
            raise ValueError(ERROR)
        for q in state["questions"]:
            if (q["status"] == "completed") != bool(q["answer"].strip()) or not set(
                q["evidence_ids"]
            ) <= evidence:
                raise ValueError(ERROR)
        for a in agents.values():
            if (a["kind"] == "question") != (a["question_id"] is not None) or (
                a["question_id"] is not None and a["question_id"] not in questions
            ):
                raise ValueError(ERROR)
        for e in state["evidence"]:
            if e["question_id"] is not None and e["question_id"] not in questions:
                raise ValueError(ERROR)
        self._ranked(state["evidence"])
        for t in state["tools"]:
            if (
                t["agent_id"] not in agents
                or t["attempt"] > agents[t["agent_id"]]["attempt"]
                or not set(t["evidence_ids"]) <= evidence
                or (t["question_id"] is not None and t["question_id"] not in questions)
            ):
                raise ValueError(ERROR)
        if state["status"] == "completed" and (
            state["exchange_id"] is None
            or any(
                x["status"] not in ("completed", "failed", "cancelled")
                for x in state["questions"] + state["agents"]
            )
        ):
            raise ValueError(ERROR)

    @staticmethod
    def _ranked(items):
        if [e["source"]["rank"] for e in items] != list(range(1, len(items) + 1)):
            raise ValueError(ERROR)
        keys = [
            (
                -e["source"]["score"],
                e["source"]["paper_id"],
                e["source"]["chunk_index"],
                e["source"]["snippet"],
                e["evidence_id"],
            )
            for e in items
        ]
        if keys != sorted(keys):
            raise ValueError(ERROR)

    def _transition(self, event):
        kind, state = event["event"], self.snapshot
        if kind.startswith("question_"):
            item = event["question"]
            old = next(
                (
                    q
                    for q in state["questions"]
                    if q["question_id"] == item["question_id"]
                ),
                None,
            )
            if (kind == "question_added") != (old is None):
                raise ValueError(ERROR)
            if old and (
                old["text"] != item["text"]
                or old["parent_id"] != item["parent_id"]
                or old["depth"] != item["depth"]
                or old["status"] in ("completed", "cancelled")
            ):
                raise ValueError(ERROR)
            expected = {
                "question_added": "queued",
                "question_answered": "completed",
                "question_failed": "failed",
            }.get(kind)
            if expected and item["status"] != expected:
                raise ValueError(ERROR)
            if old and (
                old == item
                or (
                    kind == "question_status"
                    and item["status"]
                    not in ("queued", "running", "waiting", "cancelled")
                )
            ):
                raise ValueError(ERROR)
            state["questions"] = [q for q in state["questions"] if q != old] + [item]
        elif kind.startswith("agent_"):
            item = event["agent"]
            old = next(
                (a for a in state["agents"] if a["agent_id"] == item["agent_id"]), None
            )
            if kind == "agent_started" and (
                item["status"] != "running"
                or (old is not None and old["status"] != "queued")
            ):
                raise ValueError(ERROR)
            if kind == "agent_completed" and (
                old is None
                or old["status"] != "running"
                or item["status"] not in ("completed", "failed", "cancelled")
            ):
                raise ValueError(ERROR)
            if kind == "agent_retried" and (
                old is None
                or old["status"] != "failed"
                or item["attempt"] != old["attempt"] + 1
                or item["retry_count"] != old["retry_count"] + 1
                or item["status"] != "queued"
            ):
                raise ValueError(ERROR)
            if old and any(
                old[k] != item[k] for k in ("kind", "question_id", "parent_id", "depth")
            ):
                raise ValueError(ERROR)
            if (
                old
                and kind != "agent_retried"
                and (
                    item["attempt"] != old["attempt"]
                    or old["status"] in ("completed", "cancelled")
                )
            ):
                raise ValueError(ERROR)
            state["agents"] = [a for a in state["agents"] if a != old] + [item]
        elif kind.startswith("tool_"):
            tool = event["tool"]
            identity = tool["tool_call_id"]
            agent = next(
                (a for a in state["agents"] if a["agent_id"] == tool["agent_id"]), None
            )
            if (
                agent is None
                or agent["status"] != "running"
                or tool["attempt"] != agent["attempt"]
                or identity in self.completed_tools
            ):
                raise ValueError(ERROR)
            if kind == "tool_started":
                if identity in self.started_tools:
                    raise ValueError(ERROR)
                self.started_tools[identity] = tool
            else:
                if self.started_tools.get(identity) != tool:
                    raise ValueError(ERROR)
                del self.started_tools[identity]
                self.completed_tools.add(identity)
                state["tools"].append(tool)
        elif kind == "evidence":
            old = {e["evidence_id"] for e in state["evidence"]}
            if not old <= {e["evidence_id"] for e in event["evidence"]}:
                raise ValueError(ERROR)
            for previous in state["evidence"]:
                item = next(
                    e
                    for e in event["evidence"]
                    if e["evidence_id"] == previous["evidence_id"]
                )
                if item["question_id"] != previous["question_id"] or any(
                    item["source"][k] != previous["source"][k]
                    for k in previous["source"]
                    if k != "rank"
                ):
                    raise ValueError(ERROR)
            if len(old) == len(event["evidence"]):
                raise ValueError(ERROR)
            state["evidence"] = event["evidence"]
        elif kind == "final_delta":
            if not any(
                a["agent_id"] == event["agent_id"]
                and a["kind"] == "finalizer"
                and a["status"] == "running"
                for a in state["agents"]
            ):
                raise ValueError(ERROR)
            self.final_text += event["delta"]
            validate_type(self.final_text, "reply")
        elif kind == "done":
            if (
                self.started_tools
                or (self.final_text and self.final_text != event["reply"])
                or any(
                    q["status"] not in ("completed", "failed", "cancelled")
                    for q in state["questions"] + state["agents"]
                )
                or not any(
                    a["kind"] == "finalizer" and a["status"] == "completed"
                    for a in state["agents"]
                )
            ):
                raise ValueError(ERROR)
            if [s["rank"] for s in event["sources"]] != list(
                range(1, len(event["sources"]) + 1)
            ):
                raise ValueError(ERROR)
            keys = [(s["paper_id"], s["chunk_index"]) for s in event["sources"]]
            if len(keys) != len(set(keys)) or any(
                not any(
                    all(s[k] == e["source"][k] for k in s if k != "rank")
                    for e in state["evidence"]
                )
                for s in event["sources"]
            ):
                raise ValueError(ERROR)
        validate_type(state, "snapshot")
        self._snapshot()
