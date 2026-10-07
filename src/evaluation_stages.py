"""Evaluation-only, in-memory checkpoints; chat execution is deliberately unchanged."""

from __future__ import annotations

from concurrent.futures import CancelledError
from dataclasses import dataclass
from typing import Any, Literal

from .agent_loop import (
    MAX_AGGREGATE_SOURCE_COUNT,
    MAX_TOTAL_HOP_COUNT,
    AgentCoverageError,
    _aggregate_chunks,
    _answer_multihop,
    _assert_answer_chunks,
    _compose_multihop_messages,
    _invoke_multihop,
    _load_message_types,
    _plan_multihop,
    _response_text,
    _retrieve_multihop,
    _review_multihop_coverage,
    _seen_chunks,
)
from .paper_search import PaperChunkMatch
from .planner_retry import PlannerRetryPolicy, planner_refused
from .evaluation_diagnostics import (
    EvaluationRefusalError,
    Reason,
    SafeStageFailure,
    classify_failure,
)

Stage = Literal[
    "planner",
    "retrieval",
    "subanswer",
    "coverage",
    "followup_retrieval",
    "followup_subanswer",
    "composer",
    "judge",
    "done",
]


class EvaluationStopped(BaseException):
    """Keep terminal signals outside helpers' per-stage Exception handlers."""

    def __init__(self, error_type: type[Exception]) -> None:
        self.error_type = error_type


@dataclass
class _StageModel:
    model: Any
    reason: Reason | None = None
    planner: bool = False

    def invoke(self, messages: Any) -> Any:
        self.reason = None
        try:
            response = self.model.invoke(messages)
        except (CancelledError, BrokenPipeError) as exc:
            raise EvaluationStopped(type(exc)) from None
        except Exception as exc:
            self.reason = classify_failure(exc, "model")
            raise
        if planner_refused(response):
            self.reason = ("refusal", None)
            if not self.planner:
                raise EvaluationRefusalError("Provider declined the request.")
        return response


@dataclass
class EvaluationStages:
    """Commit validated stage outputs before moving the cursor to the next stage.

    No provider objects, exceptions, messages, history stores, or graph resources
    are retained. A failed call leaves the cursor and completed outputs intact.
    """

    question: str
    stage: Stage = "planner"
    hop_index: int = 0
    initial_count: int = 0
    subqueries: tuple[str, ...] = ()
    subanswers: tuple[str, ...] = ()
    contexts: tuple[str, ...] = ()
    hop_chunks: tuple[tuple[PaperChunkMatch, ...], ...] = ()
    reused_hops: tuple[bool, ...] = ()
    chunks: tuple[PaperChunkMatch, ...] = ()
    answer: str | None = None

    @property
    def hop(self) -> int | None:
        return (
            self.hop_index + 1
            if self.stage
            in {"retrieval", "subanswer", "followup_retrieval", "followup_subanswer"}
            else None
        )

    def advance(
        self,
        model: Any,
        retriever: Any,
        system_prompt: str,
        planner_retry_policy: PlannerRetryPolicy,
    ) -> None:
        """Execute exactly one stage. The runner owns the final judge stage."""
        wrapped = _StageModel(model, planner=self.stage == "planner")
        try:
            self._advance(wrapped, retriever, system_prompt, planner_retry_policy)
            return
        except Exception as exc:
            current: BaseException | None = exc
            seen: set[int] = set()
            while current is not None and id(current) not in seen:
                seen.add(id(current))
                if isinstance(current, (CancelledError, BrokenPipeError)):
                    raise EvaluationStopped(type(current)) from None
                current = current.__cause__ or current.__context__
            reason = wrapped.reason or classify_failure(exc, self.stage)
        # Do not retain even an implicit exception context or rejected model output.
        raise SafeStageFailure(reason) from None

    def _advance(
        self,
        model: _StageModel,
        retriever: Any,
        system_prompt: str,
        planner_retry_policy: PlannerRetryPolicy,
    ) -> None:
        types = _load_message_types()
        if self.stage == "planner":
            queries = _plan_multihop(
                model, types, (), self.question, planner_retry_policy
            )
            self.subqueries = queries
            self.initial_count = len(queries)
            self.stage = "retrieval"
        elif self.stage in {"retrieval", "followup_retrieval"}:
            context, _, chunks, reused = _retrieve_multihop(
                retriever,
                self.subqueries[self.hop_index],
                _seen_chunks(self.hop_chunks),
            )
            self.contexts = (*self.contexts, context)
            self.hop_chunks = (*self.hop_chunks, chunks)
            self.reused_hops = (*self.reused_hops, reused)
            self.stage = (
                "followup_subanswer"
                if self.stage == "followup_retrieval"
                else "subanswer"
            )
        elif self.stage in {"subanswer", "followup_subanswer"}:
            _assert_answer_chunks(
                self.hop_chunks[self.hop_index],
                _seen_chunks(self.hop_chunks, before=self.hop_index),
                reused=self.reused_hops[self.hop_index],
            )
            answer = _answer_multihop(
                model,
                types,
                system_prompt,
                self.question,
                self.subqueries[self.hop_index],
                self.contexts[self.hop_index],
            )
            self.subanswers = (*self.subanswers, answer)
            self.hop_index += 1
            followup = self.stage == "followup_subanswer"
            if self.hop_index < len(self.subqueries):
                self.stage = "followup_retrieval" if followup else "retrieval"
            else:
                self.stage = "composer" if followup else "coverage"
        elif self.stage == "coverage":
            followups = _review_multihop_coverage(
                model, types, self.question, self.subqueries, self.subanswers
            )
            self.subqueries = (*self.subqueries, *followups)
            self.stage = "followup_retrieval" if followups else "composer"
        elif self.stage == "composer":
            if (
                len(self.subqueries) > MAX_TOTAL_HOP_COUNT
                or sum(map(len, self.hop_chunks)) > MAX_AGGREGATE_SOURCE_COUNT
            ):
                raise AgentCoverageError("adaptive multihop bounds were exceeded")
            chunks = _aggregate_chunks(self.hop_chunks)
            messages = _compose_multihop_messages(
                types,
                system_prompt,
                (),
                self.question,
                self.subqueries,
                self.subanswers,
            )
            answer = _response_text(_invoke_multihop(model, messages, "Composer"))
            self.chunks = chunks
            self.answer = answer
            self.stage = "judge"
        else:
            raise ValueError("stage must be executed by the evaluation runner")
