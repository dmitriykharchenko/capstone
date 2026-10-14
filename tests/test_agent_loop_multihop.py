import json
import time
from types import SimpleNamespace

import pytest

from src.chat.agent_loop import (
    AgentCoverageError,
    AgentLoop,
    AgentModelError,
    AgentRetrievalError,
    run_multihop,
)
from src.retrieval.paper_search import PaperChunkMatch
from src.storage.sqlite_history import SQLiteHistoryStore

def chunk(rank, paper_id, chunk_index=0):
    return PaperChunkMatch(
        rank=rank,
        text=f"evidence from {paper_id}",
        score=1 / rank,
        paper_id=paper_id,
        chunk_index=chunk_index,
        filename=f"{paper_id}.pdf",
        title=paper_id.title(),
        page=chunk_index + 1,
    )


class HopRetriever:
    def __init__(self, results):
        self.results = results
        self.calls = []

    def retrieve_chunks(self, query, n_results=5):
        self.calls.append((query, n_results))
        result = self.results[query]
        if isinstance(result, Exception):
            raise result
        return result[:n_results]


class StageModel:
    def __init__(self, plan='{"subqueries":["alpha evidence","beta evidence"]}'):
        self.plan = plan
        self.coverage = '{"complete":true,"follow_up_subqueries":[]}'
        self.calls = []
        self.stream_calls = []
        self.fail_stage = None
        self.final_chunks = ["final ", "answer"]

    @staticmethod
    def stage(messages):
        content = "\n".join(str(message.content) for message in messages)
        for stage in ("PLANNER", "SUBANSWER", "COVERAGE", "COMPOSER"):
            if f"MULTIHOP_{stage}" in content:
                return stage.lower()
        raise AssertionError("missing multihop stage marker")

    def invoke(self, messages):
        stage = self.stage(messages)
        self.calls.append((stage, list(messages)))
        if stage == self.fail_stage:
            raise RuntimeError(f"{stage} offline")
        if stage == "planner":
            return SimpleNamespace(content=self.plan)
        if stage == "coverage":
            return SimpleNamespace(content=self.coverage)
        payload = json.loads(messages[-1].content)
        if stage == "subanswer":
            return SimpleNamespace(content=f"summary for {payload['subquery']}")
        return SimpleNamespace(content="final answer")

    def stream(self, messages):
        assert self.stage(messages) == "composer"
        self.stream_calls.append(list(messages))
        for content in self.final_chunks:
            if isinstance(content, Exception):
                raise content
            if hasattr(content, "content"):
                yield content
            else:
                yield SimpleNamespace(content=content, response_metadata={})


def results():
    return {
        "alpha evidence": [chunk(1, "shared"), chunk(2, "alpha")],
        "beta evidence": [chunk(1, "shared"), chunk(2, "beta")],
    }


def test_parallel_answer_failure_cancels_searches_that_have_not_started():
    queries = ["first", "second", "third", "fourth", "fifth"]

    class FailingModel(StageModel):
        def __init__(self):
            super().__init__(json.dumps({"subqueries": queries}))
            self.started = []

        def invoke(self, messages):
            stage = self.stage(messages)
            if stage != "subanswer":
                return super().invoke(messages)
            query = json.loads(messages[-1].content)["subquery"]
            self.started.append(query)
            if query == "first":
                raise RuntimeError("first search failed")
            time.sleep(0.03)
            return SimpleNamespace(content=f"summary for {query}")

    model = FailingModel()
    retriever = HopRetriever({query: [chunk(1, query)] for query in queries})

    with pytest.raises(AgentModelError) as raised:
        run_multihop(model, retriever, "question")

    assert raised.value.stage == "subanswer"
    assert "fifth" not in model.started


@pytest.mark.parametrize(
    "coverage",
    [
        "not json",
        "{}",
        '{"complete":true,"follow_up_subqueries":["gap"]}',
        '{"complete":false,"follow_up_subqueries":[]}',
        '{"complete":false,"follow_up_subqueries":["gap","GAP"]}',
        '{"complete":false,"follow_up_subqueries":["alpha evidence"]}',
        '{"complete":false,"follow_up_subqueries":["a","b","c"]}',
        '{"complete":true,"complete":false,"follow_up_subqueries":[]}',
    ],
)
def test_invalid_or_duplicate_coverage_fails_before_followup_and_persistence(
    tmp_path, coverage
):
    database = tmp_path / "invalid-coverage.sqlite3"
    model = StageModel()
    model.coverage = coverage
    retriever = HopRetriever(results())
    with AgentLoop(database, model, paper_retriever=retriever) as loop:
        with pytest.raises(AgentCoverageError) as raised:
            loop.run("session", "question")
    assert raised.value.stage == "coverage"
    assert retriever.calls == [("alpha evidence", 5), ("beta evidence", 5)]
    with SQLiteHistoryStore(database) as store:
        assert store.get_messages("session") == []


@pytest.mark.parametrize(
    ("failure", "expected_stage"),
    [
        ("coverage", "coverage"),
        ("followup_retrieval", "followup_retrieval"),
        ("followup_subanswer", "followup_subanswer"),
    ],
)
def test_adaptive_failure_stages_are_explicit_and_never_persist(
    tmp_path, failure, expected_stage
):
    database = tmp_path / f"{failure}.sqlite3"
    model = StageModel()
    model.coverage = json.dumps(
        {"complete": False, "follow_up_subqueries": ["gap evidence"]}
    )
    hop_results = results()
    hop_results["gap evidence"] = [chunk(1, "gap")]
    if failure == "coverage":
        model.fail_stage = "coverage"
    elif failure == "followup_retrieval":
        hop_results["gap evidence"] = RuntimeError("secret index path")
    else:
        original_invoke = model.invoke

        def fail_followup(messages):
            if model.stage(messages) == "subanswer":
                payload = json.loads(messages[-1].content)
                if payload["subquery"] == "gap evidence":
                    raise RuntimeError("secret model detail")
            return original_invoke(messages)

        model.invoke = fail_followup
    with AgentLoop(database, model, paper_retriever=HopRetriever(hop_results)) as loop:
        with pytest.raises((AgentModelError, AgentRetrievalError)) as raised:
            loop.run("session", "question")
    assert raised.value.stage == expected_stage
    with SQLiteHistoryStore(database) as store:
        assert store.get_messages("session") == []
