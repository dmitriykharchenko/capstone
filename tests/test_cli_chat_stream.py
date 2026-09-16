from src.agent_loop import (
    MultihopCompletedEvent,
    MultihopFinalDeltaEvent,
    MultihopHopCompletedEvent,
    MultihopHopStartedEvent,
    MultihopPlanEvent,
)
from src.cli.commands import handle_chat_stream
from src.cli.config import CLIConfig
from src.sqlite_history import SourceRecord


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


def source(rank, paper_id):
    return SourceRecord(
        rank,
        paper_id,
        rank - 1,
        f"{paper_id}.pdf",
        paper_id.title(),
        rank,
        1 / rank,
        f"evidence from {paper_id}",
    )


def install_transport(monkeypatch, loop, store, captured=None):
    monkeypatch.setattr("src.cli.commands.build_history_store", lambda _cfg: store)
    monkeypatch.setattr("src.cli.commands.build_index", lambda _cfg, _store: object())
    monkeypatch.setattr(
        "src.cli.commands.build_paper_search_index", lambda _cfg: object()
    )
    monkeypatch.setattr(
        "src.cli.commands.build_agent_loop",
        lambda _cfg, _store, _index, _retriever: loop,
    )
    if captured is not None:
        monkeypatch.setattr(
            "src.cli.commands.log_retrieval",
            lambda *_args, **kwargs: captured.update(retrieval=kwargs),
        )
        monkeypatch.setattr(
            "src.cli.commands.log_chat",
            lambda *_args, **kwargs: captured.update(chat=kwargs),
        )


def test_multihop_ndjson_events_are_ordered_and_done_is_authoritative(
    monkeypatch, tmp_path
):
    alpha = source(1, "alpha")
    beta = source(2, "beta")

    class Store:
        closed = False

        def close(self):
            self.closed = True

    class Loop:
        last_context = "combined evidence"

        def stream_events(self, _session_id, _message):
            yield MultihopPlanEvent(("alpha evidence", "beta evidence"))
            yield MultihopHopStartedEvent(1, 2, "alpha evidence")
            yield MultihopHopCompletedEvent(
                1, 2, "alpha evidence", "alpha answer", (alpha,)
            )
            yield MultihopHopStartedEvent(2, 2, "beta evidence")
            yield MultihopHopCompletedEvent(
                2, 2, "beta evidence", "beta answer", (source(1, "beta"),)
            )
            yield MultihopFinalDeltaEvent("streamed draft")
            yield MultihopCompletedEvent("persisted final", (alpha, beta))

    store = Store()
    captured = {}
    install_transport(monkeypatch, Loop(), store, captured)

    events = list(
        handle_chat_stream(
            config(tmp_path), {"session_id": "session", "message": "question"}
        )
    )

    assert [event["event"] for event in events] == [
        "plan",
        "subquery",
        "subanswer",
        "subquery",
        "subanswer",
        "delta",
        "done",
    ]
    assert events[0]["subqueries"] == [
        {"id": "subquery-1", "index": 1, "query": "alpha evidence"},
        {"id": "subquery-2", "index": 2, "query": "beta evidence"},
    ]
    assert events[2]["id"] == "subquery-1"
    assert events[2]["answer"] == "alpha answer"
    assert events[-2] == {"event": "delta", "delta": "streamed draft"}
    assert events[-1]["reply"] == "persisted final"
    assert [item["paper_id"] for item in events[-1]["sources"]] == [
        "alpha",
        "beta",
    ]
    assert captured["chat"]["response"] == "persisted final"
    assert captured["retrieval"]["context"] == "combined evidence"
    assert store.closed is True


def test_multihop_transport_sanitizes_failures_and_closes_store(monkeypatch, tmp_path):
    class Store:
        closed = False

        def close(self):
            self.closed = True

    class Loop:
        def stream_events(self, _session_id, _message):
            yield MultihopPlanEvent(("alpha", "beta"))
            raise RuntimeError("api_key=secret at /private/backend.py")

    store = Store()
    install_transport(monkeypatch, Loop(), store)

    events = list(
        handle_chat_stream(
            config(tmp_path), {"session_id": "session", "message": "question"}
        )
    )

    assert events[-1] == {
        "event": "error",
        "ok": False,
        "error": "Chat failed. Check backend logs and retry.",
    }
    assert "secret" not in str(events)
    assert not any(event["event"] == "done" for event in events)
    assert store.closed is True