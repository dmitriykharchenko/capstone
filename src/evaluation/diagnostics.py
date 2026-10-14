"""Allowlisted evaluation diagnostics: never stringify exceptions or provider data."""

from __future__ import annotations

import asyncio
from concurrent.futures import CancelledError
from functools import lru_cache
from typing import Any

from ..chat.agent_loop import (
    AgentConfigurationError,
    AgentIncompleteResponseError,
    AgentModelError,
    AgentPlanError,
    AgentRetrievalError,
)

REASONS = {
    "payment_required": (
        "Provider payment is required. Check billing before retrying.",
        False,
    ),
    "authentication": (
        "Provider authentication failed. Check credentials before retrying.",
        False,
    ),
    "authorization": (
        "Provider access was denied. Check permissions before retrying.",
        False,
    ),
    "rate_limit": ("Provider rate limit reached. Try again later.", True),
    "timeout": ("The operation timed out. Try again later.", True),
    "connection": (
        "The provider could not be reached. Check connectivity and retry.",
        True,
    ),
    "http_server_error": (
        "The provider reported a server error. Try again later.",
        True,
    ),
    "http_client_error": (
        "The provider rejected the request. Check configuration before retrying.",
        False,
    ),
    "incomplete_response": (
        "The provider response was incomplete. Check output limits and retry.",
        True,
    ),
    "refusal": ("The provider declined to answer this request.", False),
    "invalid_planner_output": (
        "The planner returned an invalid plan. Try again.",
        True,
    ),
    "retrieval_failure": ("Retrieval failed. Check the paper index and retry.", False),
    "invalid_judge_output": ("The judge returned invalid scores. Try again.", True),
    "cancellation": ("Evaluation was cancelled.", False),
    "configuration": (
        "Evaluation configuration is invalid. Check configuration before retrying.",
        False,
    ),
    "unknown": (
        "Evaluation failed for an unknown reason. Check configuration and retry.",
        False,
    ),
}
STAGES = frozenset(
    {
        "planner",
        "retrieval",
        "subanswer",
        "coverage",
        "followup_retrieval",
        "followup_subanswer",
        "composer",
        "judge",
        "model",
        "evaluation",
    }
)
HOP_STAGES = frozenset(
    {"retrieval", "subanswer", "followup_retrieval", "followup_subanswer"}
)
FIELDS = frozenset(
    {
        "stage",
        "category",
        "http_status",
        "retryable",
        "retry_round",
        "stage_attempt",
        "hop_index",
        "message",
    }
)
Reason = tuple[str, int | None]


class EvaluationRefusalError(AgentModelError):
    """Typed evaluation-only refusal; no rejected output is retained."""


class SafeStageFailure(Exception):
    def __init__(self, reason: Reason) -> None:
        super().__init__(REASONS[reason[0]][0])
        self.reason = reason


@lru_cache(maxsize=1)
def _types() -> tuple[tuple[type, ...], ...]:
    timeout: tuple[type, ...] = (TimeoutError,)
    connection: tuple[type, ...] = (ConnectionError,)
    status: tuple[type, ...] = ()
    incomplete: tuple[type, ...] = (AgentIncompleteResponseError,)
    refusal: tuple[type, ...] = (EvaluationRefusalError,)
    try:
        import openai
    except ImportError:
        pass
    else:
        timeout += (openai.APITimeoutError,)
        connection += (openai.APIConnectionError,)
        status += (openai.APIStatusError,)
        incomplete += (openai.LengthFinishReasonError,)
        refusal += (openai.ContentFilterFinishReasonError,)
    try:
        import httpx
    except ImportError:
        pass
    else:
        timeout += (httpx.TimeoutException,)
        connection += (httpx.NetworkError,)
        status += (httpx.HTTPStatusError,)
    return timeout, connection, status, incomplete, refusal


def status_category(status: int) -> str:
    return {
        401: "authentication",
        402: "payment_required",
        403: "authorization",
        408: "timeout",
        429: "rate_limit",
    }.get(status, "http_server_error" if status >= 500 else "http_client_error")


def classify_failure(error: BaseException, stage: str) -> Reason:
    """Read only trusted exception types and numeric status; ignore text/headers/body."""
    timeout, connection, status_types, incomplete, refusal = _types()
    seen: set[int] = set()
    current: BaseException | None = error
    for _ in range(8):
        if current is None or id(current) in seen:
            break
        seen.add(id(current))
        if isinstance(current, SafeStageFailure):
            return current.reason
        if isinstance(current, (CancelledError, asyncio.CancelledError)):
            return "cancellation", None
        if isinstance(current, timeout):
            return "timeout", None
        if isinstance(current, connection):
            return "connection", None
        if isinstance(current, status_types):
            status = getattr(current, "status_code", None)
            if status is None:
                status = getattr(
                    getattr(current, "response", None), "status_code", None
                )
            if type(status) is int and 400 <= status <= 599:
                return status_category(status), status
        if isinstance(current, incomplete):
            return "incomplete_response", None
        if isinstance(current, refusal):
            return "refusal", None
        if isinstance(current, AgentConfigurationError):
            return "configuration", None
        current = current.__cause__
    if isinstance(error, AgentPlanError):
        return "invalid_planner_output", None
    if isinstance(error, AgentRetrievalError) or stage in {
        "retrieval",
        "followup_retrieval",
    }:
        return "retrieval_failure", None
    return "unknown", None


def failure_record(
    stage: str,
    reason: Reason,
    retry_round: int,
    stage_attempt: int,
    hop_index: int | None = None,
) -> dict[str, Any]:
    category, status = reason
    message, retryable = REASONS[category]
    return {
        "stage": stage,
        "category": category,
        "http_status": status,
        "retryable": retryable or status == 409,
        "retry_round": retry_round,
        "stage_attempt": stage_attempt,
        "hop_index": hop_index,
        "message": message,
    }


def valid_record(value: Any) -> bool:
    if not isinstance(value, dict) or set(value) != FIELDS:
        return False
    stage, category, status = value["stage"], value["category"], value["http_status"]
    if (
        not isinstance(stage, str)
        or stage not in STAGES
        or not isinstance(category, str)
        or category not in REASONS
    ):
        return False
    if status is not None and (
        type(status) is not int
        or not 400 <= status <= 599
        or status_category(status) != category
    ):
        return False
    if (
        category
        in {
            "payment_required",
            "authentication",
            "authorization",
            "rate_limit",
            "http_server_error",
            "http_client_error",
        }
        and status is None
    ):
        return False
    hop = value["hop_index"]
    return (
        type(value["retry_round"]) is int
        and 0 <= value["retry_round"] <= 2
        and type(value["stage_attempt"]) is int
        and 1 <= value["stage_attempt"] <= value["retry_round"] + 1
        and type(value["retryable"]) is bool
        and value["retryable"] == (REASONS[category][1] or status == 409)
        and (
            hop is None
            if stage not in HOP_STAGES
            else type(hop) is int and 1 <= hop <= 7
        )
        and isinstance(value["message"], str)
    )


def sanitize_record(value: Any) -> dict[str, Any]:
    if not valid_record(value):
        return failure_record("evaluation", ("unknown", None), 0, 1)
    return failure_record(
        value["stage"],
        (value["category"], value["http_status"]),
        value["retry_round"],
        value["stage_attempt"],
        value["hop_index"],
    )


def sanitize_result_diagnostics(result: dict[str, Any]) -> dict[str, Any]:
    """Rebuild diagnostics at publication, retaining safe legacy wire compatibility."""
    from .metrics import sanitize_metrics, sanitize_performance

    metrics = sanitize_metrics(result.get("metrics"))
    performance = sanitize_performance(result.get("performance"))
    result = {
        key: value
        for key, value in result.items()
        if key
        in {
            "case_index",
            "input",
            "retrieval_strategy",
            "retrieved_chunks",
            "generated_answer",
            "judge_scores",
            "error",
            "attempt_history",
        }
    }
    if metrics is not None:
        result["metrics"] = metrics
    if performance is not None:
        result["performance"] = performance
    history = result.get("attempt_history", [])
    error = result.get("error")
    if isinstance(error, dict) and set(error) == {"stage", "message"} and history == []:
        stage = error["stage"]
        stage = stage if isinstance(stage, str) and stage in STAGES else "evaluation"
        label = {
            "planner": "Question planning",
            "retrieval": "Retrieval",
            "subanswer": "Subanswer generation",
            "coverage": "Coverage review",
            "followup_retrieval": "Follow-up retrieval",
            "followup_subanswer": "Follow-up subanswer generation",
            "composer": "Final answer composition",
            "model": "Answer generation",
            "judge": "Judge evaluation",
            "evaluation": "Evaluation",
        }[stage]
        target = (
            "the paper index"
            if stage in {"retrieval", "followup_retrieval"}
            else "backend configuration"
        )
        result["error"] = {
            "stage": stage,
            "message": f"{label} failed for this case. Check {target} and retry.",
        }
        result["attempt_history"] = []
        return result
    counts: dict[tuple[str, int | None], int] = {}
    valid = isinstance(history, list) and len(history) <= 3
    if valid:
        for index, record in enumerate(history):
            if not valid_record(record) or record["retry_round"] != index:
                valid = False
                break
            key = (record["stage"], record["hop_index"])
            counts[key] = counts.get(key, 0) + 1
            if counts[key] != record["stage_attempt"]:
                valid = False
                break
    if error is not None:
        valid = (
            valid
            and valid_record(error)
            and bool(history)
            and sanitize_record(error) == sanitize_record(history[-1])
        )
    else:
        valid = valid and len(history) <= 2
    result["attempt_history"] = (
        [sanitize_record(record) for record in history] if valid else []
    )
    result["error"] = (
        (
            sanitize_record(error)
            if valid
            else failure_record("evaluation", ("unknown", None), 0, 1)
        )
        if error is not None
        else None
    )
    return result
