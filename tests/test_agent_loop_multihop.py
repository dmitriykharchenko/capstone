import json
from types import SimpleNamespace

import pytest

from src.agent_loop import (
    AgentCoverageError,
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
        "coverage",
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
    ("followups", "expected"),
    [
        (["  gamma   evidence  "], ["gamma evidence"]),
        (
            ["gamma evidence", "delta evidence"],
            ["gamma evidence", "delta evidence"],
        ),
    ],
)
def test_adaptive_followups_use_exact_hop_path_and_compose_in_order(
    tmp_path, followups, expected
):
    model = StageModel()
    model.coverage = json.dumps({"complete": False, "follow_up_subqueries": followups})
    hop_results = results()
    for query in expected:
        hop_results[query] = [chunk(1, "shared"), chunk(2, query)]
    retriever = HopRetriever(hop_results)

    with AgentLoop(
        tmp_path / "adaptive.sqlite3", model, paper_retriever=retriever
    ) as loop:
        assert loop.run("session", "question") == "final answer"
        source_ids = [source.paper_id for source in loop.last_sources]

    all_queries = ["alpha evidence", "beta evidence", *expected]
    assert retriever.calls == [(query, 5) for query in all_queries]
    assert source_ids == ["shared", "alpha", "beta", *expected]
    assert [stage for stage, _ in model.calls] == [
        "planner",
        "subanswer",
        "subanswer",
        "coverage",
        *("subanswer" for _ in expected),
        "composer",
    ]
    coverage = json.loads(model.calls[3][1][-1].content)
    assert [item["subquery"] for item in coverage["subanswers"]] == all_queries[:2]
    composition = json.loads(model.calls[-1][1][-1].content)
    assert [item["subquery"] for item in composition["subanswers"]] == all_queries
    assert [item["answer"] for item in composition["subanswers"]] == [
        f"summary for {query}" for query in all_queries
    ]


def test_seven_hops_cap_aggregate_at_thirty_five_deterministic_chunks(tmp_path):
    initial = [f"initial {index}" for index in range(1, 6)]
    followups = ["followup one", "followup two"]
    model = StageModel(json.dumps({"subqueries": initial}))
    model.coverage = json.dumps({"complete": False, "follow_up_subqueries": followups})
    hop_results = {
        query: [chunk(rank, f"{query}-{rank}", rank - 1) for rank in range(1, 6)]
        for query in [*initial, *followups]
    }

    with AgentLoop(
        tmp_path / "bounds.sqlite3",
        model,
        paper_retriever=HopRetriever(hop_results),
    ) as loop:
        loop.run("session", "question")
        sources = loop.last_sources

    assert len(sources) == 35
    assert [source.rank for source in sources] == list(range(1, 36))
    assert [source.paper_id for source in sources[:6]] == [
        "initial 1-1",
        "initial 1-2",
        "initial 1-3",
        "initial 1-4",
        "initial 1-5",
        "initial 2-1",
    ]


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
