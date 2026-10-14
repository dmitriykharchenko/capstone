"""Offline stream graph ordering, backpressure, cancellation, and durability."""


import pytest

from src.cli import commands, stream_workflows as workflows
from src.cli.stream_workflows import CommandRuntimeError
from src.indexing.paper_indexer import (
    PaperIndexProgress,
)


def counts():
    return {status: 0 for status in ("indexed", "empty", "failed", "skipped")}


@pytest.mark.parametrize(
    "factory", [workflows.get_index_graph, workflows.get_evaluation_graph]
)
def test_graphs_are_cached_ephemeral_and_deterministic(factory):
    graph = factory()
    assert graph is factory()
    assert graph.checkpointer is graph.store is graph.cache is None
    assert graph.get_graph().to_json() == factory.__wrapped__().get_graph().to_json()


@pytest.mark.parametrize("updates", [[], [PaperIndexProgress(0, 1)]])
def test_truncated_index_source_fails_closed(updates):
    refresh = []
    events = workflows.index_events(
        iter(updates), counts=counts(), refresh=lambda: refresh.append(True)
    )
    with pytest.raises(CommandRuntimeError):
        list(events)
    assert refresh == []


@pytest.mark.parametrize(
    "payload",
    [
        {},
        {"retrieval_type": "unsupported", "test_cases": []},
        {"retrieval_type": "hybrid", "test_cases": []},
        {"retrieval_type": "hybrid", "test_cases": [{}]},
    ],
)
def test_evaluation_validates_before_backend_construction(monkeypatch, payload):
    monkeypatch.setattr(
        commands,
        "build_evaluation_runner",
        lambda _config: pytest.fail("backend built"),
    )
    with pytest.raises(ValueError):
        list(commands.handle_evaluation_stream(None, payload))
