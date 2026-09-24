"""Shared raw-JSON and state-machine regressions; no stores or providers."""

from copy import deepcopy
from itertools import permutations
import json
from pathlib import Path

import pytest

from src.orchestration_wire import (
    ERROR,
    INTEGER_FIELDS,
    QUESTION_TRANSITIONS,
    SCHEMA,
    StreamValidator,
    loads,
    validate_event,
    validate_type,
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


def test_shared_integer_lexing():
    assert {key for key, _ in CASES["integer_fields"]} == INTEGER_FIELDS
    assert {name for _, name in CASES["integer_fields"]} == {
        name for name, rule in SCHEMA["types"].items() if rule["kind"] == "int"
    }
    for key, name in CASES["integer_fields"]:
        rule = SCHEMA["types"][name]
        bounds = [
            {"raw": str(n), "integer": n}
            for n in (rule["min"], rule["max"], rule["max"] + 1)
        ]
        for case in CASES["lexical"] + bounds:
            expected = (
                "integer" in case and rule["min"] <= case["integer"] <= rule["max"]
            )
            for encoded_key in (
                json.dumps(key),
                '"\\u%04x%s"' % (ord(key[0]), key[1:]),
            ):
                raw = '{"nested":[{' + encoded_key + ":" + case["raw"] + "}]}"
                try:
                    validate_type(loads(raw)["nested"][0][key], name)
                    actual = True
                except ValueError as error:
                    assert str(error) == ERROR
                    actual = False
                assert actual == expected, (key, name, case)
    for raw in ("-0", "-1", "0.5", "1e2"):
        validate_type(loads('{"score":' + raw + "}")["score"], "score")


@pytest.mark.parametrize("case", CASES["streams"], ids=lambda c: c["name"])
def test_shared_stream_cases(case):
    assert accepts([json.dumps(event) for event in stream_case(case)]) == case["valid"]


def test_shared_question_transition_table():
    assert QUESTION_TRANSITIONS == {
        key: set(values) for key, values in CASES["question_transitions"].items()
    }
    for order in permutations([6, 7, 10]):
        case = {"prefix": 6, "steps": order}
        assert accepts([json.dumps(e) for e in stream_case(case)]) == (
            order == (6, 7, 10)
        )


def integer_mutations():
    """Raw NDJSON mutations also reused by the standalone cross-language gate."""
    for position, event in enumerate(GOLDEN):

        def leaves(value, path=()):
            if type(value) is int:
                yield path
            elif isinstance(value, dict):
                for key, child in value.items():
                    yield from leaves(child, (*path, key))
            elif isinstance(value, list):
                for index, child in enumerate(value):
                    yield from leaves(child, (*path, str(index)))

        for path in leaves(event):
            for case in CASES["lexical"]:
                if "integer" in case:
                    continue
                changed = update(
                    deepcopy(event), {"set": {"/" + "/".join(path): "@integer@"}}
                )
                raw = json.dumps(changed).replace('"@integer@"', case["raw"])
                yield [json.dumps(e) for e in GOLDEN[:position]] + [raw]


def test_raw_ndjson_integer_mutations():
    for events in integer_mutations():
        assert not accepts(events)


def test_done_establishes_identity_without_mutating_fixture():
    validator = StreamValidator("session", "run", "turn")
    for event in deepcopy(GOLDEN):
        validator.accept(event)
    assert validator.snapshot["status"] == "completed"
    assert validator.snapshot["exchange_id"] == GOLDEN[-1]["exchange_id"]
    assert GOLDEN[0]["snapshot"]["exchange_id"] is None
    validate_event(GOLDEN[-1])
