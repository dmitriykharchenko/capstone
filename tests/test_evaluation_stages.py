"""Offline stage-resume and deferred-round contracts with real pipeline helpers."""

import asyncio
import json
from collections import Counter
from concurrent.futures import CancelledError
from dataclasses import replace
from types import SimpleNamespace

import pytest

from src.cli import __main__ as cli_main
from src.cli.commands import handle_evaluation_stream
from src.evaluation_runner import EvaluationRunner, validate_test_cases
from src.planner_retry import PlannerRetryPolicy
from test_evaluation_runner import FakeModel, case, chunks

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


@pytest.mark.parametrize(
    ("initial_count", "stage", "hop"),
    [(count, stage, hop) for count in (2, 5) for stage, hop in stage_keys(count)],
)
@pytest.mark.parametrize("failures", [1, 2, 3])
def test_every_stage_and_hop_resumes_without_replaying_validated_work(
    initial_count, stage, hop, failures
):
    evaluation, faults = runner(
        {("q", stage, hop): failures}, initial_count=initial_count
    )
    state = evaluation.start_case(1, validate_test_cases([case("q")])[0], "hybrid")
    previous = None
    for retry_round in range(3):
        result = evaluation.resume_case(state, retry_round)
        if previous is not None:
            assert previous["error"] is not None
        if result["error"] is None:
            break
        assert (state.stages.stage, state.stages.hop) == (stage, hop)
        assert PRIVATE not in repr(state)
        previous = result
    stages = stage_keys(initial_count)
    failure_index = stages.index((stage, hop))
    completed = stages if failures < 3 else stages[: failure_index + 1]
    assert faults.counts == Counter(
        {
            ("q", name, index): min(failures + 1, 3)
            if (name, index) == (stage, hop)
            else 1
            for name, index in completed
        }
    )
    assert len(state.failure_history) == failures
    assert [item["retry_round"] for item in state.failure_history] == list(
        range(failures)
    )
    assert [item["stage_attempt"] for item in state.failure_history] == list(
        range(1, failures + 1)
    )
    assert (result["error"] is None) == (failures < 3)
    if failures < 3:
        calls = list(faults.calls)
        assert evaluation.resume_case(state, 2) == result
        assert faults.calls == calls


def test_advancing_failure_resumes_new_stage_on_next_round_and_preserves_history():
    evaluation, faults = runner({("q", "subanswer", 2): 1, ("q", "judge", None): 1})
    state = evaluation.start_case(1, validate_test_cases([case("q")])[0], "hybrid")
    first = evaluation.resume_case(state, 0)
    assert state.stages.stage == "subanswer"
    context = state.stages.contexts
    second = evaluation.resume_case(state, 1)
    assert state.stages.stage == "judge"
    assert state.stages.contexts[:2] == context
    answer = state.stages.answer
    third = evaluation.resume_case(state, 2)
    assert first["error"]["stage"] == "subanswer"
    assert second["error"]["stage"] == "judge" and second["generated_answer"] == answer
    assert third["error"] is None and third["generated_answer"] == answer
    assert [item["stage"] for item in state.failure_history] == ["subanswer", "judge"]
    assert faults.counts[("q", "composer", None)] == 1
    assert faults.counts[("q", "retrieval", 2)] == 1


def test_direct_runner_finishes_initial_pass_before_two_failed_only_ordered_rounds():
    evaluation, faults = runner({("a", "judge", None): 3, ("c", "coverage", None): 1})
    results = evaluation.run([case(q) for q in ("a", "b", "c")], "hybrid")
    assert [result["input"]["question"] for result in results] == ["a", "b", "c"]
    transitions = [
        q
        for i, (q, _, _) in enumerate(faults.calls)
        if i == 0 or q != faults.calls[i - 1][0]
    ]
    assert transitions == ["a", "b", "c", "a", "c", "a"]
    assert [result["error"] is None for result in results] == [False, True, True]


def test_stream_rounds_publish_exactly_one_final_ordered_artifact(tmp_path):
    evaluation, faults = runner({("a", "judge", None): 3, ("c", "coverage", None): 1})
    inputs = [case(q) for q in ("a", "b", "c")]
    events = list(
        cli_main._persisted_evaluation_stream(
            handle_evaluation_stream(
                None, {"test_cases": inputs, "retrieval_type": "hybrid"}, evaluation
            ),
            results_path=tmp_path / "results.json",
        )
    )
    assert [e["event"] for e in events[:7]] == [
        "run_start",
        "case_start",
        "case_failure",
        "case_start",
        "case_success",
        "case_start",
        "case_failure",
    ]
    rounds = [e for e in events if e["event"] == "retry_round_start"]
    assert [(e["retry_round"], e["pending"]) for e in rounds] == [(1, 2), (2, 1)]
    assert [e["case_index"] for e in events if e["event"] == "case_retry_start"] == [
        1,
        3,
        1,
    ]
    assert all(e.get("completed", 0) <= 3 for e in events)
    assert sum(e["event"] == "complete" for e in events) == 1
    assert events[-1]["succeeded"] == 2 and events[-1]["failed"] == 1
    assert set(events[-1]["aggregate_scores"].values()) == {4.0}
    files = list(tmp_path.glob("results_*.json"))
    assert len(files) == 1
    artifact = json.loads(files[0].read_text())
    assert artifact["input_cases"] == inputs
    assert [item["case_index"] for item in artifact["results"]] == [1, 2, 3]
    assert [item["error"] is None for item in artifact["results"]] == [
        False,
        True,
        True,
    ]
    assert PRIVATE not in json.dumps(events) + files[0].read_text()
    assert faults.counts[("a", "composer", None)] == 1


@pytest.mark.parametrize("cut", range(13))
def test_close_at_every_retry_boundary_does_not_prefetch_or_publish(tmp_path, cut):
    evaluation, faults = runner({("q", "judge", None): 3})
    events = cli_main._persisted_evaluation_stream(
        handle_evaluation_stream(
            None, {"test_cases": [case("q")], "retrieval_type": "hybrid"}, evaluation
        ),
        results_path=tmp_path / "results.json",
    )
    frames = []
    for _ in range(cut):
        try:
            frames.append(next(events))
        except StopIteration:
            break
    calls = list(faults.calls)
    events.close()
    assert faults.calls == calls
    assert len(list(tmp_path.iterdir())) == int(
        any(e["event"] == "complete" for e in frames)
    )


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


@pytest.mark.parametrize("retry_round", [-1, 1, 2, 3, True, 0.0])
def test_out_of_order_or_invalid_rounds_are_rejected_without_work(retry_round):
    evaluation, faults = runner()
    state = evaluation.start_case(1, validate_test_cases([case("q")])[0], "hybrid")
    with pytest.raises(ValueError):
        evaluation.resume_case(state, retry_round)
    assert faults.calls == []


@pytest.mark.parametrize(("stage", "hop"), STAGES[1:])
def test_invalid_outputs_are_not_checkpointed_and_resume_same_stage(stage, hop):
    evaluation, faults = runner()
    invoke = evaluation._model.invoke
    retrieve = evaluation._retriever.retrieve_chunks
    rejected = []

    def corrupt_model(messages):
        response = invoke(messages)
        if faults.calls[-1] == ("q", stage, hop) and not rejected:
            rejected.append(True)
            return SimpleNamespace(
                content="" if "subanswer" in stage or stage == "composer" else "{}"
            )
        return response

    def corrupt_retrieval(query, n_results=5):
        result = retrieve(query, n_results)
        if faults.calls[-1] == ("q", stage, hop) and not rejected:
            rejected.append(True)
            return [replace(result[0], score=float("nan"))]
        return result

    evaluation._model.invoke = corrupt_model
    evaluation._retriever.retrieve_chunks = corrupt_retrieval
    state = evaluation.start_case(1, validate_test_cases([case("q")])[0], "hybrid")
    first = evaluation.resume_case(state, 0)
    assert first["error"]["stage"] == stage
    assert (state.stages.stage, state.stages.hop) == (stage, hop)
    assert not any(
        str(chunk.score) == "nan"
        for group in state.stages.hop_chunks
        for chunk in group
    )
    assert evaluation.resume_case(state, 1)["error"] is None
    assert faults.counts == Counter(
        {
            ("q", name, index): 2 if (name, index) == (stage, hop) else 1
            for name, index in STAGES
        }
    )


def test_no_round_three_and_new_run_starts_fresh():
    evaluation, faults = runner({("q", "judge", None): 3})
    state = evaluation.start_case(1, validate_test_cases([case("q")])[0], "hybrid")
    for retry_round in range(3):
        assert evaluation.resume_case(state, retry_round)["error"] is not None
    calls = list(faults.calls)
    with pytest.raises(ValueError):
        evaluation.resume_case(state, 3)
    assert faults.calls == calls
    assert evaluation.run([case("q")], "hybrid")[0]["error"] is None
    assert faults.counts[("q", "planner", None)] == 2


@pytest.mark.parametrize("error_type", [CancelledError, BrokenPipeError])
def test_terminal_failure_during_retry_stream_has_no_artifact(tmp_path, error_type):
    evaluation, faults = runner({("q", "judge", None): 3})
    source = handle_evaluation_stream(
        None, {"test_cases": [case("q")], "retrieval_type": "hybrid"}, evaluation
    )
    events = cli_main._persisted_evaluation_stream(
        source, results_path=tmp_path / "results.json"
    )
    assert [next(events)["event"] for _ in range(5)] == [
        "run_start",
        "case_start",
        "case_failure",
        "retry_round_start",
        "case_retry_start",
    ]
    faults.error_type = error_type
    from src.cli.runtime import CommandRuntimeError

    with pytest.raises(CommandRuntimeError):
        next(events)
    assert list(events) == [] and source.gi_frame is None
    assert list(tmp_path.iterdir()) == []
    assert faults.counts[("q", "judge", None)] == 2


def test_stream_validates_judge_template_before_any_provider_call(tmp_path):
    faults = Faults()
    evaluation = EvaluationRunner(
        Retriever(faults), Model(faults), judge_prompt_path=tmp_path / "missing.md"
    )
    events = list(
        handle_evaluation_stream(
            None, {"test_cases": [case("q")], "retrieval_type": "hybrid"}, evaluation
        )
    )
    assert faults.calls == []
    assert events[-1]["failed"] == 1
    assert str(tmp_path) not in json.dumps(events)
