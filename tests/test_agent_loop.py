import json
from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from langchain_core.messages import AIMessage, HumanMessage, SystemMessage

import src.chat.agent_loop as agent_loop_module
from src.chat.agent_loop import (
    AgentConfigurationError,
    AgentIncompleteResponseError,
    AgentLoop,
    ChatOpenAIConfig,
)
from src.retrieval.paper_search import PaperChunkMatch
from src.storage.sqlite_history import SQLiteHistoryStore

def model_stage(messages):
    content = "\n".join(str(message.content) for message in messages)
    for stage in ("PLANNER", "SUBANSWER", "COVERAGE", "COMPOSER"):
        if f"MULTIHOP_{stage}" in content:
            return stage.lower()
    raise AssertionError("missing multihop stage marker")


class FakeChatModel:
    def __init__(self, answers):
        self.answers = iter(answers)
        self.calls = []
        self.hop_calls = []

    def invoke(self, messages):
        stage = model_stage(messages)
        if stage == "planner":
            question = messages[-1].content
            return AIMessage(
                content=json.dumps(
                    {"subqueries": [f"{question} aspect 1", f"{question} aspect 2"]}
                )
            )
        if stage == "subanswer":
            self.hop_calls.append(list(messages))
            return AIMessage(content="evidence summary")
        if stage == "coverage":
            return AIMessage(content='{"complete":true,"follow_up_subqueries":[]}')
        self.calls.append(list(messages))
        return AIMessage(content=next(self.answers))


def paper_chunks(count=5):
    return [
        PaperChunkMatch(
            rank=index,
            text=f"full paper text {index}",
            score=1.0 / index,
            paper_id=f"paper-{index}",
            chunk_index=index - 1,
            filename=f"paper-{index}.pdf",
            title=f"Paper {index}",
            page=index,
        )
        for index in range(1, count + 1)
    ]


class FakeRetriever:
    def __init__(self, chunks=None):
        self.chunks = paper_chunks() if chunks is None else chunks
        self.calls = []

    def retrieve_chunks(self, query, n_results=5):
        self.calls.append((query, n_results))
        return self.chunks[:n_results]


def test_first_and_subsequent_turns_include_history_and_persist(tmp_path):
    model = FakeChatModel(["first answer", "second answer"])
    retriever = FakeRetriever()
    with AgentLoop(
        tmp_path / "history.sqlite3",
        model=model,
        system_prompt="Be precise",
        paper_retriever=retriever,
    ) as loop:
        assert loop.run("session", "first question") == "first answer"
        assert loop.run("session", "follow-up") == "second answer"

    first, second = model.calls
    assert isinstance(first[0], SystemMessage)
    assert isinstance(first[-1], HumanMessage)
    assert first[0].content == "Be precise"
    assert json.loads(first[-1].content)["original_question"] == "first question"
    assert [message.content for message in second] == [
        "Be precise",
        "first question",
        "first answer",
        second[-2].content,
        second[-1].content,
    ]
    assert json.loads(second[-1].content)["original_question"] == "follow-up"
    assert retriever.calls == [
        ("first question aspect 1", 5),
        ("first question aspect 2", 5),
        ("follow-up aspect 1", 5),
        ("follow-up aspect 2", 5),
    ]
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        assert [(m.role, m.content) for m in store.get_messages("session")] == [
            ("user", "first question"),
            ("assistant", "first answer"),
            ("user", "follow-up"),
            ("assistant", "second answer"),
        ]
        assert [source.rank for source in store.get_messages("session")[1].sources] == [
            1,
            2,
            3,
            4,
            5,
        ]


def test_configuration_validation_and_missing_key(tmp_path, monkeypatch):
    with pytest.raises(AgentConfigurationError, match="model"):
        ChatOpenAIConfig(model=" ")
    with AgentLoop(
        tmp_path / "history.sqlite3", paper_retriever=FakeRetriever()
    ) as loop:
        monkeypatch.delenv("OPENAI_API_KEY", raising=False)
        monkeypatch.delenv("OPENROUTER_API_KEY", raising=False)
        with pytest.raises(AgentConfigurationError, match="API key"):
            loop.run("session", "question")


def test_paper_context_serializes_typed_facts_without_workflow_runtime():
    chunk = PaperChunkMatch(
        1,
        "Evidence text.",
        0.8,
        "paper",
        0,
        "paper.pdf",
        "Paper",
        1,
        facts=(
            SimpleNamespace(
                fact_type="metric",
                value="92",
                statement="The reported metric is 92.",
                unit="percent",
            ),
        ),
    )

    context, sources = agent_loop_module._paper_context([chunk])

    assert '"facts"' in context
    assert '"fact_type": "metric"' in context
    assert '"unit": "percent"' in context
    assert len(sources) == 1


def test_sync_output_limit_raises_without_persisting_or_indexing(tmp_path):
    history_index = Mock()

    class LimitedModel:
        def invoke(self, messages):
            stage = model_stage(messages)
            if stage == "planner":
                return SimpleNamespace(content='{"subqueries":["one","two"]}')
            if stage == "subanswer":
                return SimpleNamespace(content="evidence")
            if stage == "coverage":
                return SimpleNamespace(
                    content='{"complete":true,"follow_up_subqueries":[]}'
                )
            return SimpleNamespace(
                content="partial answer",
                response_metadata={
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                },
            )

    database = tmp_path / "sync-limited.sqlite3"
    with AgentLoop(
        database,
        LimitedModel(),
        history_index=history_index,
        paper_retriever=FakeRetriever(),
    ) as loop:
        with pytest.raises(AgentIncompleteResponseError, match="incomplete"):
            loop.run("session", "question")

    history_index.index_exchange.assert_not_called()
    with SQLiteHistoryStore(database) as store:
        assert store.get_messages("session") == []
