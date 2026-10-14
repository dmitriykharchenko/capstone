"""Opt-in, bounded planner retries; never inspect or retain provider error text."""

from __future__ import annotations

import math
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from email.utils import parsedate_to_datetime
from functools import lru_cache
from typing import Any

MAX_PLANNER_ATTEMPTS = 3
MAX_PLANNER_RETRY_DELAY = 8.0
PLAN_CORRECTION = (
    'Return only a strict JSON object with exactly one key, "subqueries", '
    "containing 1 to 5 distinct, non-empty evidence-seeking questions. Use exactly "
    "1 question for a single fact from one paper; never split one fact into "
    "reworded questions. Do not include duplicate keys, duplicate questions, extra "
    "fields, Markdown, or explanatory text. Return the complete object."
)


@lru_cache(maxsize=1)
def _provider_error_types() -> tuple[
    tuple[type, ...], tuple[type, ...], tuple[type, ...]
]:
    connection: tuple[type, ...] = (TimeoutError, ConnectionError)
    status: tuple[type, ...] = ()
    incomplete: tuple[type, ...] = ()
    try:
        import openai
    except ImportError:
        pass
    else:
        connection += (openai.APIConnectionError, openai.APITimeoutError)
        status += (openai.APIStatusError,)
        incomplete += (openai.LengthFinishReasonError,)
    try:
        import httpx
    except ImportError:
        pass
    else:
        connection += (httpx.TimeoutException, httpx.NetworkError)
        status += (httpx.HTTPStatusError,)
    return connection, status, incomplete


@dataclass(frozen=True, slots=True)
class PlannerRetryPolicy:
    """Evaluation-only policy with deterministic 1s/2s backoff and an 8s cap."""

    max_attempts: int = MAX_PLANNER_ATTEMPTS
    sleep: Callable[[float], None] = time.sleep
    clock: Callable[[], float] = time.time

    def __post_init__(self) -> None:
        if (
            type(self.max_attempts) is not int
            or not 1 <= self.max_attempts <= MAX_PLANNER_ATTEMPTS
        ):
            raise ValueError("planner attempts must be between 1 and 3")
        if not callable(self.sleep) or not callable(self.clock):
            raise TypeError("planner sleeper and clock must be callable")

    def delay(self, attempt: int, retry_after: str | None = None) -> float:
        delay = min(2.0 ** min(max(attempt - 1, 0), 3), MAX_PLANNER_RETRY_DELAY)
        if not isinstance(retry_after, str) or len(retry_after) > 128:
            return delay
        value = retry_after.strip()
        try:
            if value.isascii() and value.isdecimal():
                seconds = float(value)
            else:
                date = parsedate_to_datetime(value)
                if date.tzinfo is None:
                    return delay
                seconds = date.timestamp() - self.clock()
        except (TypeError, ValueError, OverflowError):
            return delay
        if not math.isfinite(seconds) or seconds < 0:
            return delay
        return min(max(delay, seconds), MAX_PLANNER_RETRY_DELAY)

    def provider_delay(self, error: Exception, attempt: int) -> float | None:
        connection_types, status_types, _ = _provider_error_types()
        if isinstance(error, connection_types):
            return self.delay(attempt)
        if not isinstance(error, status_types):
            return None
        response = getattr(error, "response", None)
        status = getattr(error, "status_code", None)
        if status is None:
            status = getattr(response, "status_code", None)
        if type(status) is not int or not (
            status in {408, 409, 429} or 500 <= status <= 599
        ):
            return None
        headers = getattr(response, "headers", None)
        retry_after = (
            headers.get("retry-after") if isinstance(headers, Mapping) else None
        )
        return self.delay(attempt, retry_after)

    def incomplete(self, error: Exception) -> bool:
        return isinstance(error, _provider_error_types()[2])


def planner_refused(response: Any) -> bool:
    """Recognize structured refusal metadata before considering schema retries."""

    def refused(value: Any) -> bool:
        if isinstance(value, Mapping):
            if value.get("refusal") or value.get("type") == "refusal":
                return True
            if any(
                value.get(key) in ("content_filter", "refusal")
                for key in ("finish_reason", "stop_reason", "reason")
            ):
                return True
            return any(refused(item) for item in value.values())
        if isinstance(value, Sequence) and not isinstance(value, (str, bytes)):
            return any(refused(item) for item in value)
        return False

    return any(
        refused(value)
        for value in (
            response,
            getattr(response, "content", None),
            getattr(response, "additional_kwargs", None),
            getattr(response, "response_metadata", None),
            getattr(response, "generation_info", None),
        )
    )
