"""Offline graph ownership and existing AgentLoop/CLI contract regressions."""

import json
import threading
from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from langsmith import get_tracing_context, tracing_context

from src import chat_graph
from src.agent_loop import (
    AgentIncompleteResponseError,
    AgentLoop,
    AgentLoopError,
    AgentModelError,
    MultihopCompletedEvent,
    run_multihop,
)
from src.cli import commands
from src.cli.models import MockChatModel, _SafeIndex
from src.sqlite_history import SQLiteHistoryStore
from test_agent_loop_multihop import HopRetriever, StageModel, chunk, results
from test_cli_chat_stream import config


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


def test_compiled_graph_has_real_chat_nodes_without_durable_state():
    graph = chat_graph.get_chat_graph()
    assert graph is chat_graph.get_chat_graph()
    assert set(graph.nodes) == {
        "__start__",
        "prepare",
        "plan",
        "hop_started",
        "retrieve",
        "subanswer",
        "coverage_started",
        "coverage",
        "compose",
        "final_delta",
        "persist",
    }
    assert graph.checkpointer is None
    assert graph.store is None
    assert graph.cache is None
    assert all(node.retry_policy is None for node in graph.nodes.values())
    edges = {(edge.source, edge.target) for edge in graph.get_graph().edges}
    assert ("retrieve", "subanswer") in edges
    assert ("prepare", "plan") in edges
    assert ("compose", "final_delta") in edges
    assert ("final_delta", "persist") in edges
    assert ("persist", "__end__") in edges


@pytest.mark.parametrize("method", ["run", "invoke", "stream", "stream_events"])
def test_all_chat_entrypoints_use_graph_not_evaluation_loop(
    tmp_path, monkeypatch, method
):
    graph = chat_graph.get_chat_graph()
    invoke = Mock(wraps=graph.invoke)
    monkeypatch.setattr(graph, "invoke", invoke)
    monkeypatch.setattr("src.agent_loop.run_multihop", Mock(side_effect=AssertionError))
    model = OwnedModel()
    with AgentLoop(tmp_path / "history.db", model, paper_retriever=retriever()) as loop:
        result = getattr(loop, method)("session", "question")
        if method.startswith("stream"):
            result = list(result)
        else:
            assert result == "final answer"
        assert invoke.call_count >= 8
        assert len(loop.history_store.get_messages("session")) == 2
    assert set(model.threads) == {threading.get_ident()}
    for call in invoke.call_args_list:
        assert call.kwargs["config"] == {"callbacks": []}
        assert not {"model", "loop", "deltas", "history"} & call.args[0].keys()


@pytest.mark.parametrize("boundary", range(14))
def test_close_at_every_adaptive_public_boundary_never_advances_or_persists(
    tmp_path, boundary
):
    followups = ("gap one", "gap two")
    model = OwnedModel(followups)
    lookup = retriever(followups)
    index = Mock()
    with AgentLoop(
        tmp_path / "history.db", model, paper_retriever=lookup, history_index=index
    ) as loop:
        events = loop.stream_events("session", "question")
        for _ in range(boundary):
            assert next(events).stage != "completed"
        before = (len(model.calls), len(lookup.calls), model.pulled)
        assert loop.history_store.get_messages("session") == []
        events.close()
        assert before == (len(model.calls), len(lookup.calls), model.pulled)
        assert model.closed == (1 if model.pulled else 0)
        assert loop.history_store.get_messages("session") == []
        assert loop.last_sources == ()
        assert loop.last_context == ""
        index.index_exchange.assert_not_called()


def test_each_pull_obeys_prework_boundaries_and_commits_only_on_completion(tmp_path):
    model = OwnedModel()
    lookup = retriever()
    database = tmp_path / "history.db"
    index = Mock()
    with AgentLoop(
        database, model, paper_retriever=lookup, history_index=index
    ) as loop:
        events = loop.stream_events("session", "question")
        expected = [
            ("plan", 1, 0, 0),
            ("hop_started", 1, 0, 0),
            ("hop_completed", 2, 1, 0),
            ("hop_started", 2, 1, 0),
            ("hop_completed", 3, 2, 0),
            ("coverage_started", 3, 2, 0),
            ("coverage_completed", 4, 2, 0),
            ("final_delta", 4, 2, 1),
            ("final_delta", 4, 2, 2),
        ]
        for stage, calls, lookups, deltas in expected:
            assert next(events).stage == stage
            assert (len(model.calls), len(lookup.calls), model.pulled) == (
                calls,
                lookups,
                deltas,
            )
            assert loop.history_store.get_messages("session") == []
            index.index_exchange.assert_not_called()
        event = next(events)
        assert isinstance(event, MultihopCompletedEvent)
        assert event.answer == "final answer"
        assert event.sources == loop.last_sources
        assert model.closed == 1
        assert [m.content for m in loop.history_store.get_messages("session")] == [
            "question",
            "final answer",
        ]
        index.index_exchange.assert_called_once()
        assert list(events) == []
        events.close()
        index.index_exchange.assert_called_once()


@pytest.mark.parametrize("method", ["stream", "stream_events"])
@pytest.mark.parametrize("error", [KeyboardInterrupt, GeneratorExit])
def test_cancellation_closes_provider_without_persistence(tmp_path, method, error):
    model = OwnedModel()
    with AgentLoop(tmp_path / "history.db", model, paper_retriever=retriever()) as loop:
        events = getattr(loop, method)("session", "question")
        while True:
            event = next(events)
            if method == "stream" or event.stage == "final_delta":
                break
        with pytest.raises(error):
            events.throw(error())
        assert model.pulled == 1
        assert model.closed == 1
        assert loop.history_store.get_messages("session") == []


@pytest.mark.parametrize("failure", [RuntimeError("disconnected"), KeyboardInterrupt()])
def test_provider_failure_closes_once_and_keeps_previous_turn(tmp_path, failure):
    model = OwnedModel()
    with AgentLoop(tmp_path / "history.db", model, paper_retriever=retriever()) as loop:
        loop.run("session", "previous")
        model.final_chunks = ["partial", failure]
        expected = (
            AgentModelError if isinstance(failure, Exception) else KeyboardInterrupt
        )
        with pytest.raises(expected):
            list(loop.stream_events("session", "question"))
        assert model.closed == 1
        assert [m.content for m in loop.history_store.get_messages("session")] == [
            "previous",
            "final answer",
        ]
        assert loop.last_sources == ()


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


def test_tracing_is_disabled_inside_graph_but_restored_at_yield(tmp_path):
    model = OwnedModel()
    with AgentLoop(tmp_path / "history.db", model, paper_retriever=retriever()) as loop:
        with tracing_context(enabled=True):
            events = loop.stream_events("session", "question")
            for _ in range(8):
                next(events)
                assert get_tracing_context()["enabled"] is True
            events.close()
            assert get_tracing_context()["enabled"] is True


def test_interleaved_streams_do_not_share_state_or_resources(tmp_path):
    first, second = OwnedModel(), OwnedModel(("gap",))
    with AgentLoop(tmp_path / "a.db", first, paper_retriever=retriever()) as a:
        with AgentLoop(
            tmp_path / "b.db", second, paper_retriever=retriever(("gap",))
        ) as b:
            partial = a.stream_events("a", "first")
            assert next(partial).stage == "plan"
            assert b.run("b", "second") == "final answer"
            assert list(partial)[-1].answer == "final answer"
            assert len(a.last_sources) == 3
            assert len(b.last_sources) == 4
            assert a.history_store.get_messages("b") == []
            assert b.history_store.get_messages("a") == []


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
            "subanswer",
            "subquery",
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


def test_stream_without_provider_stream_still_waits_for_completion_pull(tmp_path):
    model = OwnedModel()
    model.stream = None
    with AgentLoop(tmp_path / "history.db", model, paper_retriever=retriever()) as loop:
        events = loop.stream_events("session", "question")
        for _ in range(8):
            event = next(events)
        assert event.stage == "final_delta"
        assert event.text == "final answer"
        assert loop.history_store.get_messages("session") == []
        events.close()
        assert loop.history_store.get_messages("session") == []


def test_late_empty_output_limit_chunk_never_commits(tmp_path):
    model = OwnedModel()
    model.final_chunks = [
        "apparently complete",
        SimpleNamespace(content="", response_metadata={"finish_reason": "length"}),
    ]
    with AgentLoop(tmp_path / "history.db", model, paper_retriever=retriever()) as loop:
        events = loop.stream_events("session", "question")
        for _ in range(8):
            next(events)
        with pytest.raises(AgentIncompleteResponseError):
            next(events)
        assert model.closed == 1
        assert loop.history_store.get_messages("session") == []


@pytest.mark.parametrize("streaming", [False, True])
def test_persistence_failure_has_no_retry_or_derived_index_write(
    tmp_path, monkeypatch, streaming
):
    index = Mock()
    with AgentLoop(
        tmp_path / "history.db",
        OwnedModel(),
        paper_retriever=retriever(),
        history_index=index,
    ) as loop:
        failure = RuntimeError("write failed")
        append = Mock(side_effect=failure)
        monkeypatch.setattr(loop.history_store, "append_exchange", append)
        with pytest.raises(RuntimeError) as raised:
            if streaming:
                list(loop.stream_events("session", "question"))
            else:
                loop.run("session", "question")
        assert raised.value is failure
        assert not getattr(failure, "__notes__", ())
        append.assert_called_once()
        index.index_exchange.assert_not_called()
        assert loop.last_sources == ()
        assert loop.history_store.get_messages("session") == []


def test_seven_hops_and_long_delta_stream_are_not_limited_by_graph_recursion(tmp_path):
    queries = tuple(f"initial {i}" for i in range(5))
    followups = ("gap one", "gap two")
    model = OwnedModel(followups)
    model.plan = json.dumps({"subqueries": queries})
    model.final_chunks = [""] + ["text "] * 60 + [""]
    lookup = HopRetriever(
        {
            query: [chunk(rank, f"{query}-{rank}") for rank in range(1, 6)]
            for query in (*queries, *followups)
        }
    )
    with AgentLoop(tmp_path / "history.db", model, paper_retriever=lookup) as loop:
        events = list(loop.stream_events("session", "question"))
        assert len(loop.last_sources) == 35
        assert [source.rank for source in loop.last_sources] == list(range(1, 36))
        assert events[-1].answer == "text " * 60
        assert sum(event.stage == "final_delta" for event in events) == 60
        assert [stage for stage, _ in model.calls].count("coverage") == 1
        assert lookup.calls == [
            (query, 5 * (index + 1))
            for index, query in enumerate((*queries, *followups))
        ]


def test_index_failure_after_stream_commit_is_explicit_and_never_replayed(tmp_path):
    index = Mock()
    failure = RuntimeError("derived index unavailable")
    index.index_exchange.side_effect = failure
    with AgentLoop(
        tmp_path / "history.db",
        OwnedModel(),
        paper_retriever=retriever(),
        history_index=index,
    ) as loop:
        events = loop.stream_events("session", "question")
        with pytest.raises(RuntimeError) as raised:
            list(events)
        assert raised.value is failure
        assert [m.content for m in loop.history_store.get_messages("session")] == [
            "question",
            "final answer",
        ]
        assert len(loop.last_sources) == 3
        assert loop.last_context
        assert list(events) == []
        index.index_exchange.assert_called_once()


def test_real_offline_mock_supports_graph_streaming(tmp_path):
    lookup = HopRetriever(
        {
            "Evidence supporting question": [chunk(1, "support")],
            "Evidence limiting or contrasting question": [chunk(1, "limit")],
        }
    )
    with AgentLoop(
        tmp_path / "history.db", MockChatModel(), paper_retriever=lookup
    ) as loop:
        assert (
            "".join(loop.stream("session", "question"))
            == "[offline mock reply] question"
        )
        assert len(loop.history_store.get_messages("session")) == 2


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
