import json
import os
from pathlib import Path

import pytest

from src.evaluation_results import (
    DEFAULT_EVALUATION_RESULTS_PATH,
    evaluation_results_artifact,
    persist_evaluation_results,
    serialize_evaluation_results,
)


def test_default_artifact_is_beside_evaluation_questions():
    expected_questions = Path(__file__).with_name("eval_questions.json").resolve()
    assert DEFAULT_EVALUATION_RESULTS_PATH == expected_questions.with_name(
        "eval_results.json"
    )


def artifact_kwargs(answer="answer"):
    input_case = {"question": "café?", "grading_notes": "yes", "sources": []}
    result = {
        "case_index": 1,
        "input": input_case,
        "retrieval_strategy": "hybrid",
        "retrieved_chunks": [],
        "generated_answer": answer,
        "judge_scores": None,
        "error": {"stage": "judge", "message": "failed"},
    }
    return {
        "input_cases": [input_case],
        "retrieval_strategy": "hybrid",
        "results": [result],
        "aggregate_scores": {"context_precision": None},
        "succeeded": 0,
        "failed": 1,
    }


def test_persists_utf8_json_and_creates_parent_directory(tmp_path):
    target = tmp_path / "nested" / "eval_results.json"

    persist_evaluation_results(target, **artifact_kwargs())

    contents = target.read_text(encoding="utf-8")
    assert "café" in contents
    assert contents.endswith("\n")
    assert json.loads(contents) == evaluation_results_artifact(**artifact_kwargs())


def test_serialization_is_deterministic_and_rejects_nonfinite_numbers():
    forward = {"z": {"b": 2, "a": 1}, "a": "café"}
    reverse = {"a": "café", "z": {"a": 1, "b": 2}}

    assert serialize_evaluation_results(forward) == serialize_evaluation_results(
        reverse
    )
    with pytest.raises(ValueError):
        serialize_evaluation_results({"score": float("nan")})


def test_completed_runs_replace_one_artifact_without_temp_files(tmp_path):
    target = tmp_path / "eval_results.json"
    persist_evaluation_results(target, **artifact_kwargs("first"))

    persist_evaluation_results(target, **artifact_kwargs("second"))

    artifact = json.loads(target.read_text(encoding="utf-8"))
    assert artifact["results"][0]["generated_answer"] == "second"
    assert list(tmp_path.iterdir()) == [target]


def test_atomic_replace_failure_preserves_last_valid_artifact(monkeypatch, tmp_path):
    target = tmp_path / "eval_results.json"
    previous = '{"schema":"last-valid"}\n'
    target.write_text(previous, encoding="utf-8")

    def fail_replace(_source, _target):
        raise OSError("replace failed")

    monkeypatch.setattr(os, "replace", fail_replace)
    with pytest.raises(OSError, match="replace failed"):
        persist_evaluation_results(target, **artifact_kwargs())

    assert target.read_text(encoding="utf-8") == previous
    assert list(tmp_path.iterdir()) == [target]
