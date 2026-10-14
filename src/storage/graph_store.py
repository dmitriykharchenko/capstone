"""Embedded CozoDB persistence for the local paper knowledge graph."""

from __future__ import annotations

import errno
import hashlib
import os
import shlex
import sys
import threading
import time
import unicodedata
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ..indexing.fact_extraction import Fact

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
    "chunk": (
        ":create chunk {chunk_id: String => paper_id: String, chunk_index: Int, page: Int}",
        (
            ("chunk_id", True, "String"),
            ("paper_id", False, "String"),
            ("chunk_index", False, "Int"),
            ("page", False, "Int"),
        ),
    ),
    "fact": (
        ":create fact {fact_id: String => fact_type: String, value: String, "
        "statement: String, unit: String}",
        (
            ("fact_id", True, "String"),
            ("fact_type", False, "String"),
            ("value", False, "String"),
            ("statement", False, "String"),
            ("unit", False, "String"),
        ),
    ),
    "paper_chunk": (
        ":create paper_chunk {paper_id: String, chunk_id: String}",
        (("paper_id", True, "String"), ("chunk_id", True, "String")),
    ),
    "chunk_fact": (
        ":create chunk_fact {chunk_id: String, fact_id: String}",
        (("chunk_id", True, "String"), ("fact_id", True, "String")),
    ),
    "paper_fact_version": (
        ":create paper_fact_version {paper_id: String => version: Int}",
        (("paper_id", True, "String"), ("version", False, "Int")),
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
class ChunkNode:
    """Stored chunk identity and provenance in the fact graph."""

    chunk_id: str
    paper_id: str
    chunk_index: int
    page: int

    def __post_init__(self) -> None:
        object.__setattr__(self, "chunk_id", _compact_text(self.chunk_id, "chunk_id"))
        object.__setattr__(self, "paper_id", _compact_text(self.paper_id, "paper_id"))
        for field in ("chunk_index", "page"):
            value = getattr(self, field)
            minimum = 0 if field == "chunk_index" else 1
            if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
                raise ValueError(f"{field} must be an integer of at least {minimum}")


@dataclass(frozen=True, slots=True)
class PaperGraph:
    paper_id: str
    title: str
    authors: tuple[AuthorNode, ...]
    topics: tuple[TopicNode, ...]


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
            self._handle.truncate(0)
            self._handle.write(str(os.getpid()).encode("ascii"))
            self._handle.flush()

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


def _is_backend_owner_command(command: str) -> bool:
    """Match only this source CLI or this exact frozen backend executable."""

    command = command.strip()
    if not command:
        return False
    executable = str(Path(sys.executable).resolve())
    if getattr(sys, "frozen", False):
        return command == executable or command.startswith(
            f"{executable} --multiprocessing-fork "
        )
    try:
        arguments = shlex.split(command)
    except ValueError:
        return False
    return (
        len(arguments) == 3
        and Path(arguments[0]).resolve() == Path(executable)
        and arguments[1:] == ["-m", "src.cli"]
    )


def stop_lock_owner(database_path: str | Path, *, timeout: float = 10.0) -> bool:
    """Stop the backend process holding the database lock; return whether one was stopped.

    Only the exact source CLI or frozen executable recorded in the lock file is
    signalled, so a stale or reused process ID never terminates another program.
    """

    if os.name == "nt":  # pragma: no cover - no owner PID is recorded on Windows
        return False
    import signal
    import subprocess

    lock_path = _database_path(database_path)
    lock_path = lock_path.with_name(f"{lock_path.name}.lock")
    try:
        pid = int(lock_path.read_text(encoding="ascii").strip())
    except (OSError, ValueError):
        return False
    if pid <= 0 or pid == os.getpid():
        return False
    command = subprocess.run(
        ["ps", "-o", "command=", "-p", str(pid)],
        capture_output=True,
        text=True,
        check=False,
    ).stdout
    if not _is_backend_owner_command(command):
        return False
    for sig, wait in ((signal.SIGTERM, timeout), (signal.SIGKILL, 2.0)):
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            return True
        deadline = time.monotonic() + wait
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return True
            time.sleep(0.05)
    return True


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
        *,
        chunks: Sequence[ChunkNode] | None = None,
        facts_by_chunk: Mapping[str, Sequence[Fact]] | None = None,
        fact_version: int | None = None,
        clear_facts: bool = False,
    ) -> PaperGraph:
        """Atomically replace one paper and optionally replace or clear its facts."""

        identity = _compact_text(paper_id, "paper_id")
        paper_title = _compact_text(title, "title")
        author_nodes = _author_nodes(authors)
        topic_nodes = _topic_nodes(topics)
        if not isinstance(clear_facts, bool):
            raise TypeError("clear_facts must be a boolean")
        if clear_facts and chunks is not None:
            raise ValueError("clear_facts cannot be combined with chunks")
        fact_graph = None
        if chunks is not None:
            fact_graph = _validated_fact_graph(
                identity, chunks, facts_by_chunk or {}, fact_version
            )
        elif facts_by_chunk is not None or fact_version is not None:
            raise ValueError("chunks are required with facts_by_chunk or fact_version")
        with self._mutex:
            client = self._require_client()
            tx = client.multi_transact(True)
            try:
                self._remove_edges(tx, identity)
                if fact_graph is not None or clear_facts:
                    self._remove_fact_graph(tx, identity)
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
                if fact_graph is not None:
                    self._put_fact_graph(tx, identity, *fact_graph)
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

    def replace_paper_facts(
        self,
        paper_id: str,
        chunks: Sequence[ChunkNode],
        facts_by_chunk: Mapping[str, Sequence[Fact]],
        fact_version: int,
    ) -> None:
        """Atomically replace only chunk/fact data for an existing paper."""

        identity = _compact_text(paper_id, "paper_id")
        fact_graph = _validated_fact_graph(
            identity, chunks, facts_by_chunk, fact_version
        )
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
                raise ValueError(f"paper {identity!r} does not exist")
            tx = client.multi_transact(True)
            try:
                self._remove_fact_graph(tx, identity)
                self._put_fact_graph(tx, identity, *fact_graph)
                self._remove_orphans(tx)
                tx.commit()
            except Exception as exc:
                try:
                    tx.abort()
                except Exception:
                    pass
                raise GraphStorePersistenceError(
                    f"failed to atomically replace facts for paper {identity!r}"
                ) from exc

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
                self._remove_fact_graph(tx, identity)
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
    def _remove_fact_graph(tx: Any, paper_id: str) -> None:
        tx.run(
            "?[chunk_id, fact_id] := *paper_chunk{paper_id: $paper_id, chunk_id}, "
            "*chunk_fact{chunk_id, fact_id} :rm chunk_fact {chunk_id, fact_id}",
            {"paper_id": paper_id},
        )
        tx.run(
            "?[paper_id, chunk_id] := *paper_chunk{paper_id, chunk_id}, "
            "paper_id = $paper_id :rm paper_chunk {paper_id, chunk_id}",
            {"paper_id": paper_id},
        )
        tx.run(
            "?[chunk_id] := *chunk{chunk_id, paper_id: $paper_id} "
            ":rm chunk {chunk_id}",
            {"paper_id": paper_id},
        )
        tx.run(
            "?[paper_id] := *paper_fact_version{paper_id}, paper_id = $paper_id "
            ":rm paper_fact_version {paper_id}",
            {"paper_id": paper_id},
        )

    @staticmethod
    def _put_fact_graph(
        tx: Any,
        paper_id: str,
        chunks: tuple[ChunkNode, ...],
        facts_by_chunk: dict[str, tuple[Fact, ...]],
        fact_version: int,
    ) -> None:
        if chunks:
            tx.run(
                "?[chunk_id, paper_id, chunk_index, page] <- $rows "
                ":put chunk {chunk_id, paper_id, chunk_index, page}",
                {
                    "rows": [
                        [node.chunk_id, paper_id, node.chunk_index, node.page]
                        for node in chunks
                    ]
                },
            )
            tx.run(
                "?[paper_id, chunk_id] <- $rows "
                ":put paper_chunk {paper_id, chunk_id}",
                {"rows": [[paper_id, node.chunk_id] for node in chunks]},
            )
        facts = {
            fact.fact_id: fact
            for node in chunks
            for fact in facts_by_chunk[node.chunk_id]
        }
        if facts:
            tx.run(
                "?[fact_id, fact_type, value, statement, unit] <- $rows "
                ":put fact {fact_id, fact_type, value, statement, unit}",
                {
                    "rows": [
                        [
                            fact.fact_id,
                            fact.fact_type,
                            fact.value,
                            fact.statement,
                            fact.unit or "",
                        ]
                        for fact in facts.values()
                    ]
                },
            )
            tx.run(
                "?[chunk_id, fact_id] <- $rows "
                ":put chunk_fact {chunk_id, fact_id}",
                {
                    "rows": [
                        [node.chunk_id, fact.fact_id]
                        for node in chunks
                        for fact in facts_by_chunk[node.chunk_id]
                    ]
                },
            )
        tx.run(
            "?[paper_id, version] <- [[$paper_id, $version]] "
            ":put paper_fact_version {paper_id, version}",
            {"paper_id": paper_id, "version": fact_version},
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
        tx.run(
            "?[fact_id] := *fact{fact_id}, not *chunk_fact{fact_id} "
            ":rm fact {fact_id}"
        )

    def fact_version_for_paper(self, paper_id: str) -> int | None:
        """Return the completed fact extraction version for one paper."""

        identity = _compact_text(paper_id, "paper_id")
        with self._mutex:
            rows = _rows(
                self._require_client().run(
                    "?[version] := *paper_fact_version{paper_id: $paper_id, version}",
                    {"paper_id": identity},
                )
            )
        return int(rows[0][0]) if rows else None

    def fact_ids_for_paper(self, paper_id: str) -> tuple[str, ...]:
        """Return distinct fact IDs connected to any chunk in one paper."""

        identity = _compact_text(paper_id, "paper_id")
        with self._mutex:
            rows = _rows(
                self._require_client().run(
                    "?[fact_id] := *paper_chunk{paper_id: $paper_id, chunk_id}, "
                    "*chunk_fact{chunk_id, fact_id}\n:sort fact_id",
                    {"paper_id": identity},
                )
            )
        return tuple(row[0] for row in rows)

    def facts_for_chunks(
        self, chunk_ids: Sequence[str]
    ) -> dict[str, tuple[Fact, ...]]:
        """Return connected typed facts for each requested chunk ID."""

        identities = _identities(chunk_ids, "chunk_ids")
        result: dict[str, tuple[Fact, ...]] = {}
        with self._mutex:
            client = self._require_client()
            for chunk_id in identities:
                rows = _rows(
                    client.run(
                        "?[fact_id, fact_type, value, statement, unit] := "
                        "*chunk_fact{chunk_id: $chunk_id, fact_id}, "
                        "*fact{fact_id, fact_type, value, statement, unit} "
                        "\n:sort fact_type, value, fact_id",
                        {"chunk_id": chunk_id},
                    )
                )
                if rows:
                    result[chunk_id] = tuple(
                        Fact(row[0], row[1], row[2], row[3], row[4] or None)
                        for row in rows
                    )
        return result

    def chunks_for_facts(
        self, fact_ids: Sequence[str]
    ) -> dict[str, tuple[str, ...]]:
        """Return sorted connected chunk IDs for each requested fact ID."""

        identities = _identities(fact_ids, "fact_ids")
        result: dict[str, tuple[str, ...]] = {}
        with self._mutex:
            client = self._require_client()
            for fact_id in identities:
                rows = _rows(
                    client.run(
                        "?[chunk_id] := "
                        "*chunk_fact{chunk_id, fact_id: $fact_id}\n:sort chunk_id",
                        {"fact_id": fact_id},
                    )
                )
                result[fact_id] = tuple(row[0] for row in rows)
        return result

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


def _identities(values: Sequence[str], label: str) -> tuple[str, ...]:
    if isinstance(values, (str, bytes)) or not isinstance(values, Sequence):
        raise TypeError(f"{label} must be a sequence of non-empty strings")
    identities = tuple(_compact_text(value, label[:-1]) for value in values)
    return tuple(dict.fromkeys(identities))


def _validated_fact_graph(
    paper_id: str,
    chunks: Sequence[ChunkNode],
    facts_by_chunk: Mapping[str, Sequence[Fact]],
    fact_version: int | None,
) -> tuple[tuple[ChunkNode, ...], dict[str, tuple[Fact, ...]], int]:
    if isinstance(chunks, (str, bytes)) or not isinstance(chunks, Sequence):
        raise TypeError("chunks must be a sequence of ChunkNode values")
    nodes = tuple(chunks)
    if any(not isinstance(node, ChunkNode) for node in nodes):
        raise TypeError("chunks must contain only ChunkNode values")
    if any(node.paper_id != paper_id for node in nodes):
        raise ValueError("every chunk must belong to paper_id")
    if len({node.chunk_id for node in nodes}) != len(nodes):
        raise ValueError("chunk_id values must be unique")
    if len({node.chunk_index for node in nodes}) != len(nodes):
        raise ValueError("chunk_index values must be unique")
    if not isinstance(facts_by_chunk, Mapping):
        raise TypeError("facts_by_chunk must be a mapping")
    known_ids = {node.chunk_id for node in nodes}
    unknown_ids = set(facts_by_chunk) - known_ids
    if unknown_ids:
        raise ValueError("facts_by_chunk contains an unknown chunk_id")
    normalized: dict[str, tuple[Fact, ...]] = {}
    for node in nodes:
        facts = facts_by_chunk.get(node.chunk_id, ())
        if isinstance(facts, (str, bytes)) or not isinstance(facts, Sequence):
            raise TypeError("facts_by_chunk values must be sequences of Fact values")
        if any(not isinstance(fact, Fact) for fact in facts):
            raise TypeError("facts_by_chunk values must contain only Fact values")
        normalized[node.chunk_id] = tuple(
            {fact.fact_id: fact for fact in facts}.values()
        )
    if (
        isinstance(fact_version, bool)
        or not isinstance(fact_version, int)
        or fact_version < 1
    ):
        raise ValueError("fact_version must be a positive integer")
    return nodes, normalized, fact_version


def _rows(result: Any) -> list[list[Any]]:
    if not isinstance(result, dict) or not isinstance(result.get("rows"), list):
        raise GraphStorePersistenceError("CozoDB returned an invalid relation result")
    return result["rows"]


def _author_rows(result: Any) -> tuple[AuthorNode, ...]:
    nodes = (AuthorNode(row[0], row[1]) for row in _rows(result))
    return tuple(
        sorted(nodes, key=lambda node: (normalize_node_name(node.name), node.author_id))
    )


def _topic_rows(result: Any) -> tuple[TopicNode, ...]:
    nodes = (TopicNode(row[0], row[1]) for row in _rows(result))
    return tuple(
        sorted(nodes, key=lambda node: (normalize_node_name(node.name), node.topic_id))
    )


__all__ = [
    "AuthorNode",
    "ChunkNode",
    "CozoGraphStore",
    "GraphStoreClosedError",
    "GraphStoreConfigurationError",
    "GraphStoreError",
    "GraphStoreLockedError",
    "GraphStorePersistenceError",
    "PaperGraph",
    "TopicNode",
    "normalize_node_name",
    "stable_node_id",
]
