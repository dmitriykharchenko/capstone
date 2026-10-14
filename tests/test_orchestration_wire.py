"""Shared raw-JSON and state-machine regressions; no stores or providers."""

from copy import deepcopy
import json
from pathlib import Path

import pytest

from src.orchestration.wire import (
    ERROR,
    StreamValidator,
    loads,
)

FIXTURES = Path(__file__).parent / "fixtures"
CASES = json.loads((FIXTURES / "orchestration-wire-cases.json").read_text())
GOLDEN = [
    json.loads(line)
    for line in (FIXTURES / "orchestration.ndjson").read_text().splitlines()
]


def update(value, changes):
    for pointer, replacement in changes.get("set", {}).items():
        keys = pointer.strip("/").split("/")
        target = value
        for key in keys[:-1]:
            target = target[int(key)] if isinstance(target, list) else target[key]
        key = int(keys[-1]) if isinstance(target, list) else keys[-1]
        target[key] = deepcopy(replacement)
    for key in changes.get("remove", []):
        del value[key]
    return value


def stream_case(case):
    events = deepcopy(GOLDEN[: case.get("prefix", 0)])
    if "snapshot" in case:
        setup = case["snapshot"]
        validator = StreamValidator("session", "run", "turn")
        for event in deepcopy(GOLDEN[: setup["after"]]):
            validator.accept(event)
        state = update(validator.snapshot, setup)
        state["checkpoint_sequence"] = GOLDEN[setup["after"] - 1]["sequence"] // 1024
        events = [
            dict(
                GOLDEN[0],
                snapshot=state,
                sequence=state["checkpoint_sequence"] * 1024 + 900,
            )
        ]
    for step in case["steps"]:
        changes = {"base": step} if type(step) is int else step
        event = update(deepcopy(GOLDEN[changes["base"]]), changes)
        event["sequence"] = events[-1]["sequence"] + 1
        events.append(event)
    return events


def accepts(raw):
    validator = StreamValidator("session", "run", "turn")
    try:
        for line in raw:
            validator.accept(loads(line))
    except ValueError as error:
        assert str(error) == ERROR
        error_event = dict(
            GOLDEN[0],
            event="error",
            sequence=validator.sequence + 1,
            code="invalid_stream",
            message=ERROR,
            resumable=True,
        )
        del error_event["snapshot"]
        if not validator.terminal:
            validator.accept(error_event)
            assert validator.terminal
        return False
    return True


@pytest.mark.parametrize("case", CASES["streams"], ids=lambda c: c["name"])
def test_shared_stream_cases(case):
    assert accepts([json.dumps(event) for event in stream_case(case)]) == case["valid"]
