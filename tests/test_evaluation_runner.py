import json

import pytest

from src.evaluation.runner import (
    JUDGE_CRITERIA,
    EvaluationRunner,
    EvaluationValidationError,
    JudgeOutputError,
    parse_judge_output,
)
from src.retrieval.paper_search import PaperChunkMatch

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
        followups=(),
    ):
        self.calls = []
        self.malformed_judges = set(malformed_judges or ())
        self.answer_failures = set(answer_failures or ())
        self.stage_failures = dict(stage_failures or {})
        self.hop_count = hop_count
        self.followups = tuple(followups)

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
        if "MULTIHOP_COVERAGE" in contents:
            payload = json.loads(messages[-1].content)
            question = payload["original_question"]
            if self.stage_failures.get(question) == "coverage":
                raise RuntimeError(f"coverage failed for {question}")
            return FakeResponse(
                json.dumps(
                    {
                        "complete": not self.followups,
                        "follow_up_subqueries": list(self.followups),
                    }
                )
            )
        payload = json.loads(messages[-1].content)
        if "MULTIHOP_SUBANSWER" in contents:
            failure = self.stage_failures.get(payload["original_question"])
            if failure == "subanswer" or (
                failure == "followup_subanswer"
                and payload["subquery"] in self.followups
            ):
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
        elif "MULTIHOP_COVERAGE" in contents:
            stages.append("coverage")
        else:
            stages.append("composer")
    return stages


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


@pytest.mark.parametrize(
    "budget",
    [
        {},
        [],
        {"max_retrievals": -1},
        {"max_retrievals": None},
        {"unknown": 1},
        {"max_rag_model_calls": 1.5},
        {"max_retrievals": True},
    ],
)
def test_invalid_performance_budget_rejects_whole_input(budget):
    with pytest.raises(EvaluationValidationError, match="performance_budget"):
        EvaluationRunner(FakeRetriever(), FakeModel()).run(
            [{**case(), "performance_budget": budget}], "hybrid"
        )


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
