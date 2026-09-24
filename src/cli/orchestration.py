"""Injectable CLI orchestration adapters. Importing this module performs no I/O."""

from contextlib import contextmanager
from pathlib import Path
from uuid import uuid4

from ..orchestration_publication import CanonicalPublisher, history_context
from ..orchestration_request import RunRequest
from ..orchestration_runtime import OrchestrationRuntime
from ..orchestration_state import RunStatus, TERMINAL_RUNS
from ..orchestration_store import SQLiteOrchestrationStore, RunNotFoundError
from ..orchestration_stream import Projection, StreamSink
from ..orchestration_tools import digest
from ..orchestration_wire import ERROR, StreamValidator, validate_type
from ..sqlite_history import SQLiteHistoryStore

GRAPH_VERSION = "orchestration-v1"
STREAM_COMMANDS = ("orchestration_start", "orchestration_resume")
READ_COMMANDS = (
    "orchestration_pending",
    "orchestration_discard",
    "orchestration_trace",
)


@contextmanager
def session_owner(path, session_id):
    """OS-owned lock: crash releases ownership, never an unsafe expiring lease."""
    import fcntl

    directory = path.parent / ".orchestration-locks"
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / digest(session_id)).open("a") as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            raise ValueError(ERROR) from None
        try:
            yield
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)


def store_path(config):
    path = Path(config.database_path).parent / "chat_orchestration.sqlite3"
    if path.resolve() == Path(config.database_path).resolve():
        raise ValueError(ERROR)
    return path


def identities(request):
    values = tuple(request.get(k) for k in ("session_id", "run_id", "turn_id"))
    for value in values:
        validate_type(value, "id")
    return values


class EmptyRetriever:
    def retrieve_chunks(self, query, n_results=5):
        return []


def _stream(
    config,
    request,
    *,
    executor=None,
    retriever=None,
    publisher_factory=CanonicalPublisher,
    index=None,
):
    """Caller-pulled events; closing the iterator interrupts but never publishes."""
    session_id, run_id, turn_id = identities(request)
    args = (run_id, session_id, GRAPH_VERSION)
    sink = None
    runtime = None
    index_history = None
    with (
        session_owner(store_path(config), session_id),
        SQLiteOrchestrationStore(store_path(config)) as store,
    ):
        try:
            with SQLiteHistoryStore(config.database_path):
                pass
            try:
                bound = store.request(*args)
            except RunNotFoundError:
                if request["command"] != "orchestration_start":
                    raise ValueError(ERROR) from None
                cutoff, history, fingerprint = history_context(
                    config.database_path, session_id
                )
                bound = RunRequest(turn_id, request.get("message"), cutoff, fingerprint)
                store.create_run(
                    session_id, GRAPH_VERSION, run_id=run_id, request=bound
                )
            if bound.turn_id != turn_id or (
                request["command"] == "orchestration_start"
                and request.get("message") != bound.query
            ):
                raise ValueError(ERROR)
            _, history, fingerprint = history_context(
                config.database_path, session_id, bound.history_cutoff
            )
            if fingerprint != bound.history_hash:
                raise ValueError(ERROR)
            projection = Projection(store, args, turn_id)
            publisher = publisher_factory(
                config.database_path, args, bound, projection.sources, index
            )
            sink = StreamSink(
                projection,
                publisher,
                "real" if config.has_api_key else "mock",
                index is not None,
            )
            if executor is None:
                from .orchestration_workers import ProcessExecutor
                from .models import build_history_store, build_index

                executor = ProcessExecutor(config)
                index_history = build_history_store(config)
                index = build_index(config, index_history)
                # Index construction is optional; publication remains canonical.
                publisher.index = index
                sink.indexed = index is not None
            runtime = OrchestrationRuntime(
                store,
                *args,
                bound.query,
                executor=executor,
                retriever=retriever or EmptyRetriever(),
                publisher=publisher,
                sink=sink,
                history=history,
            )
            current = runtime.load()
            if request["command"] == "orchestration_resume":
                runtime.resume("transport:resume:" + str(uuid4()))
            elif current.sequence > 1 and current.run.status not in TERMINAL_RUNS:
                # A repeated start is a lookup, not implicit recovery.
                yield sink.begin(current)
                yield sink.error()
                return
            yield sink.begin(runtime.load())
            while not sink.validator.terminal:
                current = runtime.step()
                yield from sink.drain()
                if current.run.status is RunStatus.INTERRUPTED:
                    yield sink.error()
                if current.run.status in TERMINAL_RUNS:
                    break
        except Exception:
            if sink is not None and not sink.validator.terminal:
                yield sink.error()
            elif sink is None:
                event = dict(
                    version=1,
                    session_id=session_id,
                    run_id=run_id,
                    turn_id=turn_id,
                    sequence=1,
                    event="error",
                    code="invalid_stream",
                    message=ERROR,
                    resumable=False,
                )
                yield StreamValidator(session_id, run_id, turn_id).accept(event)
        finally:
            try:
                if runtime is not None:
                    runtime.close()
            finally:
                if index_history is not None:
                    index_history.close()


def stream(config, request, **adapters):
    """Sanitize even failures that occur before the run or its owner lock exists."""
    sequence = 0
    terminal = False
    identity = []
    for key in ("session_id", "run_id", "turn_id"):
        value = request.get(key)
        try:
            validate_type(value, "id")
        except ValueError:
            value = "invalid"
        identity.append(value)
    events = _stream(config, request, **adapters)
    try:
        for event in events:
            sequence = event["sequence"]
            terminal = event["event"] in ("done", "error")
            yield event
    except Exception:
        if not terminal:
            yield dict(
                version=1,
                session_id=identity[0],
                run_id=identity[1],
                turn_id=identity[2],
                sequence=sequence + 1,
                event="error",
                code="invalid_stream",
                message=ERROR,
                resumable=True,
            )
    finally:
        events.close()


def control(config, request, *, publisher_factory=CanonicalPublisher):
    """Recovery controls never construct models, retrievers, or search indices."""
    try:
        session_id = request.get("session_id")
        validate_type(session_id, "id")
        command = request.get("command")
        path = store_path(config)
        if command == "orchestration_pending" and not path.exists():
            return {"pending": None}
        with SQLiteOrchestrationStore(path) as store:
            if command == "orchestration_pending":
                pending = store.list_pending(session_id, GRAPH_VERSION)
                if len(pending) > 1:
                    raise ValueError(ERROR)
                if not pending:
                    return {"pending": None}
                current = pending[0]
            elif command == "orchestration_trace":
                exchange_id = request.get("exchange_id")
                validate_type(exchange_id, "exchange")
                with SQLiteHistoryStore(config.database_path) as history:
                    history.get_exchange(exchange_id, session_id)
                current = store.trace_for_exchange(
                    exchange_id, session_id, GRAPH_VERSION
                )
            elif command == "orchestration_discard":
                session_id, run_id, turn_id = identities(request)
                current = store.load(run_id, session_id, GRAPH_VERSION)
            else:
                raise ValueError(ERROR)
            args = (current.run.run_id, session_id, GRAPH_VERSION)
            bound = store.request(*args)
            projection = Projection(store, args, bound.turn_id)
            if command == "orchestration_discard":
                if bound.turn_id != turn_id:
                    raise ValueError(ERROR)
                publisher = publisher_factory(
                    config.database_path, args, bound, projection.sources
                )
                # Discard arbitrates with a live owner via the canonical tombstone;
                # the store fences late tool effects. No owner lock is needed.
                runtime = OrchestrationRuntime(
                    store,
                    *args,
                    bound.query,
                    executor=None,
                    retriever=None,
                    publisher=publisher,
                    sink=None,
                    history=history_context(
                        config.database_path, session_id, bound.history_cutoff
                    )[1],
                )
                try:
                    current = runtime.discard()
                finally:
                    runtime.close()
            event = projection.snapshot(current)
            StreamValidator(session_id, current.run.run_id, bound.turn_id).accept(event)
            return {
                "pending" if command == "orchestration_pending" else "snapshot": event
            }
    except Exception:
        raise ValueError(ERROR) from None


def emit_stream(config, request, emit):
    """The stdout boundary owns and closes the generator on a broken pipe."""
    events = stream(config, request)
    try:
        for event in events:
            emit(event)
        return 0
    except (BrokenPipeError, OSError):
        return 1
    finally:
        events.close()
