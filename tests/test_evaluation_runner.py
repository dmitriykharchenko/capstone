import json

import pytest

from src.agent_loop import AgentLoop
from src.evaluation_runner import (
    JUDGE_CRITERIA,
    EvaluationRunner,
    EvaluationValidationError,
    JudgeOutputError,
    parse_judge_output,
)
from src.paper_search import PaperChunkMatch


def chunks(question="question"):
    return [
        PaperChunkMatch(
            rank=index,
            text=f"full chunk {index} for {question}\nsecond line",
            score=1.0 / index,
            paper_id=f"{question}-paper-{index}",
            chunk_index=index - 1,
            filename=f"{question}-source-{index}.pdf",
            title=f"{question} Source {index}",
            page=index,
        )
        for index in range(1, 6)
    ]


def case(question="Question one", notes="Reference one", sources=None):
    return {
        "question": question,
        "grading_notes": notes,
        "sources": ["source-1.pdf"] if sources is None else sources,
    }


def judge_payload(score=4):
    result = {}
    extras = {
        "relevant_chunk_ranking": ("relevant_chunk_ranks", [1, 3]),
        "context_utilization": ("chunks_used", [1, "chunk 3"]),
        "semantic_similarity": ("contradicting_claims", []),
        "completeness": ("missing_elements", []),
        "hallucination_rate": ("unsupported_claims", []),
    }
    for criterion in JUDGE_CRITERIA:
        result[criterion] = {
            "score": score,
            "confidence": "high",
            "reasoning": f"Evidence for {criterion}.",
            "flags": [],
        }
        if criterion in extras:
            key, value = extras[criterion]
            result[criterion][key] = value
    return result


class FakeRetriever:
    def __init__(self, failures=None):
        self.calls = []
        self.failures = set(failures or ())

    def retrieve_chunks(self, query, n_results=5):
        self.calls.append((query, n_results))
        if any(question in query for question in self.failures):
            raise RuntimeError(f"retrieval failed for {query}")
        return chunks(query)


class FakeResponse:
    def __init__(self, content):
        self.content = content


class FakeModel:
    def __init__(
        self,
        malformed_judges=None,
        answer_failures=None,
        stage_failures=None,
        hop_count=2,
    ):
        self.calls = []
        self.malformed_judges = set(malformed_judges or ())
        self.answer_failures = set(answer_failures or ())
        self.stage_failures = dict(stage_failures or {})
        self.hop_count = hop_count

    def invoke(self, messages):
        self.calls.append(messages)
        contents = "\n".join(message.content for message in messages)
        if messages[0].content.startswith("You are an evaluation judge"):
            for question in self.malformed_judges:
                if f"USER_QUERY: {question}" in messages[-1].content:
                    return FakeResponse("```json\n{}\n```")
            return FakeResponse(json.dumps(judge_payload()))
        if "MULTIHOP_PLANNER" in contents:
            question = messages[-1].content
            if self.stage_failures.get(question) == "planner":
                raise RuntimeError(f"planner failed for {question}")
            return FakeResponse(
                json.dumps(
                    {
                        "subqueries": [
                            f"{question} hop {index}"
                            for index in range(1, self.hop_count + 1)
                        ]
                    }
                )
            )
        payload = json.loads(messages[-1].content)
        if "MULTIHOP_SUBANSWER" in contents:
            if self.stage_failures.get(payload["original_question"]) == "subanswer":
                raise RuntimeError(
                    f"subanswer failed for {payload['original_question']}"
                )
            return FakeResponse(f"summary for {payload['subquery']}")
        if self.stage_failures.get(payload["original_question"]) == "composer":
            raise RuntimeError(f"composer failed for {payload['original_question']}")
        for question in self.answer_failures:
            if payload["original_question"] == question:
                raise RuntimeError(f"model failed for {question}")
        return FakeResponse(f"answer for {payload['original_question']}")


def model_stages(model):
    stages = []
    for messages in model.calls:
        contents = "\n".join(message.content for message in messages)
        if messages[0].content.startswith("You are an evaluation judge"):
            stages.append("judge")
        elif "MULTIHOP_PLANNER" in contents:
            stages.append("planner")
        elif "MULTIHOP_SUBANSWER" in contents:
            stages.append("subanswer")
        else:
            stages.append("composer")
    return stages


@pytest.mark.parametrize("strategy", ["vector", "bm25", "hybrid"])
def test_existing_strategy_values_all_use_multihop_default_retrieval(strategy):
    retriever = FakeRetriever()
    model = FakeModel()

    result = EvaluationRunner(retriever, model).run([case()], strategy)

    assert retriever.calls == [("Question one hop 1", 5), ("Question one hop 2", 5)]
    assert len(result[0]["retrieved_chunks"]) == 10
    assert [item["rank"] for item in result[0]["retrieved_chunks"]] == list(
        range(1, 11)
    )
    assert result[0]["error"] is None
    assert model_stages(model) == [
        "planner",
        "subanswer",
        "subanswer",
        "composer",
        "judge",
    ]


def test_rejects_invalid_strategy_before_any_calls():
    retriever = FakeRetriever()
    model = FakeModel()

    with pytest.raises(EvaluationValidationError, match="strategy must be one of"):
        EvaluationRunner(retriever, model).run([case()], "dense")

    assert retriever.calls == []
    assert model.calls == []


def test_records_empty_hop_as_retrieval_error_before_composer_or_judge():
    retriever = FakeRetriever()
    model = FakeModel()

    def retrieve_with_empty_second_hop(query, n_results=5):
        retriever.calls.append((query, n_results))
        return [] if query.endswith("hop 2") else chunks(query)

    retriever.retrieve_chunks = retrieve_with_empty_second_hop
    result = EvaluationRunner(retriever, model).run([case()], "vector")

    assert result[0]["error"]["stage"] == "retrieval"
    assert result[0]["retrieved_chunks"] == []
    assert model_stages(model) == ["planner", "subanswer"]


@pytest.mark.parametrize(
    "invalid",
    [
        [],
        {},
        [case(), {"question": " ", "grading_notes": "notes", "sources": []}],
        [case(), {"question": "q", "grading_notes": "", "sources": []}],
        [case(), {"question": "q", "grading_notes": "notes", "sources": "paper"}],
        [case(), {"question": "q", "grading_notes": "notes", "sources": [""]}],
    ],
)
def test_validates_entire_array_before_any_retrieval_or_model_call(invalid):
    retriever = FakeRetriever()
    model = FakeModel()

    with pytest.raises(EvaluationValidationError):
        EvaluationRunner(retriever, model).run(json.dumps(invalid), "vector")

    assert retriever.calls == []
    assert model.calls == []


def test_judge_prompt_uses_runtime_template_and_exact_field_mapping():
    retriever = FakeRetriever()
    model = FakeModel()
    source_case = case(
        question="Unique question",
        notes="Unique grading notes",
        sources=["alpha.pdf", "beta.pdf"],
    )

    EvaluationRunner(retriever, model).run([source_case], "hybrid")

    runtime = open("tests/judge.md", encoding="utf-8").read()
    system_section = runtime.split("\nUSER PROMPT (fill per example):\n", maxsplit=1)[0]
    judge_system = model.calls[-1][0].content
    judge_user = model.calls[-1][1].content
    assert judge_system == system_section.removeprefix("SYSTEM PROMPT:\n").strip("\n")
    assert "USER_QUERY: Unique question" in judge_user
    assert 'GROUND_TRUTH_RELEVANT: ["alpha.pdf", "beta.pdf"]' in judge_user
    assert "GENERATED_ANSWER: answer for Unique question" in judge_user
    assert "REFERENCE_ANSWER: Unique grading notes" in judge_user
    assert all(f"{index}. full chunk" in judge_user for index in range(1, 11))
    assert "{{" not in judge_user
    assert "\n...\n" not in judge_user


def test_empty_sources_map_to_not_provided():
    model = FakeModel()
    EvaluationRunner(FakeRetriever(), model).run([case(sources=[])], "vector")
    assert "GROUND_TRUTH_RELEVANT: NOT PROVIDED" in model.calls[-1][1].content


def test_cases_and_model_calls_execute_in_strict_sequential_order():
    retriever = FakeRetriever()
    model = FakeModel()
    inputs = [case("first"), case("second"), case("third")]

    results = EvaluationRunner(retriever, model).run(inputs, "bm25")

    assert [call[0] for call in retriever.calls] == [
        "first hop 1",
        "first hop 2",
        "second hop 1",
        "second hop 2",
        "third hop 1",
        "third hop 2",
    ]
    assert [result["case_index"] for result in results] == [1, 2, 3]
    assert model_stages(model) == [
        "planner",
        "subanswer",
        "subanswer",
        "composer",
        "judge",
        "planner",
        "subanswer",
        "subanswer",
        "composer",
        "judge",
        "planner",
        "subanswer",
        "subanswer",
        "composer",
        "judge",
    ]


def test_retrieval_and_model_failures_are_recorded_and_later_cases_continue():
    retriever = FakeRetriever(failures={"retrieval failure"})
    model = FakeModel(answer_failures={"model failure"})
    inputs = [
        case("retrieval failure"),
        case("model failure"),
        case("success"),
    ]

    results = EvaluationRunner(retriever, model).run(inputs, "vector")

    assert [
        result["error"]["stage"] if result["error"] else None for result in results
    ] == [
        "retrieval",
        "composer",
        None,
    ]
    assert results[2]["generated_answer"] == "answer for success"
    assert results[2]["judge_scores"]["context_precision"]["score"] == 4


@pytest.mark.parametrize("stage", ["planner", "subanswer", "composer"])
def test_each_multihop_model_failure_stage_is_explicit_and_later_cases_continue(stage):
    model = FakeModel(stage_failures={"failure": stage})

    results = EvaluationRunner(FakeRetriever(), model).run(
        [case("failure"), case("success")], "hybrid"
    )

    assert results[0]["error"]["stage"] == stage
    assert results[0]["generated_answer"] is None
    assert results[1]["error"] is None


def test_malformed_judge_is_recorded_without_discarding_answer_or_later_case():
    model = FakeModel(malformed_judges={"bad judge"})

    results = EvaluationRunner(FakeRetriever(), model).run(
        [case("bad judge"), case("good judge")], "hybrid"
    )

    assert results[0]["generated_answer"] == "answer for bad judge"
    assert results[0]["judge_scores"] is None
    assert results[0]["error"]["stage"] == "judge"
    assert results[1]["error"] is None


def test_valid_judge_json_and_aggregation_ready_result_schema():
    parsed = parse_judge_output(json.dumps(judge_payload(score=5)))
    assert tuple(parsed) == JUDGE_CRITERIA

    result = EvaluationRunner(FakeRetriever(), FakeModel()).run([case()], "vector")[0]
    assert set(result) == {
        "case_index",
        "input",
        "retrieval_strategy",
        "retrieved_chunks",
        "generated_answer",
        "judge_scores",
        "error",
    }
    assert [result["judge_scores"][name]["score"] for name in JUDGE_CRITERIA] == [4] * 7
    json.dumps(result)


def test_five_hops_aggregate_and_judge_all_twenty_five_chunks():
    model = FakeModel(hop_count=5)

    result = EvaluationRunner(FakeRetriever(), model).run([case()], "hybrid")[0]

    assert len(result["retrieved_chunks"]) == 25
    assert [chunk["rank"] for chunk in result["retrieved_chunks"]] == list(range(1, 26))
    assert "25. full chunk" in model.calls[-1][-1].content
    valid = judge_payload()
    valid["relevant_chunk_ranking"]["relevant_chunk_ranks"] = [25]
    assert parse_judge_output(json.dumps(valid))["relevant_chunk_ranking"][
        "relevant_chunk_ranks"
    ] == [25]
    valid["relevant_chunk_ranking"]["relevant_chunk_ranks"] = [26]
    with pytest.raises(JudgeOutputError):
        parse_judge_output(json.dumps(valid))


def invalid_judge_outputs():
    missing = judge_payload()
    missing.pop("context_recall")
    extra_root = judge_payload()
    extra_root["other"] = {}
    extra_field = judge_payload()
    extra_field["context_precision"]["unexpected"] = []
    bad_score = judge_payload()
    bad_score["context_precision"]["score"] = 3.5
    missing_extra = judge_payload()
    missing_extra["completeness"].pop("missing_elements")
    bad_ranks = judge_payload()
    bad_ranks["relevant_chunk_ranking"]["relevant_chunk_ranks"] = [0]
    return [missing, extra_root, extra_field, bad_score, missing_extra, bad_ranks]


@pytest.mark.parametrize("invalid", invalid_judge_outputs())
def test_rejects_incomplete_or_invalid_judge_schema(invalid):
    with pytest.raises(JudgeOutputError):
        parse_judge_output(json.dumps(invalid))


@pytest.mark.parametrize("text", ["not json", "```json\n{}\n```", '{"x": NaN}'])
def test_rejects_malformed_or_nonstandard_judge_json(text):
    with pytest.raises(JudgeOutputError):
        parse_judge_output(text)


def test_stateless_calls_do_not_use_chat_persistence(monkeypatch):
    def persistence_was_called(*_args, **_kwargs):
        raise AssertionError("evaluation must not write chat persistence")

    monkeypatch.setattr(
        "src.sqlite_history.SQLiteHistoryStore.append_exchange",
        persistence_was_called,
    )
    monkeypatch.setattr(
        "src.sqlite_history.SQLiteHistoryStore.get_messages",
        persistence_was_called,
    )
    monkeypatch.setattr(
        "src.chat_history_index.ChatHistoryIndex.index_exchange",
        persistence_was_called,
    )
    model = FakeModel()

    result = EvaluationRunner(FakeRetriever(), model).run([case()], "vector")

    assert result[0]["error"] is None
    assert model_stages(model) == [
        "planner",
        "subanswer",
        "subanswer",
        "composer",
        "judge",
    ]


def test_chat_and_evaluation_share_calls_answer_and_aggregate_sources(tmp_path):
    question = "Compare systems"
    chat_model = FakeModel()
    chat_retriever = FakeRetriever()
    with AgentLoop(
        tmp_path / "chat.sqlite3", chat_model, paper_retriever=chat_retriever
    ) as chat:
        chat_answer = chat.run("session", question)
        chat_sources = chat.last_sources

    evaluation_model = FakeModel()
    evaluation_retriever = FakeRetriever()
    evaluation = EvaluationRunner(evaluation_retriever, evaluation_model).run(
        [case(question)], "hybrid"
    )[0]

    assert chat_retriever.calls == evaluation_retriever.calls
    assert chat_answer == evaluation["generated_answer"]
    assert [
        (source.rank, source.paper_id, source.chunk_index) for source in chat_sources
    ] == [
        (chunk["rank"], chunk["paper_id"], chunk["chunk_index"])
        for chunk in evaluation["retrieved_chunks"]
    ]
    chat_pipeline_calls = [
        [message.content for message in messages] for messages in chat_model.calls
    ]
    evaluation_pipeline_calls = [
        [message.content for message in messages]
        for messages in evaluation_model.calls[:-1]
    ]
    assert chat_pipeline_calls == evaluation_pipeline_calls
