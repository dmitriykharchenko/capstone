"""Offline stage-resume and deferred-round contracts with real pipeline helpers."""

import asyncio
import json
from collections import Counter
from concurrent.futures import CancelledError

import pytest

from src.chat.planner_retry import PlannerRetryPolicy
from test_evaluation_runner import EvaluationRunner, FakeModel, case, chunks

PRIVATE = "synthetic-private-stage-failure"


def stage_keys(initial_count):
    return [
        ("planner", None),
        *[
            (stage, hop)
            for hop in range(1, initial_count + 1)
            for stage in ("retrieval", "subanswer")
        ],
        ("coverage", None),
        *[
            (stage, hop)
            for hop in range(initial_count + 1, initial_count + 3)
            for stage in ("followup_retrieval", "followup_subanswer")
        ],
        ("composer", None),
        ("judge", None),
    ]


STAGES = stage_keys(2)


class Faults:
    def __init__(self, failures=None, error_type=RuntimeError, initial_count=2):
        self.failures = Counter(failures or {})
        self.counts = Counter()
        self.calls = []
        self.error_type = error_type
        self.initial_count = initial_count

    def visit(self, question, stage, hop=None):
        key = (question, stage, hop)
        self.calls.append(key)
        self.counts[key] += 1
        if self.failures[key]:
            self.failures[key] -= 1
            raise self.error_type(PRIVATE)


def hop_key(query, retrieval=False, initial_count=2):
    followup = " followup " in query
    question, index = query.rsplit(" followup " if followup else " hop ", 1)
    stage = "retrieval" if retrieval else "subanswer"
    return (
        question,
        ("followup_" if followup else "") + stage,
        int(index) + (initial_count if followup else 0),
    )


class Model(FakeModel):
    def __init__(self, faults):
        super().__init__(hop_count=faults.initial_count)
        self.faults = faults

    def invoke(self, messages):
        text = "\n".join(message.content for message in messages)
        if messages[0].content.startswith("You are an evaluation judge"):
            question = messages[-1].content.split("USER_QUERY: ", 1)[1].splitlines()[0]
            self.faults.visit(question, "judge")
        elif "MULTIHOP_PLANNER" in text:
            self.faults.visit(messages[-1].content, "planner")
        else:
            payload = json.loads(messages[-1].content)
            question = payload["original_question"]
            if "MULTIHOP_SUBANSWER" in text:
                self.faults.visit(
                    *hop_key(payload["subquery"], initial_count=self.hop_count)
                )
            elif "MULTIHOP_COVERAGE" in text:
                self.faults.visit(question, "coverage")
                self.followups = (f"{question} followup 1", f"{question} followup 2")
            else:
                self.faults.visit(question, "composer")
        return super().invoke(messages)


class Retriever:
    def __init__(self, faults):
        self.faults = faults

    def retrieve_chunks(self, query, n_results=5):
        assert n_results >= 5
        self.faults.visit(
            *hop_key(query, retrieval=True, initial_count=self.faults.initial_count)
        )
        return chunks(query)


def runner(failures=None, error_type=RuntimeError, initial_count=2):
    faults = Faults(failures, error_type, initial_count)
    return EvaluationRunner(
        Retriever(faults),
        Model(faults),
        planner_retry_policy=PlannerRetryPolicy(
            sleep=lambda _: pytest.fail("unexpected delay")
        ),
    ), faults


@pytest.mark.parametrize(("stage", "hop"), STAGES)
@pytest.mark.parametrize(
    "error_type",
    [KeyboardInterrupt, asyncio.CancelledError, CancelledError, BrokenPipeError],
)
def test_terminal_signals_never_become_deferred_case_failures(stage, hop, error_type):
    evaluation, faults = runner({("q", stage, hop): 1}, error_type)
    with pytest.raises(error_type):
        evaluation.run([case("q"), case("later")], "hybrid")
    assert all(q == "q" for q, _, _ in faults.calls)
    assert faults.counts[("q", stage, hop)] == 1
