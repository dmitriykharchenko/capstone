from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

from src.cli import dispatch
from src.cli.config import CLIConfig
from src.cli.graph_queries import handle_graph_query
from src.graph_store import (
    CozoGraphStore,
    GraphStoreConfigurationError,
    GraphStoreLockedError,
    stable_node_id,
)


def config(tmp_path: Path) -> CLIConfig:
    return CLIConfig(
        database_path=tmp_path / "history.db",
        papers_directory=tmp_path / "papers",
        chroma_directory=tmp_path / "chroma",
        chat_model="model",
        chat_base_url="https://example.test/v1",
        embedding_model="embedding",
        embedding_base_url="https://example.test/v1",
        api_key=None,
        system_prompt=None,
    )


def graph_path(tmp_path: Path) -> Path:
    return tmp_path / "paper_graph.db"


def seed_graph(tmp_path: Path) -> None:
    with CozoGraphStore(graph_path(tmp_path)) as store:
        store.replace_paper("p2", "Second", ["Ada", "Bob"], ["Graphs"])
        store.replace_paper("p1", "First", ["Ada", "Carol"], ["Graphs", "Logic"])


@pytest.mark.parametrize(
    ("query", "identifier_key", "identifier", "result_key"),
    [
        ("paper_details", "paper_id", "p1", "paper"),
        ("papers_by_author", "author_id", stable_node_id("author", "Ada"), "papers"),
        ("papers_by_topic", "topic_id", stable_node_id("topic", "Graphs"), "papers"),
        ("topics_by_author", "author_id", stable_node_id("author", "Ada"), "topics"),
        ("coauthors", "author_id", stable_node_id("author", "Ada"), "coauthors"),
        ("neighborhood", "node_id", stable_node_id("author", "Ada"), "neighborhood"),
    ],
)
def test_cli_queries_are_json_safe(
    tmp_path, query, identifier_key, identifier, result_key
):
    seed_graph(tmp_path)
    result = handle_graph_query(
        config(tmp_path), {"query": query, identifier_key: identifier, "max_depth": 2}
    )

    assert result["enabled"] is True and result["available"] is True
    assert result["query"] == query and result[result_key]
    json.dumps(result)


def test_cli_neighborhood_uses_canonical_edge_kinds(tmp_path):
    seed_graph(tmp_path)

    result = handle_graph_query(
        config(tmp_path),
        {
            "query": "neighborhood",
            "node_id": stable_node_id("author", "Ada"),
            "max_depth": 2,
        },
    )

    assert {edge["kind"] for edge in result["neighborhood"]["edges"]} == {
        "authored_by",
        "has_topic",
    }


@pytest.mark.parametrize(
    ("query", "identifier_key", "result_key", "empty"),
    [
        ("paper_details", "paper_id", "paper", None),
        ("papers_by_author", "author_id", "papers", []),
        ("papers_by_topic", "topic_id", "papers", []),
        ("topics_by_author", "author_id", "topics", []),
        ("coauthors", "author_id", "coauthors", []),
        ("neighborhood", "node_id", "neighborhood", {"nodes": [], "edges": []}),
    ],
)
def test_cli_missing_nodes_have_empty_results(
    tmp_path, query, identifier_key, result_key, empty
):
    result = handle_graph_query(
        config(tmp_path), {"query": query, identifier_key: "missing"}
    )
    assert result[result_key] == empty


def test_cli_distinguishes_disabled_dependency_from_unavailable_store(tmp_path):
    def disabled(_path):
        raise GraphStoreConfigurationError("native dependency missing")

    def unavailable(_path):
        raise GraphStoreLockedError("busy")

    request = {"query": "paper_details", "paper_id": "p1"}
    dependency = handle_graph_query(config(tmp_path), request, store_factory=disabled)
    locked = handle_graph_query(config(tmp_path), request, store_factory=unavailable)

    assert dependency == {
        "enabled": False,
        "available": False,
        "query": "paper_details",
        "paper": None,
        "reason": "embedded CozoDB dependency unavailable",
    }
    assert locked["enabled"] is True and locked["available"] is False
    assert locked["reason"] == "paper graph unavailable (GraphStoreLockedError)"


def test_public_dispatch_exposes_graph_query(monkeypatch, tmp_path):
    expected = {"enabled": True, "available": True, "query": "paper_details"}
    monkeypatch.setattr(
        "src.cli.dispatcher.handle_graph_query", lambda _config, _request: expected
    )
    request = {"query": "paper_details", "paper_id": "p1"}

    assert dispatch("graph_query", config(tmp_path), request) is expected


@pytest.mark.parametrize(
    "payload, message",
    [
        ({"query": "unknown", "paper_id": "p1"}, "query must be one of"),
        ({"query": "paper_details", "paper_id": " "}, "paper_id is required"),
        (
            {"query": "neighborhood", "node_id": "p1", "max_depth": 6},
            "max_depth must be between",
        ),
    ],
)
def test_cli_rejects_invalid_queries_identifiers_and_bounds(tmp_path, payload, message):
    with pytest.raises(ValueError, match=message):
        handle_graph_query(config(tmp_path), payload)


def test_module_cli_reopens_persisted_graph_and_reports_lock_contention(tmp_path):
    seed_graph(tmp_path)
    request = {
        "command": "graph_query",
        "query": "paper_details",
        "paper_id": "p1",
    }
    environment = {**os.environ, "CAPSTONE_CHROMA_DIR": str(tmp_path / "chroma")}
    command = [sys.executable, "-m", "src.cli"]
    reopened = subprocess.run(
        command,
        input=json.dumps(request),
        cwd=Path(__file__).parents[1],
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )
    assert reopened.returncode == 0
    assert json.loads(reopened.stdout)["paper"]["paper_id"] == "p1"

    with CozoGraphStore(graph_path(tmp_path)):
        blocked = subprocess.run(
            command,
            input=json.dumps(request),
            cwd=Path(__file__).parents[1],
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )
    payload = json.loads(blocked.stdout)
    assert blocked.returncode == 0
    assert payload["enabled"] is True and payload["available"] is False
    assert payload["paper"] is None


def test_module_cli_emits_json_error_for_invalid_graph_request(tmp_path):
    environment = {**os.environ, "CAPSTONE_CHROMA_DIR": str(tmp_path / "chroma")}
    failed = subprocess.run(
        [sys.executable, "-m", "src.cli"],
        input=json.dumps({"command": "graph_query", "query": "paper_details"}),
        cwd=Path(__file__).parents[1],
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )

    assert failed.returncode == 1
    assert json.loads(failed.stdout) == {
        "ok": False,
        "error": "paper_id is required and must be a non-empty string",
        "error_type": "ValueError",
    }
