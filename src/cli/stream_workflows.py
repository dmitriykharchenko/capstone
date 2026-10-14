"""Pull-driven, ephemeral indexing and evaluation graphs.

Each invocation advances to exactly one public event without worker prefetch.
Evaluation checkpoints are run-local, with deferred rounds driven only by the
next caller pull. The existing indexer retains its bounded worker pool.
"""

from __future__ import annotations

from collections.abc import Callable, Iterator, Mapping
from dataclasses import dataclass, field
from functools import lru_cache
from typing import TYPE_CHECKING, Any, TypedDict

from ..evaluation.runner import JUDGE_CRITERIA, _raise_if_terminal, validate_test_cases
from ..evaluation.metrics import aggregate_metrics
from ..retrieval.paper_search import EVALUATION_RETRIEVAL_STRATEGIES

if TYPE_CHECKING:
    from langgraph.graph.state import CompiledStateGraph
    from langgraph.runtime import Runtime


class StreamState(TypedDict, total=False):
    phase: str
    event: dict[str, Any]
    total: int
    completed: int
    counts: dict[str, int]
    seen_start: bool
    retrieval_type: str
    cases: list[Any]
    case_input: dict[str, Any]
    succeeded: int
    failed: int
    score_totals: dict[str, float]
    retry_round: int
    pending: list[int]
    position: int
    results: dict[int, dict[str, Any]]


class CommandRuntimeError(RuntimeError):
    """Fixed public failure for stream graph infrastructure."""

    def __init__(self) -> None:
        super().__init__("Command runtime failed. Check backend logs and retry.")


class _OperationFailure(Exception):
    def __init__(self, error: Exception) -> None:
        super().__init__("Stream operation failed")
        self.error = error


def _invoke(graph_factory: Callable, state: StreamState, context: Any) -> StreamState:
    try:
        from langsmith import tracing_context

        with tracing_context(enabled=False):
            updated = graph_factory().invoke(
                {**state, "event": None},
                context=context,
                config={"callbacks": []},
            )
        if not isinstance(updated.get("event"), dict):
            raise CommandRuntimeError()
        return updated
    except _OperationFailure as exc:
        raise exc.error from None
    except Exception:
        raise CommandRuntimeError() from None


@dataclass
class IndexInvocation:
    events: Iterator[Any]
    started_events: bool
    persist: Callable[[Any], None] | None
    refresh: Callable[[], None] | None
    progress: Any = None


def _index_read(state: StreamState, runtime: Runtime[IndexInvocation]) -> dict:
    try:
        runtime.context.progress = next(runtime.context.events)
    except StopIteration:
        return {"phase": "finish"}
    except Exception as exc:
        raise _OperationFailure(exc) from None
    return {"phase": "progress"}


def _index_progress(state: StreamState, runtime: Runtime[IndexInvocation]) -> dict:
    context = runtime.context
    progress = context.progress
    update: dict[str, Any] = {"total": progress.total, "completed": progress.completed}
    if context.started_events and progress.started is not None:
        event = {
            "event": "started",
            "completed": progress.completed,
            "total": progress.total,
            "paper_id": progress.started.paper_id,
            "filename": progress.started.filename,
        }
    elif progress.outcome is None:
        event = {"event": "start", "completed": 0, "total": progress.total}
        update["seen_start"] = True
    else:
        outcome = progress.outcome
        counts = dict(state["counts"])
        counts[outcome.status] += 1
        update["counts"] = counts
        if outcome.status != "skipped" and context.persist is not None:
            context.persist(outcome)
        event = {
            "event": "progress",
            "completed": progress.completed,
            "total": progress.total,
            "paper_id": outcome.paper_id,
            "filename": outcome.filename,
            "status": outcome.status,
            "chunk_count": outcome.chunk_count,
            "error": outcome.error,
        }
    return {**update, "phase": "read", "event": event}


def _index_finish(state: StreamState, runtime: Runtime[IndexInvocation]) -> dict:
    if (
        not state["seen_start"]
        or state["completed"] != state["total"]
        or sum(state["counts"].values()) != state["total"]
    ):
        raise CommandRuntimeError()
    if runtime.context.refresh is not None:
        runtime.context.refresh()
    return {
        "phase": "end",
        "event": {
            "ok": True,
            "event": "done",
            "completed": state["completed"],
            "total": state["total"],
            **state["counts"],
        },
    }


@lru_cache(maxsize=1)
def get_index_graph() -> CompiledStateGraph:
    from langgraph.graph import END, START, StateGraph

    graph = StateGraph(StreamState, context_schema=IndexInvocation)
    graph.add_node("read", _index_read)
    graph.add_node("progress", _index_progress)
    graph.add_node("finish", _index_finish)
    graph.add_edge(START, "read")
    graph.add_conditional_edges(
        "read",
        lambda state: state["phase"],
        {"progress": "progress", "finish": "finish"},
    )
    graph.add_edge("progress", END)
    graph.add_edge("finish", END)
    return graph.compile(name="cli_index_stream")


def index_events(
    events: Iterator[Any],
    *,
    counts: dict[str, int],
    started_events: bool = False,
    persist: Callable[[Any], None] | None = None,
    refresh: Callable[[], None] | None = None,
) -> Iterator[dict[str, Any]]:
    context = IndexInvocation(events, started_events, persist, refresh)
    state: StreamState = {
        "phase": "read",
        "total": 0,
        "completed": 0,
        "counts": counts,
        "seen_start": False,
    }
    try:
        while state["phase"] != "end":
            state = _invoke(get_index_graph, state, context)
            yield state["event"]
    finally:
        close = getattr(events, "close", None)
        if close is not None:
            close()


@dataclass
class EvaluationInvocation:
    config: Any
    request: dict[str, Any]
    evaluator: Any
    build_runner: Callable[[Any], Any]
    checkpoints: dict[int, Any] = field(default_factory=dict)


def _evaluation_prepare(
    state: StreamState, runtime: Runtime[EvaluationInvocation]
) -> dict:
    from . import commands

    context = runtime.context
    try:
        retrieval_type = commands._require_str(
            context.request.get("retrieval_type"), "retrieval_type"
        )
        if retrieval_type not in EVALUATION_RETRIEVAL_STRATEGIES:
            allowed = ", ".join(EVALUATION_RETRIEVAL_STRATEGIES)
            raise ValueError(f"retrieval_type must be one of: {allowed}")
        cases = validate_test_cases(context.request.get("test_cases"))
        if context.evaluator is None:
            try:
                context.evaluator = context.build_runner(context.config)
            except Exception as exc:
                raise RuntimeError(
                    "Evaluation backend is unavailable. Check backend configuration and retry."
                ) from exc
    except Exception as exc:
        raise _OperationFailure(exc) from None
    return {
        "phase": "case_start",
        "retrieval_type": retrieval_type,
        "cases": cases,
        "total": len(cases),
        "completed": 0,
        "succeeded": 0,
        "failed": 0,
        "score_totals": {criterion: 0.0 for criterion in JUDGE_CRITERIA},
        "retry_round": 0,
        "pending": list(range(len(cases))),
        "position": 0,
        "results": {},
        "event": {
            "event": "run_start",
            "retrieval_type": retrieval_type,
            "total": len(cases),
        },
    }


def _evaluation_start(state: StreamState) -> dict:
    index = state["pending"][state["position"]]
    case_input = state["cases"][index].as_dict()
    if state["retry_round"]:
        return {
            "phase": "case_result",
            "case_input": case_input,
            "event": {
                "event": "case_retry_start",
                "case_index": index + 1,
                "total": state["total"],
                "retry_round": state["retry_round"],
            },
        }
    return {
        "phase": "case_result",
        "case_input": case_input,
        "event": {
            "event": "case_start",
            "case_index": index + 1,
            "total": state["total"],
            "retrieval_type": state["retrieval_type"],
            "input": case_input,
        },
    }


def _evaluation_result(
    state: StreamState, runtime: Runtime[EvaluationInvocation]
) -> dict:
    from . import commands

    index = state["pending"][state["position"]]
    case_index = index + 1
    case_input = state["case_input"]
    retrieval_type = state["retrieval_type"]
    context = runtime.context
    try:
        if callable(getattr(context.evaluator, "start_case", None)) and callable(
            getattr(context.evaluator, "resume_case", None)
        ):
            if index not in context.checkpoints:
                context.checkpoints[index] = context.evaluator.start_case(
                    case_index, state["cases"][index], retrieval_type
                )
            result = context.evaluator.resume_case(
                context.checkpoints[index], state["retry_round"]
            )
        else:
            result = context.evaluator.run([case_input], retrieval_type)[0]
        result = {**result, "case_index": case_index}
    except Exception as exc:  # noqa: BLE001 - preserve per-case continuation
        _raise_if_terminal(exc)
        result = {
            "case_index": case_index,
            "input": case_input,
            "retrieval_strategy": retrieval_type,
            "retrieved_chunks": [],
            "generated_answer": None,
            "judge_scores": None,
            "error": {
                "stage": "evaluation",
                "message": commands._evaluation_error_message("evaluation"),
            },
        }
    result = commands._sanitize_evaluation_result(result)
    results = {**state["results"], index: result}
    successful = [
        item
        for item in results.values()
        if item.get("error") is None and isinstance(item.get("judge_scores"), Mapping)
    ]
    succeeded = len(successful)
    failed = len(results) - succeeded
    totals = {
        criterion: sum(
            float(item["judge_scores"][criterion]["score"]) for item in successful
        )
        for criterion in JUDGE_CRITERIA
    }
    event = "case_success" if result.get("error") is None else "case_failure"
    completed = len(results)
    position = state["position"] + 1
    retryable_failures = [
        item
        for item in results.values()
        if item.get("error") is not None
    ]
    more_rounds = bool(retryable_failures) and state["retry_round"] < 2
    phase = (
        "case_start"
        if position < len(state["pending"])
        else ("retry_round" if more_rounds else "complete")
    )
    return {
        "phase": phase,
        "completed": completed,
        "position": position,
        "results": results,
        "succeeded": succeeded,
        "failed": failed,
        "score_totals": totals,
        "event": {
            "event": "case_retry_result" if state["retry_round"] else event,
            **({"retry_round": state["retry_round"]} if state["retry_round"] else {}),
            "completed": completed,
            "total": state["total"],
            "result": result,
        },
    }


def _evaluation_retry_round(state: StreamState) -> dict:
    pending = [
        index
        for index in range(state["total"])
        if state["results"][index].get("error") is not None
    ]
    retry_round = state["retry_round"] + 1
    return {
        "phase": "case_start",
        "retry_round": retry_round,
        "pending": pending,
        "position": 0,
        "event": {
            "event": "retry_round_start",
            "retry_round": retry_round,
            "total": state["total"],
            "pending": len(pending),
        },
    }


def _evaluation_complete(state: StreamState) -> dict:
    return {
        "phase": "end",
        "event": {
            "event": "complete",
            "retrieval_type": state["retrieval_type"],
            "completed": state["total"],
            "total": state["total"],
            "succeeded": state["succeeded"],
            "failed": state["failed"],
            "aggregate_metrics": aggregate_metrics(list(state["results"].values())),
            "aggregate_scores": {
                criterion: total / state["succeeded"] if state["succeeded"] else None
                for criterion, total in state["score_totals"].items()
            },
        },
    }


@lru_cache(maxsize=1)
def get_evaluation_graph() -> CompiledStateGraph:
    from langgraph.graph import END, START, StateGraph

    graph = StateGraph(StreamState, context_schema=EvaluationInvocation)
    nodes = {
        "prepare": _evaluation_prepare,
        "case_start": _evaluation_start,
        "case_result": _evaluation_result,
        "retry_round": _evaluation_retry_round,
        "complete": _evaluation_complete,
    }
    for name, node in nodes.items():
        graph.add_node(name, node)
        graph.add_edge(name, END)
    graph.add_conditional_edges(
        START, lambda state: state["phase"], {name: name for name in nodes}
    )
    return graph.compile(name="cli_evaluation_stream")


def evaluation_events(
    config: Any,
    request: dict[str, Any],
    runner: Any,
    build_runner: Callable[[Any], Any],
) -> Iterator[dict[str, Any]]:
    context = EvaluationInvocation(config, request, runner, build_runner)
    state: StreamState = {"phase": "prepare"}
    while state["phase"] != "end":
        state = _invoke(get_evaluation_graph, state, context)
        yield state["event"]
