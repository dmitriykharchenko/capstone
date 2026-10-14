"""Evaluation-only, in-memory checkpoints; chat execution is deliberately unchanged."""

from __future__ import annotations

import asyncio
from concurrent.futures import CancelledError
from dataclasses import dataclass
from collections.abc import Callable
from typing import Any, Literal

from ..chat.agent_loop import (
    MAX_AGGREGATE_SOURCE_COUNT,
    MAX_TOTAL_HOP_COUNT,
    AgentCoverageError,
    _aggregate_chunks,
    _answer_multihop_round,
    _assert_answer_chunks,
    _compose_multihop_messages,
    _invoke_multihop,
    _load_message_types,
    _plan_multihop,
    _response_text,
    _retrieve_multihop,
    _review_multihop_coverage,
    _RoundAnswerFailure,
)
from ..retrieval.paper_search import PaperChunkMatch
from ..chat.planner_retry import PlannerRetryPolicy, planner_refused
from .diagnostics import (
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
    subanswers: tuple[str | None, ...] = ()
    contexts: tuple[str, ...] = ()
    hop_chunks: tuple[tuple[PaperChunkMatch, ...], ...] = ()
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
        answer_model_factory: Callable[[int], Any] | None = None,
    ) -> None:
        """Execute exactly one stage. The runner owns the final judge stage."""
        wrapped = _StageModel(model, planner=self.stage == "planner")
        try:
            self._advance(
                wrapped,
                retriever,
                system_prompt,
                planner_retry_policy,
                answer_model_factory,
            )
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
        answer_model_factory: Callable[[int], Any] | None,
    ) -> None:
        types = _load_message_types()
        if self.stage == "planner":
            queries = _plan_multihop(
                model,
                types,
                (),
                self.question,
                planner_retry_policy,
            )
            self.subqueries = queries
            self.initial_count = len(queries)
            self.subanswers = (None,) * len(queries)
            self.stage = "retrieval"
        elif self.stage in {"retrieval", "followup_retrieval"}:
            context, _, chunks = _retrieve_multihop(
                retriever,
                self.subqueries[self.hop_index],
            )
            self.contexts = (*self.contexts, context)
            self.hop_chunks = (*self.hop_chunks, chunks)
            self.hop_index += 1
            if self.hop_index == len(self.subqueries):
                self.stage = (
                    "followup_subanswer"
                    if self.stage == "followup_retrieval"
                    else "subanswer"
                )
        elif self.stage in {"subanswer", "followup_subanswer"}:
            indices = tuple(
                index
                for index, answer in enumerate(self.subanswers)
                if answer is None
            )
            answer_models = (
                [model] * len(indices)
                if answer_model_factory is None
                else [_StageModel(answer_model_factory(index)) for index in indices]
            )
            for index in indices:
                _assert_answer_chunks(self.hop_chunks[index])
            try:
                answers = _answer_multihop_round(
                    answer_models,
                    types,
                    system_prompt,
                    self.question,
                    [self.subqueries[index] for index in indices],
                    [self.contexts[index] for index in indices],
                    answer_indices=indices,
                )
            except _RoundAnswerFailure as failure:
                subanswers = list(self.subanswers)
                for index, answer in failure.answers.items():
                    subanswers[index] = answer
                self.subanswers = tuple(subanswers)
                self.hop_index = failure.index
                if isinstance(failure.error, EvaluationStopped):
                    raise failure.error
                if isinstance(
                    failure.error,
                    (KeyboardInterrupt, asyncio.CancelledError, CancelledError, BrokenPipeError),
                ):
                    raise EvaluationStopped(type(failure.error)) from None
                raise SafeStageFailure(
                    classify_failure(failure.error, self.stage)
                ) from None
            subanswers = list(self.subanswers)
            for index, answer in zip(indices, answers, strict=True):
                subanswers[index] = answer
            self.subanswers = tuple(subanswers)
            self.hop_index = len(self.subqueries)
            followup = self.stage == "followup_subanswer"
            self.stage = "composer" if followup else "coverage"
        elif self.stage == "coverage":
            if any(answer is None for answer in self.subanswers):
                raise AgentCoverageError("subanswer round was incomplete")
            followups = _review_multihop_coverage(
                model,
                types,
                self.question,
                self.subqueries,
                self.subanswers,
            )
            self.subqueries = (*self.subqueries, *followups)
            self.subanswers = (*self.subanswers, *((None,) * len(followups)))
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
