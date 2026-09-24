"""Linearizable run-key arbitration in canonical history, not the trace database.

Only the canonical exchange stores message/source contents. The arbitration
table retains a fingerprint and tombstone even if its exchange is later removed,
so an old run can never resurrect a deleted conversation.
"""

from contextlib import contextmanager
from dataclasses import asdict
import sqlite3

from .orchestration_agents import HistoryTurn
from .orchestration_runtime import Discarded, FinalAnswer, Published
from .orchestration_tools import digest
from .orchestration_wire import ERROR, validate_type
from .sqlite_history import SQLiteHistoryStore


def history_context(path, session_id, cutoff=None):
    with connection(path) as db:
        if cutoff is None:
            cutoff = db.execute(
                "SELECT coalesce(max(id), 0) FROM messages WHERE session_id = ?",
                (session_id,),
            ).fetchone()[0]
        rows = db.execute(
            "SELECT role, content FROM messages WHERE session_id = ? AND id <= ? "
            "AND role IN ('user', 'assistant') ORDER BY id DESC LIMIT 16",
            (session_id, cutoff),
        ).fetchall()
    turns = []
    for role, content in reversed(rows):
        content = content.encode()[:2048].decode(errors="ignore")
        validate_type(content, "query")
        candidate = turns + [HistoryTurn(role, content)]
        if len(str([asdict(t) for t in candidate]).encode()) > 7500:
            turns = turns[1:]
        turns.append(HistoryTurn(role, content))
    return cutoff, tuple(turns), digest([asdict(t) for t in turns])


@contextmanager
def connection(path):
    db = sqlite3.connect(path, timeout=10)
    try:
        db.execute("PRAGMA foreign_keys = ON")
        db.execute("PRAGMA synchronous = FULL")
        with db:
            yield db
    finally:
        db.close()


class CanonicalPublisher:
    def __init__(self, path, args, request, sources, index=None):
        self.path, self.args, self.request = path, args, request
        self.sources, self.index = sources, index
        self.key = "exchange-" + digest(args)
        self.fingerprint = digest([*args, request.encode()])
        with SQLiteHistoryStore(path):
            pass
        with connection(path) as db:
            db.execute(
                "CREATE TABLE IF NOT EXISTS orchestration_publications ("
                "run_key TEXT PRIMARY KEY NOT NULL, fingerprint TEXT NOT NULL, "
                "status TEXT NOT NULL CHECK(status IN ('published', 'discarded')), "
                "exchange_id INTEGER UNIQUE REFERENCES exchanges(id) ON DELETE SET NULL)"
            )

    def _lookup(self, db, key):
        if key != self.key:
            raise ValueError(ERROR)
        row = db.execute(
            "SELECT fingerprint, status, exchange_id FROM orchestration_publications WHERE run_key = ?",
            (key,),
        ).fetchone()
        if row is None:
            return None
        if row[0] != self.fingerprint:
            raise ValueError(ERROR)
        if row[1] == "discarded" or row[2] is None:
            return Discarded()
        rows = db.execute(
            "SELECT role, content FROM messages WHERE session_id = ? AND exchange_id = ? ORDER BY id",
            (self.args[1], row[2]),
        ).fetchall()
        if (
            len(rows) != 2
            or rows[0] != ("user", self.request.query)
            or rows[1][0] != "assistant"
        ):
            raise ValueError(ERROR)
        validate_type(rows[1][1], "reply")
        return Published(row[2], FinalAnswer(rows[1][1]))

    def lookup(self, key):
        with connection(self.path) as db:
            return self._lookup(db, key)

    def publish(self, key, answer):
        answer.__post_init__()
        validate_type(answer.assistant_text, "reply")
        sources = self.sources(answer.evidence_ids)
        validate_type(sources, "sources")
        with connection(self.path) as db:
            db.execute("BEGIN IMMEDIATE")
            existing = self._lookup(db, key)
            if isinstance(existing, Discarded):
                raise ValueError(ERROR)
            if existing is not None:
                if (
                    existing.answer.assistant_text != answer.assistant_text
                    or self._sources(db, existing.exchange_id) != sources
                ):
                    raise ValueError(ERROR)
                return existing
            exchange_id = db.execute(
                "INSERT INTO exchanges (session_id) VALUES (?)", (self.args[1],)
            ).lastrowid
            db.executemany(
                "INSERT INTO messages (session_id, role, content, exchange_id) VALUES (?, ?, ?, ?)",
                [
                    (self.args[1], "user", self.request.query, exchange_id),
                    (self.args[1], "assistant", answer.assistant_text, exchange_id),
                ],
            )
            db.executemany(
                "INSERT INTO exchange_sources (exchange_id, rank, paper_id, chunk_index, filename, title, page, score, snippet) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    (
                        exchange_id,
                        *(
                            source[k]
                            for k in (
                                "rank",
                                "paper_id",
                                "chunk_index",
                                "filename",
                                "title",
                                "page",
                                "score",
                                "snippet",
                            )
                        ),
                    )
                    for source in sources
                ],
            )
            db.execute(
                "INSERT INTO orchestration_publications VALUES (?, ?, 'published', ?)",
                (key, self.fingerprint, exchange_id),
            )
        warning = False
        if self.index is not None:
            try:
                self.index.index_exchange(exchange_id, self.args[1])
            except Exception:
                warning = True
        return Published(exchange_id, answer, warning)

    def discard(self, key):
        with connection(self.path) as db:
            db.execute("BEGIN IMMEDIATE")
            existing = self._lookup(db, key)
            if existing is not None:
                return existing
            db.execute(
                "INSERT INTO orchestration_publications VALUES (?, ?, 'discarded', NULL)",
                (key, self.fingerprint),
            )
        return Discarded()

    def saved_sources(self, exchange_id):
        with connection(self.path) as db:
            return self._sources(db, exchange_id)

    def _sources(self, db, exchange_id):
        owner = db.execute(
            "SELECT session_id FROM exchanges WHERE id = ?", (exchange_id,)
        ).fetchone()
        if owner is None or owner[0] != self.args[1]:
            raise ValueError(ERROR)
        names = (
            "rank",
            "paper_id",
            "chunk_index",
            "filename",
            "title",
            "page",
            "score",
            "snippet",
        )
        rows = db.execute(
            "SELECT rank, paper_id, chunk_index, filename, title, page, score, snippet FROM exchange_sources WHERE exchange_id = ? ORDER BY rank LIMIT 36",
            (exchange_id,),
        ).fetchall()
        sources = [dict(zip(names, row)) for row in rows]
        validate_type(sources, "sources")
        return sources
