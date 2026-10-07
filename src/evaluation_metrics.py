"""Numeric-only evaluation telemetry; no prompts, provider objects or errors retained.

Calls are application invocations (not hidden SDK retries). Active latency excludes
time between resume calls. Evidence sizes count submissions, not retrieval results;
retries count again. An exchange is a returned RAG model response, even if invalid.
"""

from __future__ import annotations

import math
import os
from collections.abc import Callable, Mapping, Sequence
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import asdict, dataclass, field, fields, replace
from time import perf_counter
from typing import Any


def _get(value: Any, key: str, default: Any = None) -> Any:
    return (
        value.get(key, default)
        if isinstance(value, Mapping)
        else getattr(value, key, default)
    )


def _number(value: Any, *, integer: bool = False) -> int | float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    if not 0 <= value <= 2**53 - 1 or (integer and not isinstance(value, int)):
        return None
    return value


@dataclass(frozen=True)
class EvaluationPricing:
    """Explicit USD rates, never guessed from a model name or an endpoint."""

    input_usd_per_million: float | None = None
    output_usd_per_million: float | None = None
    embedding_usd_per_million: float | None = None

    def __post_init__(self) -> None:
        if any(v is not None and _number(v) is None for v in asdict(self).values()):
            raise ValueError("evaluation prices must be finite non-negative numbers")

    @classmethod
    def from_env(cls) -> EvaluationPricing:
        values = []
        for kind in ("INPUT", "OUTPUT", "EMBEDDING"):
            name = f"CAPSTONE_EVAL_{kind}_USD_PER_MILLION"
            raw = os.getenv(name)
            try:
                value = None if raw is None or not raw.strip() else float(raw)
            except ValueError:
                raise ValueError(
                    f"{name} must be a finite non-negative number"
                ) from None
            if value is not None and _number(value) is None:
                raise ValueError(f"{name} must be a finite non-negative number")
            values.append(value)
        return cls(*values)


def _usage(response: Any, pricing: EvaluationPricing, embedding: bool) -> dict:
    metadata = _get(response, "response_metadata", {}) or {}
    raw = (
        _get(metadata, "token_usage")
        or _get(metadata, "usage")
        or _get(response, "usage", {})
        or {}
    )
    normalized = _get(response, "usage_metadata", {}) or {}
    counts = {}
    for name, alias in (
        ("input", "prompt"),
        ("output", "completion"),
        ("total", "total"),
    ):
        value = _number(_get(normalized, f"{name}_tokens"), integer=True)
        if value is None:
            value = _number(_get(raw, f"{alias}_tokens"), integer=True)
        counts[name] = value
    if embedding:
        counts["output"] = 0
    if (
        counts["total"] is None
        and counts["input"] is not None
        and counts["output"] is not None
    ):
        counts["total"] = counts["input"] + counts["output"]
    elif (
        all(counts[n] is not None for n in ("input", "output", "total"))
        and counts["total"] != counts["input"] + counts["output"]
    ):
        counts["total"] = None
    cost = _number(_get(raw, "cost"))
    source = "provider" if cost is not None else None
    input_rate = (
        pricing.embedding_usd_per_million
        if embedding
        else pricing.input_usd_per_million
    )
    output_rate = 0 if embedding else pricing.output_usd_per_million
    if cost is None and all(
        v is not None
        for v in (input_rate, output_rate, counts["input"], counts["output"])
    ):
        cost = (
            counts["input"] * input_rate + counts["output"] * output_rate
        ) / 1_000_000
        if _number(cost) is not None:
            source = "rates"
        else:
            cost = None
    return {**counts, "cost": cost, "source": source}


def _totals(calls: Sequence[dict]) -> tuple[dict, dict]:
    tokens = {}
    for name in ("input", "output", "total"):
        values = [call[name] for call in calls if call[name] is not None]
        tokens[name] = sum(values) if len(values) == len(calls) else None
        tokens[f"known_{name}"] = sum(values)
    tokens["reported_calls"] = sum(
        all(call[n] is not None for n in ("input", "output", "total")) for call in calls
    )
    priced = [call["cost"] for call in calls if call["cost"] is not None]
    return tokens, {
        "estimated_usd": sum(priced) if len(priced) == len(calls) else None,
        "known_usd": sum(priced),
        "priced_calls": len(priced),
        "provider_reported_calls": sum(call["source"] == "provider" for call in calls),
        "rate_estimated_calls": sum(call["source"] == "rates" for call in calls),
    }


@dataclass
class _Context:
    chunks: int = 0
    characters: int = 0
    utf8_bytes: int = 0
    max_chunks: int = 0
    max_characters: int = 0
    prompt_characters: int = 0
    max_prompt_characters: int = 0
    identities: set[tuple[Any, Any]] = field(default_factory=set, repr=False)
    texts: set[str] = field(default_factory=set, repr=False)
    duplicate_chunks: int = 0

    def add(
        self,
        messages: Sequence[Any],
        text: str,
        chunks: Sequence[Any],
        *,
        reused: bool = False,
    ) -> None:
        size = sum(
            len(content)
            for message in messages
            if isinstance(content := _get(message, "content"), str)
        )
        self.prompt_characters += size
        self.max_prompt_characters = max(self.max_prompt_characters, size)
        self.chunks += len(chunks)
        self.characters += len(text)
        self.utf8_bytes += len(text.encode("utf-8"))
        self.max_chunks = max(self.max_chunks, len(chunks))
        self.max_characters = max(self.max_characters, len(text))
        for chunk in chunks:
            identity = (_get(chunk, "paper_id"), _get(chunk, "chunk_index"))
            normalized = " ".join(str(_get(chunk, "text", "")).split()).casefold()
            if not reused and (identity in self.identities or normalized in self.texts):
                self.duplicate_chunks += 1
            self.identities.add(identity)
            self.texts.add(normalized)

    def snapshot(self) -> dict:
        return {
            **{
                name: getattr(self, name)
                for name in self.__dataclass_fields__
                if name not in {"identities", "texts"}
            },
            "unique_chunks": len(self.identities),
        }


@dataclass
class _Stage:
    latency_ms: float = 0
    calls: list[dict] = field(default_factory=list)
    exchanges: int = 0
    tool_calls: int = 0
    context: _Context = field(default_factory=_Context)

    def snapshot(self) -> dict:
        tokens, cost = _totals(self.calls)
        return {
            "latency_ms": self.latency_ms,
            "model_calls": len(self.calls),
            "exchanges": self.exchanges,
            "tool_calls": self.tool_calls,
            "tokens": tokens,
            "cost": cost,
            "context": self.context.snapshot(),
        }


_ACTIVE: ContextVar[tuple[Any, str] | None] = ContextVar(
    "evaluation_metrics", default=None
)


@dataclass
class EvaluationMetrics:
    pricing: EvaluationPricing = field(default_factory=EvaluationPricing)
    clock: Callable[[], float] = field(default=perf_counter, repr=False)
    stages: dict[str, _Stage] = field(default_factory=dict)
    context: _Context = field(default_factory=_Context)
    rag_calls: int = 0
    judge_calls: int = 0
    embedding_calls: int = 0
    reused_searches: int = 0

    @contextmanager
    def stage(self, name: str):
        bucket = self.stages.setdefault(name, _Stage())
        token = _ACTIVE.set((self, name))
        start = self.clock()
        try:
            yield
        finally:
            bucket.latency_ms += max(0, self.clock() - start) * 1000
            _ACTIVE.reset(token)

    def call(
        self,
        name: str,
        invoke: Callable[[], Any],
        *,
        embedding: bool = False,
        messages: Sequence[Any] = (),
        context: str = "",
        chunks: Sequence[Any] = (),
        reused: bool = False,
    ) -> Any:
        bucket = self.stages.setdefault(name, _Stage())
        if embedding:
            self.embedding_calls += 1
        elif name == "judge":
            self.judge_calls += 1
        else:
            self.rag_calls += 1
        if not embedding:
            bucket.context.add(messages, context, chunks, reused=reused)
            if name != "judge":
                self.context.add(messages, context, chunks, reused=reused)
        response = None
        try:
            response = invoke()
            if not embedding:
                bucket.exchanges += 1
            return response
        finally:
            bucket.calls.append(_usage(response, self.pricing, embedding))

    def snapshot(self) -> dict:
        calls = [call for stage in self.stages.values() for call in stage.calls]
        tokens, cost = _totals(calls)
        total_ms = sum(stage.latency_ms for stage in self.stages.values())
        judge_ms = self.stages.get("judge", _Stage()).latency_ms
        return {
            "version": 1,
            "latency_ms": {
                "total": total_ms,
                "rag": total_ms - judge_ms,
                "judge": judge_ms,
            },
            "model_calls": {
                "total": len(calls),
                "rag": self.rag_calls,
                "judge": self.judge_calls,
                "embedding": self.embedding_calls,
            },
            "tokens": tokens,
            "cost": cost,
            "workflow": {
                "exchanges": sum(
                    s.exchanges for n, s in self.stages.items() if n != "judge"
                ),
                "tool_calls": sum(s.tool_calls for s in self.stages.values()),
                "reused_searches": self.reused_searches,
            },
            "context": self.context.snapshot(),
            "stages": {name: stage.snapshot() for name, stage in self.stages.items()},
            "pricing": asdict(self.pricing),
        }


@dataclass
class MeasuredModel:
    model: Any
    metrics: EvaluationMetrics
    stage: str
    context: str = ""
    chunks: Sequence[Any] = ()
    reused: bool = False

    def invoke(self, messages: Sequence[Any]) -> Any:
        return self.metrics.call(
            self.stage,
            lambda: self.model.invoke(messages),
            messages=messages,
            context=self.context,
            chunks=self.chunks,
            reused=self.reused,
        )


@dataclass
class MeasuredRetriever:
    retriever: Any
    metrics: EvaluationMetrics
    stage: str

    def retrieve_chunks(self, *args: Any, **kwargs: Any) -> Any:
        self.metrics.stages.setdefault(self.stage, _Stage()).tool_calls += 1
        return self.retriever.retrieve_chunks(*args, **kwargs)


def observe_embedding(invoke: Callable[[], Any]) -> Any:
    """No-op outside an evaluation stage; capture SDK usage before vector extraction."""
    active = _ACTIVE.get()
    return (
        invoke()
        if active is None
        else active[0].call(active[1], invoke, embedding=True)
    )


_PERFORMANCE_CHECKS = {
    "max_rag_latency_ms": "rag_latency_ms",
    "max_rag_model_calls": "rag_model_calls",
    "max_retrievals": "retrievals",
    "max_followup_queries": "followup_queries",
    "max_rag_tokens": "rag_tokens",
    "max_cost_usd": "rag_cost_usd",
    "max_duplicate_chunks": "duplicate_chunks",
}
_FRACTIONAL_LIMITS = {"max_rag_latency_ms", "max_cost_usd"}
_CHECK_STATUSES = {"pass", "fail", "unknown"}
_PERFORMANCE_STATUSES = {"pass", "fail", "incomplete"}


@dataclass(frozen=True)
class PerformanceBudget:
    """RAG-only limits checked per case; judge work is excluded, retries count.

    ``None`` disables a check. Defaults admit one fully bounded workflow:
    three planner attempts, seven hop subanswers, coverage and composer.
    """

    max_rag_latency_ms: float | None = 60_000
    max_rag_model_calls: int | None = 12
    max_retrievals: int | None = 7
    max_followup_queries: int | None = 2
    max_rag_tokens: int | None = None
    max_cost_usd: float | None = None
    max_duplicate_chunks: int | None = 0

    def __post_init__(self) -> None:
        for name, value in asdict(self).items():
            if value is not None and (
                _number(value, integer=name not in _FRACTIONAL_LIMITS) is None
            ):
                raise ValueError(f"{name} must be a non-negative number or null")

    @classmethod
    def overrides(cls, value: Any) -> tuple[tuple[str, int | float], ...]:
        """Validate a per-case JSON object of numeric limit overrides."""
        if not isinstance(value, Mapping) or not value:
            raise ValueError("performance_budget must be a non-empty JSON object")
        unknown = set(value) - {item.name for item in fields(cls)}
        if unknown or any(item is None for item in value.values()):
            raise ValueError("performance_budget has unknown or empty limits")
        cls(**value)
        return tuple(sorted(value.items()))

    def merged(
        self, overrides: Sequence[tuple[str, int | float]]
    ) -> PerformanceBudget:
        return replace(self, **dict(overrides))


def _rag_total(snapshot: Mapping[str, Any], section: str, key: str) -> Any:
    values = [
        stage[section][key]
        for name, stage in snapshot["stages"].items()
        if name != "judge"
    ]
    return None if any(v is None for v in values) else sum(values)


def check_performance(
    snapshot: Mapping[str, Any],
    budget: PerformanceBudget,
    *,
    followup_queries: int | None,
    completed: bool,
) -> dict:
    """Compare one case's metrics snapshot with its budget, failing on any breach.

    A check is ``unknown`` when the provider did not report the value; the case is
    then ``incomplete`` rather than passing. Unfinished cases are never ``pass``.
    """
    values = {
        "rag_latency_ms": snapshot["latency_ms"]["rag"],
        "rag_model_calls": snapshot["model_calls"]["rag"],
        "retrievals": snapshot["workflow"]["tool_calls"],
        "followup_queries": followup_queries,
        "rag_tokens": _rag_total(snapshot, "tokens", "total"),
        "rag_cost_usd": _rag_total(snapshot, "cost", "estimated_usd"),
        "duplicate_chunks": snapshot["context"]["duplicate_chunks"],
    }
    checks = {}
    for limit_name, check_name in _PERFORMANCE_CHECKS.items():
        limit = getattr(budget, limit_name)
        if limit is None:
            continue
        value = values[check_name]
        checks[check_name] = {
            "value": value,
            "limit": limit,
            "status": (
                "unknown" if value is None else "pass" if value <= limit else "fail"
            ),
        }
    statuses = {check["status"] for check in checks.values()}
    status = (
        "fail"
        if "fail" in statuses
        else "incomplete"
        if "unknown" in statuses or not completed
        else "pass"
    )
    return {"version": 1, "status": status, "checks": checks}


def sanitize_performance(value: Any) -> dict | None:
    """Accept only the fixed performance-check schema."""
    if (
        not isinstance(value, Mapping)
        or set(value) != {"version", "status", "checks"}
        or value["version"] != 1
        or isinstance(value["version"], bool)
        or value["status"] not in _PERFORMANCE_STATUSES
        or not isinstance(value["checks"], Mapping)
        or not set(value["checks"]) <= set(_PERFORMANCE_CHECKS.values())
    ):
        return None
    limits = {check: limit for limit, check in _PERFORMANCE_CHECKS.items()}
    checks = {}
    for name, check in value["checks"].items():
        integer = limits[name] not in _FRACTIONAL_LIMITS
        if (
            not isinstance(check, Mapping)
            or set(check) != {"value", "limit", "status"}
            or check["status"] not in _CHECK_STATUSES
            or _number(check["limit"], integer=integer) is None
            or (check["value"] is None) != (check["status"] == "unknown")
            or (
                check["value"] is not None
                and _number(check["value"], integer=integer) is None
            )
        ):
            return None
        checks[name] = {key: check[key] for key in ("value", "limit", "status")}
    return {"version": 1, "status": value["status"], "checks": checks}


def sanitize_metrics(value: Any) -> dict | None:
    """Accept only the fixed numeric schema, never an arbitrary telemetry payload."""
    template = EvaluationMetrics().snapshot()
    stage_template = _Stage().snapshot()
    allowed_stages = {
        "planner",
        "retrieval",
        "subanswer",
        "coverage",
        "followup_retrieval",
        "followup_subanswer",
        "composer",
        "judge",
    }

    def clean(item: Any, expected: Any, path: tuple[str, ...] = ()) -> Any:
        if isinstance(expected, dict):
            if not isinstance(item, Mapping):
                raise ValueError
            if path == ("stages",):
                if not set(item) <= allowed_stages:
                    raise ValueError
                return {
                    k: clean(v, stage_template, (*path, k)) for k, v in item.items()
                }
            if set(item) != set(expected):
                raise ValueError
            return {k: clean(item[k], v, (*path, k)) for k, v in expected.items()}
        key = path[-1]
        parent = path[-2] if len(path) > 1 else ""
        nullable = (
            (parent == "tokens" and key in {"input", "output", "total"})
            or parent == "pricing"
            or key == "estimated_usd"
        )
        if item is None and nullable:
            return None
        integer = parent not in {"latency_ms", "pricing"} and key not in {
            "latency_ms",
            "estimated_usd",
            "known_usd",
        }
        if _number(item, integer=integer) is None or (key == "version" and item != 1):
            raise ValueError
        return item

    try:
        return clean(value, template)
    except (ValueError, TypeError):
        return None


def aggregate_metrics(results: Sequence[Mapping[str, Any]]) -> dict:
    """Aggregate final snapshots only, including failed cases and partial coverage."""
    metrics = [
        m for r in results if (m := sanitize_metrics(r.get("metrics"))) is not None
    ]

    def summed(section: str) -> dict:
        if not metrics:
            return {}
        return {
            key: (
                None
                if any(m[section][key] is None for m in metrics)
                else sum(m[section][key] for m in metrics)
            )
            for key in metrics[0][section]
        }

    times = sorted(m["latency_ms"]["total"] for m in metrics)

    def percentile(fraction: float) -> float | None:
        if not times:
            return None
        position = (len(times) - 1) * fraction
        low, high = math.floor(position), math.ceil(position)
        return times[low] + (times[high] - times[low]) * (position - low)

    return {
        "cases_measured": len(metrics),
        "cases_total": len(results),
        "latency_ms": {
            **summed("latency_ms"),
            "mean": sum(times) / len(times) if times else None,
            "p50": percentile(0.5),
            "p95": percentile(0.95),
        },
        **{
            name: summed(name) for name in ("model_calls", "tokens", "cost", "workflow")
        },
        "context": {
            key: (
                max(m["context"][key] for m in metrics)
                if key.startswith("max_")
                else value
            )
            for key, value in summed("context").items()
        },
        "performance": _aggregate_performance(results),
    }


def _aggregate_performance(results: Sequence[Mapping[str, Any]]) -> dict:
    checked = [
        p
        for r in results
        if (p := sanitize_performance(r.get("performance"))) is not None
    ]
    failed_checks: dict[str, int] = {}
    for performance in checked:
        for name, check in performance["checks"].items():
            if check["status"] == "fail":
                failed_checks[name] = failed_checks.get(name, 0) + 1
    return {
        "cases_checked": len(checked),
        **{
            status: sum(p["status"] == status for p in checked)
            for status in ("pass", "fail", "incomplete")
        },
        "failed_checks": failed_checks,
    }
