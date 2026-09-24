import json
import os
import re
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path
from uuid import UUID

import pytest

from src import evaluation_results
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

    published = persist_evaluation_results(target, **artifact_kwargs())

    assert published.parent == target.parent
    assert not target.exists()
    contents = published.read_text(encoding="utf-8")
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


def test_completed_runs_retain_all_artifacts_without_temp_files(tmp_path):
    target = tmp_path / "eval_results.json"
    target.write_text("legacy", encoding="utf-8")
    first = persist_evaluation_results(target, **artifact_kwargs("first"))
    previous = first.read_bytes()
    second = persist_evaluation_results(target, **artifact_kwargs("second"))
    assert first != second
    assert first.read_bytes() == previous
    assert target.read_text(encoding="utf-8") == "legacy"
    assert json.loads(second.read_text())["results"][0]["generated_answer"] == "second"
    assert set(tmp_path.iterdir()) == {target, first, second}


def test_atomic_publication_failure_preserves_last_valid_artifact(
    monkeypatch, tmp_path
):
    target = tmp_path / "eval_results.json"
    previous = '{"schema":"last-valid"}\n'
    target.write_text(previous, encoding="utf-8")

    def fail_link(_source, _target, **_kwargs):
        raise OSError("publication failed")

    monkeypatch.setattr(os, "link", fail_link)
    with pytest.raises(OSError, match="publication failed"):
        persist_evaluation_results(target, **artifact_kwargs())

    assert target.read_text(encoding="utf-8") == previous
    assert list(tmp_path.iterdir()) == [target]


def freeze_identity(monkeypatch, *, microsecond=123456, identifier=1):
    class Clock:
        @staticmethod
        def now(tz):
            assert tz is timezone.utc
            return datetime(2026, 9, 24, 18, 2, 3, microsecond, tzinfo=tz)

    monkeypatch.setattr(evaluation_results, "datetime", Clock)
    monkeypatch.setattr(evaluation_results, "uuid4", lambda: UUID(int=identifier))


@pytest.mark.parametrize(
    "template", ["eval_results.json", "custom.output.JSON", "no_suffix"]
)
def test_deterministic_safe_name_preserves_template(monkeypatch, tmp_path, template):
    freeze_identity(monkeypatch)
    source = tmp_path / template
    actual = persist_evaluation_results(source, **artifact_kwargs())
    assert actual.name == (
        f"{source.stem}_2026-09-24T18-02-03.123456Z_"
        f"00000000-0000-0000-0000-000000000001{source.suffix}"
    )
    assert re.fullmatch(r"[A-Za-z0-9_.-]+", actual.name)
    assert len(actual.name.encode()) <= 255


def test_same_timestamp_distinct_ids_and_lexical_utc_order(monkeypatch, tmp_path):
    template = tmp_path / "eval_results.json"
    names = []
    for micros, identifier in [(123457, 1), (123456, 2), (123456, 3)]:
        freeze_identity(monkeypatch, microsecond=micros, identifier=identifier)
        names.append(persist_evaluation_results(template, **artifact_kwargs()))
    assert len(set(names)) == 3
    assert sorted(names) == [names[1], names[2], names[0]]
    assert set(tmp_path.iterdir()) == set(names)


@pytest.mark.parametrize("kind", ["file", "symlink", "hardlink", "directory"])
def test_exact_collision_never_touches_existing_entry(monkeypatch, tmp_path, kind):
    freeze_identity(monkeypatch)
    template = tmp_path / "eval_results.json"
    target = persist_evaluation_results(template, **artifact_kwargs("prior"))
    prior = target.read_bytes()
    other = tmp_path / "other"
    target.rename(other)
    if kind == "file":
        target.write_bytes(prior)
    elif kind == "symlink":
        target.symlink_to(other)
    elif kind == "hardlink":
        target.hardlink_to(other)
    else:
        target.mkdir()
    metadata = target.lstat()
    with pytest.raises(FileExistsError):
        persist_evaluation_results(template, **artifact_kwargs("next"))
    assert target.lstat() == metadata
    assert other.read_bytes() == prior
    assert set(tmp_path.iterdir()) == {target, other}


def test_complete_synced_file_is_visible_only_at_publication(monkeypatch, tmp_path):
    template = tmp_path / "eval_results.json"
    real_link, real_fsync = os.link, os.fsync
    synced = []

    def sync(fd):
        real_fsync(fd)
        synced.append(True)

    def publish(source, target, **kwargs):
        assert not target.exists()
        assert synced == [True]
        assert json.loads(source.read_text()) == evaluation_results_artifact(
            **artifact_kwargs()
        )
        real_link(source, target, **kwargs)

    monkeypatch.setattr(os, "fsync", sync)
    monkeypatch.setattr(os, "link", publish)
    actual = persist_evaluation_results(template, **artifact_kwargs())
    assert list(tmp_path.iterdir()) == [actual]


@pytest.mark.parametrize("failure", ["serialize", "fsync", "interrupt"])
def test_prepublication_failures_leave_no_artifact_or_staging(
    monkeypatch, tmp_path, failure
):
    def fail(*args, **kwargs):
        if failure == "interrupt":
            raise KeyboardInterrupt()
        raise ValueError("injected failure")

    if failure == "serialize":
        monkeypatch.setattr(evaluation_results, "serialize_evaluation_results", fail)
    else:
        monkeypatch.setattr(os, "fsync", fail)
    with pytest.raises(KeyboardInterrupt if failure == "interrupt" else ValueError):
        persist_evaluation_results(tmp_path / "eval_results.json", **artifact_kwargs())
    assert list(tmp_path.iterdir()) == []


@pytest.mark.parametrize("same_identifier", [False, True])
def test_concurrent_publishers_never_replace_each_other(
    monkeypatch, tmp_path, same_identifier
):
    from uuid import uuid4

    freeze_identity(monkeypatch)
    if not same_identifier:
        monkeypatch.setattr(evaluation_results, "uuid4", uuid4)

    def publish(answer):
        try:
            return persist_evaluation_results(
                tmp_path / "eval_results.json", **artifact_kwargs(answer)
            )
        except FileExistsError:
            return None

    with ThreadPoolExecutor(max_workers=2) as pool:
        paths = list(pool.map(publish, ["first", "second"]))
    published = [p for p in paths if p is not None]
    assert len(published) == (1 if same_identifier else 2)
    assert set(tmp_path.iterdir()) == set(published)
    for answer, path in zip(["first", "second"], paths):
        if path is not None:
            assert (
                json.loads(path.read_text())["results"][0]["generated_answer"] == answer
            )


def test_maximum_component_length_and_too_long_template_fail_without_staging(tmp_path):
    target = tmp_path / ("r" * 185 + ".json")
    published = persist_evaluation_results(target, **artifact_kwargs())
    assert len(published.name.encode()) == 255
    with pytest.raises(OSError):
        persist_evaluation_results(
            tmp_path / ("r" * 186 + ".json"), **artifact_kwargs()
        )
    assert list(tmp_path.iterdir()) == [published]
