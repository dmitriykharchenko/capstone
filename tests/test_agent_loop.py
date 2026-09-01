from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from langchain_core.messages import AIMessage, HumanMessage, SystemMessage

from src.agent_loop import AgentConfigurationError, AgentLoop, AgentModelError, ChatOpenAIConfig
from src.sqlite_history import SQLiteHistoryStore


class FakeChatModel:
    def __init__(self, answers):
        self.answers = iter(answers)
        self.calls = []

    def invoke(self, messages):
        self.calls.append(list(messages))
        return AIMessage(content=next(self.answers))


def test_first_and_subsequent_turns_include_history_and_persist(tmp_path):
    model = FakeChatModel(["first answer", "second answer"])
    with AgentLoop(tmp_path / "history.sqlite3", model=model, system_prompt="Be precise") as loop:
        assert loop.run("session", "first question") == "first answer"
        assert loop.run("session", "follow-up") == "second answer"

    first, second = model.calls
    assert isinstance(first[0], SystemMessage)
    assert isinstance(first[-1], HumanMessage)
    assert [message.content for message in first] == ["Be precise", "first question"]
    assert [message.content for message in second] == [
        "Be precise",
        "first question",
        "first answer",
        "follow-up",
    ]
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        assert [(m.role, m.content) for m in store.get_messages("session")] == [
            ("user", "first question"),
            ("assistant", "first answer"),
            ("user", "follow-up"),
            ("assistant", "second answer"),
        ]


def test_history_persists_across_loop_instances_and_sessions_are_isolated(tmp_path):
    database = tmp_path / "history.sqlite3"
    first_model = FakeChatModel(["alpha reply"])
    with AgentLoop(database, first_model) as loop:
        loop.run("alpha", "alpha question")

    second_model = FakeChatModel(["beta reply"])
    with AgentLoop(database, second_model) as loop:
        loop.run("beta", "beta question")
    assert [m.content for m in second_model.calls[0]] == [
        second_model.calls[0][0].content,
        "beta question",
    ]

    with SQLiteHistoryStore(database) as store:
        assert [m.content for m in store.get_messages("alpha")] == ["alpha question", "alpha reply"]
        assert [m.content for m in store.get_messages("beta")] == ["beta question", "beta reply"]


@pytest.mark.parametrize("session_id", [None, " ", 3])
def test_run_validates_session_id(tmp_path, session_id):
    with AgentLoop(tmp_path / "history.sqlite3", FakeChatModel(["answer"])) as loop:
        with pytest.raises((TypeError, ValueError), match="session_id"):
            loop.run(session_id, "question")  # type: ignore[arg-type]


@pytest.mark.parametrize("user_text", [None, " ", 3])
def test_run_validates_user_text(tmp_path, user_text):
    with AgentLoop(tmp_path / "history.sqlite3", FakeChatModel(["answer"])) as loop:
        with pytest.raises((TypeError, ValueError), match="user_text"):
            loop.run("session", user_text)  # type: ignore[arg-type]


def test_failed_model_does_not_persist_partial_turn(tmp_path):
    history_index = Mock()

    class BrokenModel:
        def invoke(self, _messages):
            raise RuntimeError("offline failure")

    with AgentLoop(
        tmp_path / "history.sqlite3",
        BrokenModel(),
        history_index=history_index,
    ) as loop:
        with pytest.raises(AgentModelError, match="invocation failed"):
            loop.run("session", "question")
    history_index.index_exchange.assert_not_called()
    with SQLiteHistoryStore(tmp_path / "history.sqlite3") as store:
        assert store.get_messages("session") == []


def test_configuration_validation_and_missing_key(tmp_path, monkeypatch):
    with pytest.raises(AgentConfigurationError, match="model"):
        ChatOpenAIConfig(model=" ")
    with AgentLoop(tmp_path / "history.sqlite3") as loop:
        monkeypatch.delenv("OPENAI_API_KEY", raising=False)
        monkeypatch.delenv("OPENROUTER_API_KEY", raising=False)
        with pytest.raises(AgentConfigurationError, match="API key"):
            loop.run("session", "question")


def test_injected_model_can_return_simple_content_object(tmp_path):
    class SimpleModel:
        def invoke(self, _messages):
            return SimpleNamespace(content="plain text")

    with AgentLoop(tmp_path / "history.sqlite3", SimpleModel()) as loop:
        assert loop.invoke("session", "question") == "plain text"


def test_successful_turn_is_atomically_persisted_then_indexed(tmp_path):
    database = tmp_path / "history.sqlite3"
    with SQLiteHistoryStore(database) as store:
        indexed = []

        class FakeIndex:
            def index_exchange(self, exchange_id, session_id):
                indexed.append(store.get_exchange(exchange_id, session_id))

        loop = AgentLoop(
            history_store=store,
            model=FakeChatModel(["answer"]),
            history_index=FakeIndex(),
        )
        assert loop.run("session", "question") == "answer"

    assert len(indexed) == 1
    assert indexed[0].user_message.content == "question"
    assert indexed[0].assistant_message.content == "answer"


def test_index_failure_is_explicit_after_sqlite_commit(tmp_path):
    class BrokenIndex:
        def index_exchange(self, _exchange_id, _session_id):
            raise RuntimeError("offline Chroma")

    database = tmp_path / "history.sqlite3"
    with AgentLoop(
        database,
        FakeChatModel(["answer"]),
        history_index=BrokenIndex(),
    ) as loop:
        with pytest.raises(RuntimeError, match="offline Chroma"):
            loop.run("session", "question")

    with SQLiteHistoryStore(database) as store:
        assert [(item.role, item.content) for item in store.get_messages("session")] == [
            ("user", "question"),
            ("assistant", "answer"),
        ]