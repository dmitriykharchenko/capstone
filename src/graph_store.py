"""Embedded CozoDB persistence for the local paper knowledge graph."""

from __future__ import annotations

import errno
import hashlib
import os
import threading
import time
import unicodedata
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

MAX_NEIGHBORHOOD_DEPTH = 5
_SCHEMAS = {
    "paper": (
        ":create paper {paper_id: String => title: String}",
        (("paper_id", True, "String"), ("title", False, "String")),
    ),
    "author": (
        ":create author {author_id: String => name: String, normalized_name: String}",
        (
            ("author_id", True, "String"),
            ("name", False, "String"),
            ("normalized_name", False, "String"),
        ),
    ),
    "topic": (
        ":create topic {topic_id: String => name: String, normalized_name: String}",
        (
            ("topic_id", True, "String"),
            ("name", False, "String"),
            ("normalized_name", False, "String"),
        ),
    ),
    "authored_by": (
        ":create authored_by {paper_id: String, author_id: String}",
        (("paper_id", True, "String"), ("author_id", True, "String")),
    ),
    "has_topic": (
        ":create has_topic {paper_id: String, topic_id: String}",
        (("paper_id", True, "String"), ("topic_id", True, "String")),
    ),
}


class GraphStoreError(RuntimeError):
    """Base error for graph persistence failures."""


class GraphStoreConfigurationError(GraphStoreError):
    """Raised when embedded CozoDB is unavailable or incompatible."""


class GraphStoreLockedError(GraphStoreError):
    """Raised when another process owns the embedded database."""


class GraphStoreClosedError(GraphStoreError):
    """Raised when an operation is attempted after close."""


class GraphStorePersistenceError(GraphStoreError):
    """Raised when an atomic graph operation fails."""


@dataclass(frozen=True, slots=True)
class AuthorNode:
    author_id: str
    name: str


@dataclass(frozen=True, slots=True)
class TopicNode:
    topic_id: str
    name: str


@dataclass(frozen=True, slots=True)
class PaperGraph:
    paper_id: str
    title: str
    authors: tuple[AuthorNode, ...]
    topics: tuple[TopicNode, ...]


@dataclass(frozen=True, slots=True)
class GraphNode:
    node_id: str
    kind: str
    label: str


@dataclass(frozen=True, slots=True)
class GraphEdge:
    source_id: str
    target_id: str
    kind: str


@dataclass(frozen=True, slots=True)
class GraphNeighborhood:
    nodes: tuple[GraphNode, ...]
    edges: tuple[GraphEdge, ...]


def normalize_node_name(value: str) -> str:
    """Return the deterministic identity form for an author or topic name."""

    return _compact_text(value, "node name").casefold()


def stable_node_id(kind: str, value: str) -> str:
    """Build a stable, namespaced identifier from a normalized label."""

    if kind not in {"author", "topic"}:
        raise ValueError("kind must be 'author' or 'topic'")
    normalized = normalize_node_name(value)
    digest = hashlib.sha256(normalized.encode("utf-8")).hexdigest()
    return f"{kind}:{digest}"


class _DatabaseLock:
    def __init__(self, path: Path, timeout: float) -> None:
        self._path = path
        self._timeout = timeout
        self._handle: Any | None = None

    def acquire(self) -> None:
        self._handle = self._path.open("a+b")
        deadline = time.monotonic() + self._timeout
        while True:
            try:
                self._try_lock()
                return
            except OSError as exc:
                if exc.errno not in {errno.EACCES, errno.EAGAIN}:
                    self.release()
                    raise
                if time.monotonic() >= deadline:
                    self.release()
                    raise GraphStoreLockedError(
                        f"CozoDB is already open in another process: {self._path}"
                    ) from exc
                time.sleep(0.05)

    def _try_lock(self) -> None:
        assert self._handle is not None
        if os.name == "nt":  # pragma: no cover - exercised on Windows targets
            import msvcrt

            self._handle.seek(0)
            if not self._handle.read(1):
                self._handle.write(b"\0")
                self._handle.flush()
            self._handle.seek(0)
            msvcrt.locking(self._handle.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl

            fcntl.flock(self._handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)

    def release(self) -> None:
        if self._handle is None:
            return
        try:
            if os.name == "nt":  # pragma: no cover - exercised on Windows targets
                import msvcrt

                self._handle.seek(0)
                msvcrt.locking(self._handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                import fcntl

                fcntl.flock(self._handle.fileno(), fcntl.LOCK_UN)
        finally:
            self._handle.close()
            self._handle = None


class CozoGraphStore:
    """Own one serialized, SQLite-backed embedded CozoDB client."""

    def __init__(
        self,
        database_path: str | Path,
        *,
        lock_timeout: float = 5.0,
        client_factory: Callable[..., Any] | None = None,
    ) -> None:
        self.database_path = _database_path(database_path)
        if isinstance(lock_timeout, bool) or not isinstance(lock_timeout, (int, float)):
            raise TypeError("lock_timeout must be a non-negative number")
        if lock_timeout < 0:
            raise ValueError("lock_timeout must be a non-negative number")
        self.database_path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = _DatabaseLock(
            self.database_path.with_name(f"{self.database_path.name}.lock"),
            float(lock_timeout),
        )
        self._mutex = threading.RLock()
        self._closed = False
        self._client: Any | None = None
        self._lock.acquire()
        try:
            factory = client_factory or _load_client()
            self._client = factory("sqlite", str(self.database_path), dataframe=False)
            self._initialize_schema()
        except GraphStoreError:
            self._release_failed_open()
            raise
        except Exception as exc:
            self._release_failed_open()
            raise GraphStoreConfigurationError(
                "failed to open embedded CozoDB; install "
                "pycozo[embedded]==0.7.6 for this Python/platform"
            ) from exc

    def _release_failed_open(self) -> None:
        if self._client is not None:
            try:
                self._client.close()
            except Exception:
                pass
        self._client = None
        self._lock.release()

    def _initialize_schema(self) -> None:
        client = self._require_client()
        relations = _rows(client.run("::relations"))
        existing = {row[0] for row in relations}
        for name, (create_script, expected) in _SCHEMAS.items():
            if name not in existing:
                client.run(create_script)
            columns = _rows(client.run(f"::columns {name}"))
            actual = tuple((row[0], row[1], row[3]) for row in columns)
            if actual != expected:
                raise GraphStoreConfigurationError(
                    f"incompatible CozoDB relation schema for {name!r}"
                )

    def replace_paper(
        self,
        paper_id: str,
        title: str,
        authors: Sequence[str],
        topics: Sequence[str],
    ) -> PaperGraph:
        """Atomically replace one paper and its author/topic edges."""

        identity = _compact_text(paper_id, "paper_id")
        paper_title = _compact_text(title, "title")
        author_nodes = _author_nodes(authors)
        topic_nodes = _topic_nodes(topics)
        with self._mutex:
            client = self._require_client()
            tx = client.multi_transact(True)
            try:
                self._remove_edges(tx, identity)
                tx.run(
                    "?[paper_id, title] <- [[$paper_id, $title]] "
                    ":put paper {paper_id, title}",
                    {"paper_id": identity, "title": paper_title},
                )
                tx.run(
                    "?[author_id, name, normalized_name] <- $rows "
                    ":put author {author_id, name, normalized_name}",
                    {
                        "rows": [
                            [node.author_id, node.name, normalize_node_name(node.name)]
                            for node in author_nodes
                        ]
                    },
                )
                tx.run(
                    "?[topic_id, name, normalized_name] <- $rows "
                    ":put topic {topic_id, name, normalized_name}",
                    {
                        "rows": [
                            [node.topic_id, node.name, normalize_node_name(node.name)]
                            for node in topic_nodes
                        ]
                    },
                )
                tx.run(
                    "?[paper_id, author_id] <- $rows "
                    ":put authored_by {paper_id, author_id}",
                    {"rows": [[identity, node.author_id] for node in author_nodes]},
                )
                tx.run(
                    "?[paper_id, topic_id] <- $rows "
                    ":put has_topic {paper_id, topic_id}",
                    {"rows": [[identity, node.topic_id] for node in topic_nodes]},
                )
                self._remove_orphans(tx)
                tx.commit()
            except Exception as exc:
                try:
                    tx.abort()
                except Exception:
                    pass
                raise GraphStorePersistenceError(
                    f"failed to atomically replace paper {identity!r}"
                ) from exc
        return PaperGraph(identity, paper_title, author_nodes, topic_nodes)

    def remove_paper(self, paper_id: str) -> bool:
        """Atomically remove a paper, its edges, and newly orphaned nodes."""

        identity = _compact_text(paper_id, "paper_id")
        with self._mutex:
            client = self._require_client()
            exists = bool(
                _rows(
                    client.run(
                        "?[paper_id] := *paper{paper_id}, paper_id = $paper_id",
                        {"paper_id": identity},
                    )
                )
            )
            if not exists:
                return False
            tx = client.multi_transact(True)
            try:
                self._remove_edges(tx, identity)
                tx.run(
                    "?[paper_id] <- [[$paper_id]] :rm paper {paper_id}",
                    {"paper_id": identity},
                )
                self._remove_orphans(tx)
                tx.commit()
            except Exception as exc:
                try:
                    tx.abort()
                except Exception:
                    pass
                raise GraphStorePersistenceError(
                    f"failed to atomically remove paper {identity!r}"
                ) from exc
        return True

    @staticmethod
    def _remove_edges(tx: Any, paper_id: str) -> None:
        tx.run(
            "?[paper_id, author_id] := *authored_by{paper_id, author_id}, "
            "paper_id = $paper_id :rm authored_by {paper_id, author_id}",
            {"paper_id": paper_id},
        )
        tx.run(
            "?[paper_id, topic_id] := *has_topic{paper_id, topic_id}, "
            "paper_id = $paper_id :rm has_topic {paper_id, topic_id}",
            {"paper_id": paper_id},
        )

    @staticmethod
    def _remove_orphans(tx: Any) -> None:
        tx.run(
            "?[author_id] := *author{author_id}, not *authored_by{author_id} "
            ":rm author {author_id}"
        )
        tx.run(
            "?[topic_id] := *topic{topic_id}, not *has_topic{topic_id} "
            ":rm topic {topic_id}"
        )

    def get_paper(self, paper_id: str) -> PaperGraph | None:
        """Return one paper with direct authors and topics."""

        identity = _compact_text(paper_id, "paper_id")
        with self._mutex:
            client = self._require_client()
            rows = _rows(
                client.run(
                    "?[title] := *paper{paper_id: $paper_id, title}",
                    {"paper_id": identity},
                )
            )
            if not rows:
                return None
            authors = _author_rows(
                client.run(
                    "?[author_id, name] := "
                    "*authored_by{paper_id: $paper_id, author_id}, "
                    "*author{author_id, name}\n:sort name",
                    {"paper_id": identity},
                )
            )
            topics = _topic_rows(
                client.run(
                    "?[topic_id, name] := "
                    "*has_topic{paper_id: $paper_id, topic_id}, "
                    "*topic{topic_id, name}\n:sort name",
                    {"paper_id": identity},
                )
            )
            return PaperGraph(identity, rows[0][0], authors, topics)

    def papers_for_author(self, author_id: str) -> tuple[PaperGraph, ...]:
        """Return papers directly connected to an author."""

        return self._papers_for_node("authored_by", "author_id", author_id)

    def papers_for_topic(self, topic_id: str) -> tuple[PaperGraph, ...]:
        """Return papers directly connected to a topic."""

        return self._papers_for_node("has_topic", "topic_id", topic_id)

    def _papers_for_node(
        self, relation: str, column: str, node_id: str
    ) -> tuple[PaperGraph, ...]:
        identity = _compact_text(node_id, column)
        with self._mutex:
            client = self._require_client()
            rows = _rows(
                client.run(
                    f"?[paper_id] := *{relation}{{paper_id, {column}: $node_id}}"
                    "\n:sort paper_id",
                    {"node_id": identity},
                )
            )
            papers = tuple(self.get_paper(row[0]) for row in rows)
            return tuple(paper for paper in papers if paper is not None)

    def coauthors_for_author(self, author_id: str) -> tuple[AuthorNode, ...]:
        """Return distinct authors who share at least one paper."""

        identity = _compact_text(author_id, "author_id")
        with self._mutex:
            result = self._require_client().run(
                "?[other_id, name] := "
                "*authored_by{paper_id, author_id: $author_id}, "
                "*authored_by{paper_id, author_id: other_id}, "
                "other_id != $author_id, *author{author_id: other_id, name}"
                "\n:sort name",
                {"author_id": identity},
            )
            return _author_rows(result)

    def topics_for_author(self, author_id: str) -> tuple[TopicNode, ...]:
        """Return distinct topics reached through an author's papers."""

        identity = _compact_text(author_id, "author_id")
        with self._mutex:
            result = self._require_client().run(
                "?[topic_id, name] := "
                "*authored_by{paper_id, author_id: $author_id}, "
                "*has_topic{paper_id, topic_id}, *topic{topic_id, name}"
                "\n:sort name",
                {"author_id": identity},
            )
            return _topic_rows(result)

    def neighborhood(self, node_id: str, *, max_depth: int = 1) -> GraphNeighborhood:
        """Return a deterministic induced neighborhood up to five hops."""

        identity = _compact_text(node_id, "node_id")
        if isinstance(max_depth, bool) or not isinstance(max_depth, int):
            raise TypeError("max_depth must be an integer")
        if not 0 <= max_depth <= MAX_NEIGHBORHOOD_DEPTH:
            raise ValueError(
                f"max_depth must be between 0 and {MAX_NEIGHBORHOOD_DEPTH}"
            )
        with self._mutex:
            client = self._require_client()
            nodes = self._all_nodes(client)
            if identity not in nodes:
                raise KeyError(f"graph node does not exist: {identity!r}")
            edges = self._all_edges(client)
            adjacency: dict[str, set[str]] = {node: set() for node in nodes}
            for edge in edges:
                adjacency[edge.source_id].add(edge.target_id)
                adjacency[edge.target_id].add(edge.source_id)
            visited = {identity}
            frontier = {identity}
            for _ in range(max_depth):
                frontier = {
                    neighbor
                    for current in frontier
                    for neighbor in adjacency[current]
                    if neighbor not in visited
                }
                visited.update(frontier)
            selected_nodes = tuple(
                sorted(
                    (nodes[current] for current in visited),
                    key=lambda node: (node.kind, node.label, node.node_id),
                )
            )
            selected_edges = tuple(
                edge
                for edge in edges
                if edge.source_id in visited and edge.target_id in visited
            )
            return GraphNeighborhood(selected_nodes, selected_edges)

    @staticmethod
    def _all_nodes(client: Any) -> dict[str, GraphNode]:
        nodes: dict[str, GraphNode] = {}
        for kind, id_column in (
            ("paper", "paper_id"),
            ("author", "author_id"),
            ("topic", "topic_id"),
        ):
            rows = _rows(
                client.run(
                    f"?[node_id, label] := *{kind}{{{id_column}: node_id, "
                    f"{'title' if kind == 'paper' else 'name'}: label}}"
                )
            )
            nodes.update({row[0]: GraphNode(row[0], kind, row[1]) for row in rows})
        return nodes

    @staticmethod
    def _all_edges(client: Any) -> tuple[GraphEdge, ...]:
        edges = [
            GraphEdge(row[0], row[1], "authorship")
            for row in _rows(
                client.run(
                    "?[paper_id, author_id] := *authored_by{paper_id, author_id}"
                    "\n:sort paper_id, author_id"
                )
            )
        ]
        edges.extend(
            GraphEdge(row[0], row[1], "paper_topic")
            for row in _rows(
                client.run(
                    "?[paper_id, topic_id] := *has_topic{paper_id, topic_id}"
                    "\n:sort paper_id, topic_id"
                )
            )
        )
        return tuple(
            sorted(edges, key=lambda edge: (edge.kind, edge.source_id, edge.target_id))
        )

    def _require_client(self) -> Any:
        if self._closed or self._client is None:
            raise GraphStoreClosedError("CozoGraphStore is closed")
        return self._client

    def close(self) -> None:
        """Close CozoDB and release the cross-process lock; safe to repeat."""

        with self._mutex:
            if self._closed:
                return
            self._closed = True
            try:
                if self._client is not None:
                    self._client.close()
            finally:
                self._client = None
                self._lock.release()

    def __enter__(self) -> CozoGraphStore:
        return self

    def __exit__(self, _exc_type: object, _exc: object, _traceback: object) -> None:
        self.close()


def _load_client() -> Callable[..., Any]:
    try:
        from pycozo import Client
    except (ImportError, OSError) as exc:
        raise GraphStoreConfigurationError(
            "embedded CozoDB requires pycozo[embedded]==0.7.6"
        ) from exc
    return Client


def _database_path(value: str | Path) -> Path:
    if not isinstance(value, (str, Path)):
        raise TypeError("database_path must be a non-empty string or Path")
    if not str(value).strip():
        raise ValueError("database_path must not be empty")
    return Path(value)


def _compact_text(value: str, label: str) -> str:
    if not isinstance(value, str):
        raise TypeError(f"{label} must be a non-empty string")
    compact = " ".join(unicodedata.normalize("NFKC", value).split())
    if not compact:
        raise ValueError(f"{label} must not be empty")
    return compact


def _node_names(values: Sequence[str], label: str) -> tuple[str, ...]:
    if isinstance(values, (str, bytes)) or not isinstance(values, Sequence):
        raise TypeError(f"{label} must be a sequence of non-empty strings")
    deduplicated: dict[str, str] = {}
    for value in values:
        display = _compact_text(value, label[:-1])
        deduplicated.setdefault(normalize_node_name(display), display)
    if not deduplicated:
        raise ValueError(f"{label} must contain at least one value")
    return tuple(deduplicated.values())


def _author_nodes(values: Sequence[str]) -> tuple[AuthorNode, ...]:
    nodes = (
        AuthorNode(stable_node_id("author", name), name)
        for name in _node_names(values, "authors")
    )
    return tuple(sorted(nodes, key=lambda node: normalize_node_name(node.name)))


def _topic_nodes(values: Sequence[str]) -> tuple[TopicNode, ...]:
    nodes = (
        TopicNode(stable_node_id("topic", name), name)
        for name in _node_names(values, "topics")
    )
    return tuple(sorted(nodes, key=lambda node: normalize_node_name(node.name)))


def _rows(result: Any) -> list[list[Any]]:
    if not isinstance(result, dict) or not isinstance(result.get("rows"), list):
        raise GraphStorePersistenceError("CozoDB returned an invalid relation result")
    return result["rows"]


def _author_rows(result: Any) -> tuple[AuthorNode, ...]:
    return tuple(AuthorNode(row[0], row[1]) for row in _rows(result))


def _topic_rows(result: Any) -> tuple[TopicNode, ...]:
    return tuple(TopicNode(row[0], row[1]) for row in _rows(result))


__all__ = [
    "AuthorNode",
    "CozoGraphStore",
    "GraphEdge",
    "GraphNeighborhood",
    "GraphNode",
    "GraphStoreClosedError",
    "GraphStoreConfigurationError",
    "GraphStoreError",
    "GraphStoreLockedError",
    "GraphStorePersistenceError",
    "MAX_NEIGHBORHOOD_DEPTH",
    "PaperGraph",
    "TopicNode",
    "normalize_node_name",
    "stable_node_id",
]
