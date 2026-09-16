import json
from types import SimpleNamespace

import pytest

from src.agent_loop import (
    AgentIncompleteResponseError,
    AgentLoop,
    AgentModelError,
    AgentPlanError,
    AgentRetrievalError,
    MultihopCompletedEvent,
    MultihopFinalDeltaEvent,
)
from src.cli.models import MockChatModel
from src.paper_search import PaperChunkMatch
from src.sqlite_history import SQLiteHistoryStore


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
        return result


class StageModel:
    def __init__(self, plan='{"subqueries":["alpha evidence","beta evidence"]}'):
        self.plan = plan
        self.calls = []
        self.stream_calls = []
        self.fail_stage = None
        self.final_chunks = ["final ", "answer"]

    @staticmethod
    def stage(messages):
        content = "\n".join(str(message.content) for message in messages)
        for stage in ("PLANNER", "SUBANSWER", "COMPOSER"):
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


def test_run_retrieves_each_hop_composes_all_answers_and_persists_only_final(tmp_path):
    database = tmp_path / "history.sqlite3"
    model = StageModel()
    retriever = HopRetriever(results())
    with AgentLoop(database, model, paper_retriever=retriever) as loop:
        assert loop.run("session", "compare alpha and beta") == "final answer"
        assert [source.paper_id for source in loop.last_sources] == [
            "shared",
            "alpha",
            "beta",
        ]

    assert retriever.calls == [("alpha evidence", 5), ("beta evidence", 5)]
    assert [stage for stage, _ in model.calls] == [
        "planner",
        "subanswer",
        "subanswer",
        "composer",
    ]
    composition = json.loads(model.calls[-1][1][-1].content)
    assert composition["original_question"] == "compare alpha and beta"
    assert [item["answer"] for item in composition["subanswers"]] == [
        "summary for alpha evidence",
        "summary for beta evidence",
    ]
    with SQLiteHistoryStore(database) as store:
        messages = store.get_messages("session")
    assert [(message.role, message.content) for message in messages] == [
        ("user", "compare alpha and beta"),
        ("assistant", "final answer"),
    ]
    assert [source.rank for source in messages[-1].sources] == [1, 2, 3]


def test_stream_events_are_ordered_typed_and_persist_after_completion(tmp_path):
    model = StageModel()
    database = tmp_path / "history.sqlite3"
    with AgentLoop(database, model, paper_retriever=HopRetriever(results())) as loop:
        events = list(loop.stream_events("session", "question"))

    assert [event.stage for event in events] == [
        "plan",
        "hop_started",
        "hop_completed",
        "hop_started",
        "hop_completed",
        "final_delta",
        "final_delta",
        "completed",
    ]
    assert [
        event.text for event in events if isinstance(event, MultihopFinalDeltaEvent)
    ] == [
        "final ",
        "answer",
    ]
    assert isinstance(events[-1], MultihopCompletedEvent)
    with SQLiteHistoryStore(database) as store:
        assert [item.content for item in store.get_messages("session")] == [
            "question",
            "final answer",
        ]


@pytest.mark.parametrize(
    "plan",
    [
        "",
        "not json",
        "{}",
        '{"subqueries":[]}',
        '{"subqueries":["same"," SAME "]}',
        '{"subqueries":["1","2","3","4","5","6"]}',
    ],
)
def test_invalid_plans_fail_before_retrieval_or_persistence(tmp_path, plan):
    database = tmp_path / "history.sqlite3"
    retriever = HopRetriever(results())
    with AgentLoop(database, StageModel(plan), paper_retriever=retriever) as loop:
        with pytest.raises((AgentPlanError, AgentModelError)):
            loop.run("session", "question")
    assert retriever.calls == []
    with SQLiteHistoryStore(database) as store:
        assert store.get_messages("session") == []


@pytest.mark.parametrize("failure", ["retrieval", "subanswer", "composer"])
def test_hop_and_model_failures_never_persist_partial_exchange(tmp_path, failure):
    database = tmp_path / f"{failure}.sqlite3"
    model = StageModel()
    model.fail_stage = failure if failure != "retrieval" else None
    hop_results = results()
    if failure == "retrieval":
        hop_results["beta evidence"] = RuntimeError("index offline")
    with AgentLoop(database, model, paper_retriever=HopRetriever(hop_results)) as loop:
        error = AgentRetrievalError if failure == "retrieval" else AgentModelError
        with pytest.raises(error):
            loop.run("session", "question")
    with SQLiteHistoryStore(database) as store:
        assert store.get_messages("session") == []


def test_incomplete_or_interrupted_final_stream_is_not_persisted(tmp_path):
    for name, chunks in (
        (
            "incomplete",
            [
                SimpleNamespace(
                    content="partial", response_metadata={"finish_reason": "length"}
                )
            ],
        ),
        ("interrupted", ["partial ", RuntimeError("disconnected")]),
    ):
        database = tmp_path / f"{name}.sqlite3"
        model = StageModel()
        model.final_chunks = chunks
        with AgentLoop(
            database, model, paper_retriever=HopRetriever(results())
        ) as loop:
            with pytest.raises((AgentIncompleteResponseError, AgentModelError)):
                list(loop.stream_events("session", "question"))
        with SQLiteHistoryStore(database) as store:
            assert store.get_messages("session") == []


def test_abandoned_event_stream_does_not_persist_partial_exchange(tmp_path):
    database = tmp_path / "abandoned.sqlite3"
    with AgentLoop(
        database, StageModel(), paper_retriever=HopRetriever(results())
    ) as loop:
        stream = loop.stream_events("session", "question")
        assert next(stream).stage == "plan"
        stream.close()
    with SQLiteHistoryStore(database) as store:
        assert store.get_messages("session") == []


def test_offline_mock_model_supports_every_multihop_stage(tmp_path):
    with AgentLoop(
        tmp_path / "mock.sqlite3",
        MockChatModel(),
        paper_retriever=HopRetriever(
            {
                "Evidence supporting question": [chunk(1, "support")],
                "Evidence limiting or contrasting question": [chunk(1, "limit")],
            }
        ),
    ) as loop:
        answer = loop.run("session", "question")
    assert answer == "[offline mock reply] question"
