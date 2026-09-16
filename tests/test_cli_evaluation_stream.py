import io
import json

import pytest

from src.cli import __main__ as cli_main
from src.cli.commands import handle_evaluation_stream
from src.cli.config import CLIConfig
from src.evaluation_runner import (
    JUDGE_CRITERIA,
    EvaluationValidationError,
)


@pytest.fixture
def evaluation_results_path(tmp_path):
    return tmp_path / "eval_results.json"


def config(tmp_path):
    return CLIConfig(
        database_path=tmp_path / "history.db",
        chroma_directory=tmp_path / "chroma",
        papers_directory=tmp_path / "papers",
        chat_model="chat-model",
        chat_base_url="https://example.test/v1",
        embedding_model="embed-model",
        embedding_base_url="https://example.test/v1",
        api_key=None,
        system_prompt=None,
        log_path=tmp_path / "chat.log",
        retrieval_log_path=tmp_path / "retrieval.log",
    )


def test_streams_ordered_success_failure_and_aggregate_scores(
    tmp_path, evaluation_results_path
):
    cases = [
        {"question": "first", "grading_notes": "one", "sources": ["a.pdf"]},
        {"question": "second", "grading_notes": "two", "sources": ["b.pdf"]},
        {"question": "third", "grading_notes": "three", "sources": []},
    ]

    class FakeRunner:
        def run(self, payload, strategy):
            source = payload[0]
            index = cases.index(source) + 1
            error = (
                {"stage": "judge", "message": "invalid judge JSON"}
                if index == 2
                else None
            )
            scores = (
                None
                if error
                else {
                    criterion: {
                        "score": index,
                        "confidence": "high",
                        "reasoning": "ok",
                        "flags": [],
                    }
                    for criterion in JUDGE_CRITERIA
                }
            )
            return [
                {
                    "case_index": 1,
                    "input": source,
                    "retrieval_strategy": strategy,
                    "retrieved_chunks": [],
                    "generated_answer": f"answer {index}",
                    "judge_scores": scores,
                    "error": error,
                }
            ]

    events = list(
        cli_main._persisted_evaluation_stream(
            handle_evaluation_stream(
                config(tmp_path),
                {"retrieval_type": "hybrid", "test_cases": cases},
                FakeRunner(),
            ),
            results_path=evaluation_results_path,
        )
    )

    assert [event["event"] for event in events] == [
        "run_start",
        "case_start",
        "case_success",
        "case_start",
        "case_failure",
        "case_start",
        "case_success",
        "complete",
    ]
    assert [
        event["input"] for event in events if event["event"] == "case_start"
    ] == cases
    results = [
        event["result"]
        for event in events
        if event["event"].startswith("case_") and "result" in event
    ]
    assert [result["case_index"] for result in results] == [1, 2, 3]
    assert results[1]["error"]["stage"] == "judge"
    done = events[-1]
    assert (done["total"], done["succeeded"], done["failed"]) == (3, 2, 1)
    assert list(done["aggregate_scores"]) == list(JUDGE_CRITERIA)
    assert set(done["aggregate_scores"].values()) == {2.0}
    artifact = json.loads(evaluation_results_path.read_text(encoding="utf-8"))
    assert artifact["schema"] == "rag-evaluation-results"
    assert artifact["schema_version"] == 1
    assert artifact["metadata"] == {
        "retrieval_identifier": "multihop-hybrid",
        "retrieval_strategy": "hybrid",
    }
    assert artifact["input_cases"] == cases
    assert artifact["results"] == results
    assert artifact["aggregate_scores"] == done["aggregate_scores"]
    assert artifact["summary"] == {"total": 3, "succeeded": 2, "failed": 1}


def test_case_exception_is_streamed_without_aborting_following_cases(tmp_path):
    class FakeRunner:
        calls = 0

        def run(self, payload, strategy):
            self.calls += 1
            if self.calls == 1:
                raise RuntimeError("backend unavailable")
            return [
                {
                    "case_index": 1,
                    "input": payload[0],
                    "retrieval_strategy": strategy,
                    "retrieved_chunks": [],
                    "generated_answer": None,
                    "judge_scores": None,
                    "error": {"stage": "retrieval", "message": "no chunks"},
                }
            ]

    cases = [
        {"question": "one", "grading_notes": "a", "sources": []},
        {"question": "two", "grading_notes": "b", "sources": []},
    ]
    events = list(
        handle_evaluation_stream(
            config(tmp_path),
            {"retrieval_type": "vector", "test_cases": cases},
            FakeRunner(),
        )
    )
    failures = [event["result"] for event in events if event["event"] == "case_failure"]
    assert [result["case_index"] for result in failures] == [1, 2]
    assert failures[0]["error"] == {
        "stage": "evaluation",
        "message": "Evaluation failed for this case. Check backend configuration and retry.",
    }
    assert failures[1]["error"] == {
        "stage": "retrieval",
        "message": "Retrieval failed for this case. Check the paper index and retry.",
    }
    assert events[-1]["aggregate_scores"] == {
        criterion: None for criterion in JUDGE_CRITERIA
    }


def test_validation_and_backend_transport_failures_preserve_last_artifact(
    monkeypatch, tmp_path, evaluation_results_path
):
    previous = '{"schema":"last-valid"}\n'
    evaluation_results_path.write_text(previous, encoding="utf-8")

    with pytest.raises(EvaluationValidationError):
        list(
            cli_main._persisted_evaluation_stream(
                handle_evaluation_stream(
                    config(tmp_path),
                    {"retrieval_type": "hybrid", "test_cases": []},
                ),
                results_path=evaluation_results_path,
            )
        )
    assert evaluation_results_path.read_text(encoding="utf-8") == previous

    def unavailable(_config):
        raise RuntimeError("process unavailable")

    monkeypatch.setattr("src.cli.commands.build_evaluation_runner", unavailable)
    with pytest.raises(RuntimeError, match="Evaluation backend is unavailable"):
        list(
            cli_main._persisted_evaluation_stream(
                handle_evaluation_stream(
                    config(tmp_path),
                    {
                        "retrieval_type": "hybrid",
                        "test_cases": [
                            {"question": "q", "grading_notes": "g", "sources": []}
                        ],
                    },
                ),
                results_path=evaluation_results_path,
            )
        )
    assert evaluation_results_path.read_text(encoding="utf-8") == previous


def test_persists_only_after_terminal_event_is_successfully_consumed(
    tmp_path, evaluation_results_path
):
    previous = '{"schema":"last-valid"}\n'
    evaluation_results_path.write_text(previous, encoding="utf-8")

    class FakeRunner:
        def run(self, payload, strategy):
            return [
                {
                    "case_index": 1,
                    "input": payload[0],
                    "retrieval_strategy": strategy,
                    "retrieved_chunks": [],
                    "generated_answer": None,
                    "judge_scores": None,
                    "error": {"stage": "judge", "message": "failed"},
                }
            ]

    events = cli_main._persisted_evaluation_stream(
        handle_evaluation_stream(
            config(tmp_path),
            {
                "retrieval_type": "hybrid",
                "test_cases": [{"question": "q", "grading_notes": "g", "sources": []}],
            },
            FakeRunner(),
        ),
        results_path=evaluation_results_path,
    )
    terminal = None
    while terminal is None or terminal["event"] != "complete":
        terminal = next(events)
    assert evaluation_results_path.read_text(encoding="utf-8") == previous

    with pytest.raises(StopIteration):
        next(events)
    assert json.loads(evaluation_results_path.read_text(encoding="utf-8"))[
        "summary"
    ] == {
        "failed": 1,
        "succeeded": 0,
        "total": 1,
    }


def test_sanitizes_path_and_credential_shaped_case_errors(tmp_path):
    cases = [
        {"question": "one", "grading_notes": "a", "sources": []},
        {"question": "two", "grading_notes": "b", "sources": []},
    ]

    class FakeRunner:
        calls = 0

        def run(self, payload, strategy):
            self.calls += 1
            if self.calls == 2:
                raise RuntimeError("api_key=secret at /private/secret/tests/judge.md")
            return [
                {
                    "case_index": 1,
                    "input": payload[0],
                    "retrieval_strategy": strategy,
                    "retrieved_chunks": [],
                    "generated_answer": None,
                    "judge_scores": None,
                    "error": {
                        "stage": "judge",
                        "message": "token=secret at /private/secret/tests/judge.md",
                    },
                }
            ]

    events = list(
        handle_evaluation_stream(
            config(tmp_path),
            {"retrieval_type": "vector", "test_cases": cases},
            FakeRunner(),
        )
    )
    failures = [event for event in events if event["event"] == "case_failure"]

    assert failures[0]["result"]["error"] == {
        "stage": "judge",
        "message": "Judge evaluation failed for this case. Check backend configuration and retry.",
    }
    assert failures[1]["result"]["error"]["stage"] == "evaluation"
    assert "/private/secret" not in json.dumps(events)
    assert "api_key" not in json.dumps(events)


@pytest.mark.parametrize(
    ("stage", "message"),
    [
        (
            "planner",
            "Question planning failed for this case. Check backend configuration and retry.",
        ),
        (
            "subanswer",
            "Subanswer generation failed for this case. Check backend configuration and retry.",
        ),
        (
            "coverage",
            "Coverage review failed for this case. Check backend configuration and retry.",
        ),
        (
            "followup_retrieval",
            "Follow-up retrieval failed for this case. Check the paper index and retry.",
        ),
        (
            "followup_subanswer",
            "Follow-up subanswer generation failed for this case. Check backend configuration and retry.",
        ),
        (
            "composer",
            "Final answer composition failed for this case. Check backend configuration and retry.",
        ),
    ],
)
def test_preserves_multihop_failure_stage_and_continues(stage, message, tmp_path):
    cases = [
        {"question": "failure", "grading_notes": "a", "sources": []},
        {"question": "success", "grading_notes": "b", "sources": []},
    ]

    class FakeRunner:
        def run(self, payload, strategy):
            failed = payload[0]["question"] == "failure"
            scores = (
                None
                if failed
                else {
                    criterion: {
                        "score": 4,
                        "confidence": "high",
                        "reasoning": "ok",
                        "flags": [],
                    }
                    for criterion in JUDGE_CRITERIA
                }
            )
            return [
                {
                    "case_index": 1,
                    "input": payload[0],
                    "retrieval_strategy": strategy,
                    "retrieved_chunks": [],
                    "generated_answer": None if failed else "answer",
                    "judge_scores": scores,
                    "error": {"stage": stage, "message": "secret"} if failed else None,
                }
            ]

    events = list(
        handle_evaluation_stream(
            config(tmp_path),
            {"retrieval_type": "hybrid", "test_cases": cases},
            FakeRunner(),
        )
    )
    results = [event["result"] for event in events if "result" in event]

    assert results[0]["error"] == {"stage": stage, "message": message}
    assert results[1]["error"] is None
    assert events[-1]["succeeded"] == 1
    assert events[-1]["failed"] == 1


def test_cli_main_routes_evaluation_stream(monkeypatch, tmp_path):
    expected = [{"event": "run_start", "retrieval_type": "bm25", "total": 1}]
    monkeypatch.setattr(cli_main.CLIConfig, "from_env", lambda: config(tmp_path))
    monkeypatch.setattr(cli_main, "load_dotenv_if_present", lambda: None)
    monkeypatch.setattr(
        cli_main, "handle_evaluation_stream", lambda cfg, request: iter(expected)
    )
    monkeypatch.setattr(
        cli_main.sys,
        "stdin",
        io.StringIO(json.dumps({"command": "evaluate", "stream": True})),
    )
    output = io.StringIO()
    monkeypatch.setattr(cli_main.sys, "stdout", output)

    assert cli_main.main() == 0
    assert [json.loads(line) for line in output.getvalue().splitlines()] == expected


def test_cli_main_sanitizes_evaluation_transport_exception(monkeypatch, tmp_path):
    def failing_stream(config, request):
        raise RuntimeError("api_key=secret at /private/secret/tests/judge.md")
        yield

    monkeypatch.setattr(cli_main.CLIConfig, "from_env", lambda: config(tmp_path))
    monkeypatch.setattr(cli_main, "load_dotenv_if_present", lambda: None)
    monkeypatch.setattr(cli_main, "handle_evaluation_stream", failing_stream)
    monkeypatch.setattr(
        cli_main.sys,
        "stdin",
        io.StringIO(json.dumps({"command": "evaluate", "stream": True})),
    )
    output = io.StringIO()
    monkeypatch.setattr(cli_main.sys, "stdout", output)

    assert cli_main.main() == 1
    assert json.loads(output.getvalue()) == {
        "ok": False,
        "error": "Evaluation transport failed. Check backend logs and retry.",
    }
    assert "secret" not in output.getvalue()
