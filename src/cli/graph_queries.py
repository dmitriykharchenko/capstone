"""Machine-readable queries for the persisted paper knowledge graph."""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from typing import Any

from ..graph_store import (
    AuthorNode,
    CozoGraphStore,
    GraphNeighborhood,
    GraphStoreConfigurationError,
    GraphStoreError,
    PaperGraph,
    TopicNode,
)
from .config import CLIConfig

GRAPH_DATABASE_FILENAME = "paper_graph.db"
GRAPH_QUERY_KINDS = (
    "paper_details",
    "papers_by_author",
    "papers_by_topic",
    "topics_by_author",
    "coauthors",
    "neighborhood",
)


def handle_graph_query(
    config: CLIConfig,
    request: dict[str, Any],
    *,
    store_factory: Callable[[Path], CozoGraphStore] = CozoGraphStore,
) -> dict[str, Any]:
    """Run one allow-listed graph read without exposing raw CozoScript."""

    query = _require_query(request.get("query"))
    key = _result_key(query)
    empty = _empty_result(query)
    identifier_name = {
        "paper_details": "paper_id",
        "papers_by_author": "author_id",
        "papers_by_topic": "topic_id",
        "topics_by_author": "author_id",
        "coauthors": "author_id",
        "neighborhood": "node_id",
    }[query]
    identifier = _require_identifier(request.get(identifier_name), identifier_name)
    try:
        with store_factory(_graph_database_path(config)) as store:
            result = _run_query(store, query, identifier, request)
    except GraphStoreConfigurationError:
        return _status_payload(
            query,
            key,
            empty,
            enabled=False,
            reason="embedded CozoDB dependency unavailable",
        )
    except GraphStoreError as exc:
        return _status_payload(
            query,
            key,
            empty,
            enabled=True,
            reason=f"paper graph unavailable ({type(exc).__name__})",
        )
    return _status_payload(query, key, result, enabled=True)


def _run_query(
    store: CozoGraphStore, query: str, identifier: str, request: dict[str, Any]
) -> Any:
    if query == "paper_details":
        paper = store.get_paper(identifier)
        return _paper_payload(paper) if paper is not None else None
    if query == "papers_by_author":
        return [_paper_payload(paper) for paper in store.papers_for_author(identifier)]
    if query == "papers_by_topic":
        return [_paper_payload(paper) for paper in store.papers_for_topic(identifier)]
    if query == "topics_by_author":
        return [_topic_payload(topic) for topic in store.topics_for_author(identifier)]
    if query == "coauthors":
        return [
            _author_payload(author) for author in store.coauthors_for_author(identifier)
        ]
    depth = request.get("max_depth", 1)
    return _neighborhood_payload(store.neighborhood(identifier, max_depth=depth))


def _status_payload(
    query: str,
    key: str,
    result: Any,
    *,
    enabled: bool,
    reason: str | None = None,
) -> dict[str, Any]:
    payload = {
        "enabled": enabled,
        "available": reason is None,
        "query": query,
        key: result,
    }
    if reason is not None:
        payload["reason"] = reason
    return payload


def _paper_payload(paper: PaperGraph) -> dict[str, Any]:
    return {
        "paper_id": paper.paper_id,
        "title": paper.title,
        "authors": [_author_payload(author) for author in paper.authors],
        "topics": [_topic_payload(topic) for topic in paper.topics],
    }


def _author_payload(author: AuthorNode) -> dict[str, str]:
    return {"author_id": author.author_id, "name": author.name}


def _topic_payload(topic: TopicNode) -> dict[str, str]:
    return {"topic_id": topic.topic_id, "name": topic.name}


def _neighborhood_payload(graph: GraphNeighborhood) -> dict[str, Any]:
    return {
        "nodes": [
            {"node_id": node.node_id, "kind": node.kind, "label": node.label}
            for node in graph.nodes
        ],
        "edges": [
            {
                "source_id": edge.source_id,
                "target_id": edge.target_id,
                "kind": edge.kind,
            }
            for edge in graph.edges
        ],
    }


def _require_query(value: Any) -> str:
    if not isinstance(value, str) or value not in GRAPH_QUERY_KINDS:
        allowed = ", ".join(GRAPH_QUERY_KINDS)
        raise ValueError(f"query must be one of: {allowed}")
    return value


def _require_identifier(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{name} is required and must be a non-empty string")
    return value


def _result_key(query: str) -> str:
    return {
        "paper_details": "paper",
        "papers_by_author": "papers",
        "papers_by_topic": "papers",
        "topics_by_author": "topics",
        "coauthors": "coauthors",
        "neighborhood": "neighborhood",
    }[query]


def _empty_result(query: str) -> Any:
    if query == "paper_details":
        return None
    if query == "neighborhood":
        return {"nodes": [], "edges": []}
    return []


def _graph_database_path(config: CLIConfig) -> Path:
    return config.chroma_directory.parent / GRAPH_DATABASE_FILENAME


__all__ = ["GRAPH_QUERY_KINDS", "handle_graph_query"]
