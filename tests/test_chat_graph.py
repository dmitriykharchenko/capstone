"""Offline graph ownership and existing AgentLoop/CLI contract regressions."""

import json
import threading
from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from langsmith import get_tracing_context

from src.chat import graph as chat_graph
from src.chat.agent_loop import (
    AgentLoop,
    AgentLoopError,
    run_multihop,
)
from src.cli import commands
from src.cli.models import _SafeIndex
from src.storage.sqlite_history import SQLiteHistoryStore
from test_agent_loop_multihop import HopRetriever, StageModel, chunk, results
from test_cli_evaluation_stream import config

class OwnedModel(StageModel):
    def __init__(self, followups=()):
        super().__init__()
        self.coverage = json.dumps(
            {"complete": not followups, "follow_up_subqueries": list(followups)}
        )
        self.pulled = 0
        self.closed = 0
        self.threads = []

    def invoke(self, messages):
        self.threads.append(threading.get_ident())
        assert get_tracing_context()["enabled"] is False
        return super().invoke(messages)

    def stream(self, messages):
        self.stream_calls.append(list(messages))
        try:
            for item in self.final_chunks:
                self.threads.append(threading.get_ident())
                assert get_tracing_context()["enabled"] is False
                self.pulled += 1
                if isinstance(item, BaseException):
                    raise item
                yield item
        finally:
            self.closed += 1


def retriever(followups=()):
    return HopRetriever(
        {**results(), **{query: [chunk(1, query)] for query in followups}}
    )


@pytest.mark.parametrize("streaming", [False, True])
@pytest.mark.parametrize("followups", [(), ("gap",), ("gap one", "gap two")])
def test_chat_graph_matches_unchanged_stateless_evaluation_prompts_and_sources(
    tmp_path, streaming, followups
):
    reference_model = StageModel()
    reference_model.coverage = json.dumps(
        {"complete": not followups, "follow_up_subqueries": list(followups)}
    )
    reference = run_multihop(reference_model, retriever(followups), "question")
    model = OwnedModel(followups)
    with AgentLoop(
        tmp_path / "history.db", model, paper_retriever=retriever(followups)
    ) as loop:
        answer = (
            "".join(loop.stream("session", "question"))
            if streaming
            else loop.run("session", "question")
        )
        assert answer == reference.answer
        assert loop.last_sources == reference.sources
        assert loop.last_context == "\n\n".join(reference.contexts)
    calls = [messages for _, messages in model.calls] + model.stream_calls
    assert calls == [messages for _, messages in reference_model.calls]


def test_graph_failure_is_fixed_and_does_not_persist(tmp_path, monkeypatch):
    monkeypatch.setattr(
        chat_graph,
        "get_chat_graph",
        Mock(side_effect=RuntimeError("private graph detail")),
    )
    with AgentLoop(
        tmp_path / "history.db", OwnedModel(), paper_retriever=retriever()
    ) as loop:
        with pytest.raises(AgentLoopError) as raised:
            loop.run("session", "question")
        assert (
            str(raised.value) == "Chat workflow failed. Check backend logs and retry."
        )
        assert loop.history_store.get_messages("session") == []


@pytest.mark.parametrize("streaming", [False, True])
def test_actual_cli_graph_preserves_logs_payload_and_best_effort_indexing(
    tmp_path, monkeypatch, streaming
):
    cfg = config(tmp_path)
    model = OwnedModel(("gap",))
    lookup = retriever(("gap",))
    failing_index = Mock()
    failing_index.index_exchange.side_effect = RuntimeError("derived index unavailable")
    index = _SafeIndex(failing_index)
    monkeypatch.setattr(commands, "build_index", lambda *_: index)
    monkeypatch.setattr(commands, "build_paper_search_index", lambda *_: lookup)
    monkeypatch.setattr(
        commands,
        "build_agent_loop",
        lambda _cfg, store, history_index, papers: AgentLoop(
            history_store=store,
            model=model,
            history_index=history_index,
            paper_retriever=papers,
        ),
    )
    retrieval_log, chat_log = Mock(), Mock()
    monkeypatch.setattr(commands, "log_retrieval", retrieval_log)
    monkeypatch.setattr(commands, "log_chat", chat_log)
    request = {"session_id": "session", "message": "question"}
    if streaming:
        events = list(commands.handle_chat_stream(cfg, request))
        assert [e["event"] for e in events] == [
            "plan",
            "subquery",
            "subquery",
            "subanswer",
            "subanswer",
            "coverage",
            "coverage",
            "subquery",
            "subanswer",
            "delta",
            "delta",
            "done",
        ]
        reply = events[-1]
        assert set(reply) == {
            "event",
            "ok",
            "done",
            "session_id",
            "reply",
            "sources",
            "chat_mode",
            "indexed",
        }
    else:
        reply = commands.handle_chat(cfg, request)
        assert set(reply) == {"session_id", "reply", "sources", "chat_mode", "indexed"}
    assert reply["reply"] == "final answer"
    assert reply["indexed"] is True
    assert reply["chat_mode"] == "mock"
    failing_index.index_exchange.assert_called_once()
    retrieval_log.assert_called_once()
    chat_log.assert_called_once()
    assert retrieval_log.call_args.kwargs["results"] == reply["sources"]
    assert chat_log.call_args.kwargs["response"] == reply["reply"]
    with SQLiteHistoryStore(cfg.database_path) as store:
        assert [m.content for m in store.get_messages("session")] == [
            "question",
            "final answer",
        ]


@pytest.mark.parametrize("failure", ["planner", "coverage", "incomplete"])
def test_actual_cli_graph_errors_are_single_terminal_sanitized_events(
    tmp_path, monkeypatch, failure
):
    cfg = config(tmp_path)
    model = OwnedModel()
    if failure == "incomplete":
        model.final_chunks = [
            "partial",
            SimpleNamespace(content="", response_metadata={"finish_reason": "length"}),
        ]
    else:
        model.fail_stage = failure
    monkeypatch.setattr(commands, "build_index", lambda *_: None)
    monkeypatch.setattr(commands, "build_paper_search_index", lambda *_: retriever())
    monkeypatch.setattr(
        commands,
        "build_agent_loop",
        lambda _cfg, store, _index, papers: AgentLoop(
            history_store=store, model=model, paper_retriever=papers
        ),
    )
    logs = Mock()
    monkeypatch.setattr(commands, "log_chat", logs)
    monkeypatch.setattr(commands, "log_retrieval", logs)
    events = list(
        commands.handle_chat_stream(
            cfg, {"session_id": "session", "message": "question"}
        )
    )
    assert events[-1] == {
        "event": "error",
        "ok": False,
        "error": (
            "Chat response was incomplete; shorten the request and try again"
            if failure == "incomplete"
            else "Chat failed. Check backend logs and retry."
        ),
    }
    assert sum(event["event"] == "error" for event in events) == 1
    assert not any(event["event"] == "done" for event in events)
    logs.assert_not_called()
    with SQLiteHistoryStore(cfg.database_path) as store:
        assert store.get_messages("session") == []
