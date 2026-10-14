"""Stateless, injectable runner for retrieval-augmented generation evaluations."""

from __future__ import annotations

import json
import math
import re
from collections.abc import Callable, Mapping, Sequence
from concurrent.futures import CancelledError
from copy import deepcopy
from dataclasses import dataclass, field as dataclass_field
from numbers import Real
from pathlib import Path
from time import perf_counter
from typing import Any
from urllib.parse import urlsplit

from ..chat.agent_loop import (
    DEFAULT_SYSTEM_PROMPT,
    MAX_AGGREGATE_SOURCE_COUNT,
    ChatModel,
    ChatOpenAIConfig,
    PaperChunkRetriever,
    _create_chat_openai,
    _assert_answer_chunks,
    _load_message_types,
    _raise_if_output_limited,
    _response_text,
)
from .stages import EvaluationStages, EvaluationStopped
from .metrics import (
    EvaluationMetrics,
    EvaluationPricing,
    MeasuredModel,
    MeasuredRetriever,
    PerformanceBudget,
    check_performance,
)
from .diagnostics import (
    EvaluationRefusalError,
    classify_failure,
    failure_record,
)
from ..retrieval.paper_search import (
    EVALUATION_RETRIEVAL_STRATEGIES,
)
from ..chat.planner_retry import PlannerRetryPolicy, planner_refused

DEFAULT_JUDGE_PROMPT_PATH = Path(__file__).resolve().parents[2] / "tests" / "judge.md"
JUDGE_CRITERIA = (
    "context_precision",
    "context_recall",
    "relevant_chunk_ranking",
    "context_utilization",
    "semantic_similarity",
    "completeness",
    "hallucination_rate",
)
EvaluationRetriever = PaperChunkRetriever
_BASE_JUDGE_FIELDS = {"score", "confidence", "reasoning", "flags"}
_EXTRA_JUDGE_FIELDS = {
    "relevant_chunk_ranking": "relevant_chunk_ranks",
    "context_utilization": "chunks_used",
    "semantic_similarity": "contradicting_claims",
    "completeness": "missing_elements",
    "hallucination_rate": "unsupported_claims",
}
_SYSTEM_HEADER = "SYSTEM PROMPT:\n"
_USER_HEADER = "\nUSER PROMPT (fill per example):\n"
_CHUNK_TEMPLATE = "1. {{chunk_1}}\n2. {{chunk_2}}\n..."
_FOLLOWUPS_DECIDED = {
    "followup_retrieval",
    "followup_subanswer",
    "composer",
    "judge",
    "done",
}
_PLACEHOLDERS = re.compile(
    r"\{\{query\}\}|\{\{ground_truth_or_\"NOT PROVIDED\"\}\}"
    r"|\{\{generated_answer\}\}|\{\{reference_answer\}\}"
)
_JUDGE_PROMPT_CACHE_KEY = "rag-eval-judge"


class EvaluationError(RuntimeError):
    """Base error for evaluation configuration and validation failures."""


class EvaluationValidationError(EvaluationError, ValueError):
    """Raised before evaluation when the complete input array is invalid."""


class JudgeOutputError(EvaluationError, ValueError):
    """Raised when a judge response is not the required strict JSON object."""


@dataclass(frozen=True)
class EvaluationCase:
    """One fully validated test case."""

    question: str
    grading_notes: str
    sources: tuple[str, ...]
    performance_budget: tuple[tuple[str, int | float], ...] = ()

    def as_dict(self) -> dict[str, Any]:
        value: dict[str, Any] = {
            "question": self.question,
            "grading_notes": self.grading_notes,
            "sources": list(self.sources),
        }
        if self.performance_budget:
            value["performance_budget"] = dict(self.performance_budget)
        return value


@dataclass(frozen=True)
class _JudgeTemplate:
    system_prompt: str
    user_prompt: str


@dataclass
class EvaluationCaseState:
    """Run-local checkpoint and safe failure history, never a durable chat store."""

    index: int
    case: EvaluationCase
    strategy: str
    stages: EvaluationStages
    judge_template: _JudgeTemplate
    result: dict[str, Any] | None = None
    retry_round: int = -1
    attempts: dict[tuple[str, int | None], int] = dataclass_field(default_factory=dict)
    failure_history: list[dict[str, Any]] = dataclass_field(default_factory=list)
    metrics: EvaluationMetrics = dataclass_field(default_factory=EvaluationMetrics)


def validate_test_cases(value: Any) -> tuple[EvaluationCase, ...]:
    """Validate the entire parsed JSON value before returning any test cases."""

    if not isinstance(value, list) or not value:
        raise EvaluationValidationError("test data must be a non-empty JSON array")
    validated: list[EvaluationCase] = []
    for index, raw_case in enumerate(value, start=1):
        if not isinstance(raw_case, Mapping):
            raise EvaluationValidationError(f"case {index} must be a JSON object")
        question = _required_text(raw_case, "question", index)
        grading_notes = _required_text(raw_case, "grading_notes", index)
        sources = raw_case.get("sources")
        if not isinstance(sources, list):
            raise EvaluationValidationError(
                f"case {index} sources must be a JSON array"
            )
        for source_index, source in enumerate(sources, start=1):
            if not isinstance(source, str) or not source.strip():
                raise EvaluationValidationError(
                    f"case {index} source {source_index} must be a non-empty string"
                )
        budget: tuple[tuple[str, int | float], ...] = ()
        if "performance_budget" in raw_case:
            try:
                budget = PerformanceBudget.overrides(raw_case["performance_budget"])
            except (TypeError, ValueError):
                raise EvaluationValidationError(
                    f"case {index} performance_budget is invalid"
                ) from None
        validated.append(
            EvaluationCase(question, grading_notes, tuple(sources), budget)
        )
    return tuple(validated)


def parse_test_cases(payload: Any) -> tuple[EvaluationCase, ...]:
    """Parse JSON text (or validate an already parsed value) as evaluation cases."""

    value = payload
    if isinstance(payload, (str, bytes, bytearray)):
        try:
            value = json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise EvaluationValidationError(
                f"test data is not valid JSON: {exc}"
            ) from exc
    return validate_test_cases(value)


def load_test_cases(path: str | Path) -> tuple[EvaluationCase, ...]:
    """Read and validate a UTF-8 JSON test file."""

    try:
        payload = Path(path).read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise EvaluationValidationError(f"unable to read test data: {exc}") from exc
    return parse_test_cases(payload)


def parse_judge_output(text: str) -> dict[str, dict[str, Any]]:
    """Strictly parse and validate all seven judge criteria and their fields."""

    if not isinstance(text, str) or not text.strip():
        raise JudgeOutputError("judge output must be a non-empty JSON object")
    try:
        value = json.loads(
            text,
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except (json.JSONDecodeError, ValueError) as exc:
        raise JudgeOutputError(f"judge output is not strict JSON: {exc}") from exc
    if not isinstance(value, dict) or set(value) != set(JUDGE_CRITERIA):
        raise JudgeOutputError("judge output must contain exactly all seven criteria")
    for criterion in JUDGE_CRITERIA:
        _validate_criterion(criterion, value[criterion])
    return value


class EvaluationRunner:
    """Run validated cases sequentially without reading or writing chat history."""

    def __init__(
        self,
        retriever: EvaluationRetriever | Any,
        model: ChatModel | Any | None = None,
        *,
        config: ChatOpenAIConfig | None = None,
        judge_prompt_path: str | Path = DEFAULT_JUDGE_PROMPT_PATH,
        system_prompt: str = DEFAULT_SYSTEM_PROMPT,
        planner_retry_policy: PlannerRetryPolicy | None = None,
        pricing: EvaluationPricing | None = None,
        clock: Callable[[], float] = perf_counter,
        performance_budget: PerformanceBudget | None = None,
    ) -> None:
        if not callable(getattr(retriever, "retrieve_chunks", None)):
            raise TypeError("retriever must provide retrieve_chunks")
        if model is not None and not callable(getattr(model, "invoke", None)):
            raise TypeError("model must provide invoke(messages)")
        if not isinstance(system_prompt, str) or not system_prompt.strip():
            raise TypeError("system_prompt must be a non-empty string")
        self._retriever = retriever
        self._model = model
        self._model_was_injected = model is not None
        self._judge_model = None
        self._config = config or ChatOpenAIConfig()
        self._judge_prompt_path = Path(judge_prompt_path)
        self._system_prompt = system_prompt
        self._planner_retry_policy = planner_retry_policy or PlannerRetryPolicy()
        self._pricing = pricing or EvaluationPricing()
        self._clock = clock
        self._performance_budget = performance_budget or PerformanceBudget()

    def run(self, payload: Any, strategy: str) -> list[dict[str, Any]]:
        """Finish the initial pass, then resume failures in two ordered rounds."""

        cases = parse_test_cases(payload)
        _validate_strategy(strategy)
        judge_template = _load_judge_template(self._judge_prompt_path)
        states = [
            self.start_case(index, case, strategy, judge_template=judge_template)
            for index, case in enumerate(cases, start=1)
        ]
        for retry_round in range(3):
            pending = [state for state in states if state.stages.stage != "done"]
            if not pending:
                break
            for state in pending:
                self.resume_case(state, retry_round)
        return [deepcopy(state.result) for state in states]

    def run_file(self, path: str | Path, strategy: str) -> list[dict[str, Any]]:
        """Run all cases from one UTF-8 JSON file."""

        cases = load_test_cases(path)
        return self.run([case.as_dict() for case in cases], strategy)

    def start_case(
        self,
        index: int,
        case: EvaluationCase,
        strategy: str,
        *,
        judge_template: _JudgeTemplate | None = None,
    ) -> EvaluationCaseState:
        """Allocate a case without executing any provider or retrieval work."""
        _validate_strategy(strategy)
        state = EvaluationCaseState(
            index,
            case,
            strategy,
            EvaluationStages(case.question),
            judge_template
            if judge_template is not None
            else _load_judge_template(self._judge_prompt_path),
            metrics=EvaluationMetrics(self._pricing, self._clock),
        )
        return state

    def resume_case(
        self, state: EvaluationCaseState, retry_round: int
    ) -> dict[str, Any]:
        """Advance from the exact cursor until completion or one stage failure."""
        if state.stages.stage == "done":
            if state.result is None:
                raise ValueError("evaluation case has no completed result")
            return deepcopy(state.result)
        if (
            isinstance(retry_round, bool)
            or not isinstance(retry_round, int)
            or retry_round != state.retry_round + 1
            or not 0 <= retry_round <= 2
        ):
            raise ValueError("evaluation retry rounds must advance from 0 through 2")
        state.retry_round = retry_round
        result: dict[str, Any] = {
            "case_index": state.index,
            "input": state.case.as_dict(),
            "retrieval_strategy": state.strategy,
            "retrieved_chunks": [],
            "generated_answer": None,
            "judge_scores": None,
            "error": None,
        }
        stages = state.stages
        while stages.stage != "done":
            key = (stages.stage, stages.hop)
            state.attempts[key] = state.attempts.get(key, 0) + 1
            try:
                with state.metrics.stage(stages.stage):
                    if stages.stage == "judge":
                        _assert_answer_chunks(stages.chunks)
                        result["retrieved_chunks"] = _serialize_chunks(stages.chunks)
                        result["generated_answer"] = stages.answer
                        template = state.judge_template
                        prompt = _render_judge_prompt(
                            template, state.case, stages.chunks, stages.answer
                        )
                        model = MeasuredModel(
                            self._judge_model_instance(),
                            state.metrics,
                            "judge",
                            _ranked_chunk_text(stages.chunks),
                            stages.chunks,
                        )
                        text = self._invoke(_judge_messages(template, prompt), model)
                        result["judge_scores"] = parse_judge_output(text)
                        stages.stage = "done"
                    else:
                        subanswer = stages.stage in {"subanswer", "followup_subanswer"}
                        model = self._model_instance()
                        answer_model_factory = None
                        if subanswer:
                            stage_name = stages.stage

                            def answer_model_factory(index: int) -> MeasuredModel:
                                return MeasuredModel(
                                    self._model_instance(),
                                    state.metrics,
                                    stage_name,
                                    stages.contexts[index],
                                    stages.hop_chunks[index],
                                )
                        else:
                            model = MeasuredModel(model, state.metrics, stages.stage)
                        stages.advance(
                            model,
                            MeasuredRetriever(
                                self._retriever, state.metrics, stages.stage
                            ),
                            self._system_prompt,
                            self._planner_retry_policy,
                            answer_model_factory,
                        )
            except EvaluationStopped as exc:
                raise exc.error_type() from None
            except Exception as exc:
                _raise_if_terminal(exc)
                error_key = (stages.stage, stages.hop)
                if error_key != key:
                    state.attempts[error_key] = (
                        state.attempts.get(error_key, 0) + state.attempts.pop(key)
                    )
                    key = error_key
                reason = (
                    ("invalid_judge_output", None)
                    if isinstance(exc, JudgeOutputError)
                    else classify_failure(exc, stages.stage)
                )
                result["error"] = failure_record(
                    stages.stage, reason, retry_round, state.attempts[key], stages.hop
                )
                state.failure_history.append(dict(result["error"]))
                break
        result["attempt_history"] = deepcopy(state.failure_history)
        result["metrics"] = state.metrics.snapshot()
        result["performance"] = check_performance(
            result["metrics"],
            self._performance_budget.merged(state.case.performance_budget),
            followup_queries=(
                len(stages.subqueries) - stages.initial_count
                if stages.stage in _FOLLOWUPS_DECIDED
                else None
            ),
            completed=stages.stage == "done",
        )
        state.result = result
        return deepcopy(result)

    def _invoke(self, messages: Sequence[Any], model: Any = None) -> str:
        response = (self._model_instance() if model is None else model).invoke(messages)
        if planner_refused(response):
            raise EvaluationRefusalError("Provider declined the request.")
        _raise_if_output_limited(response)
        return _response_text(response)

    def _model_instance(self) -> ChatModel | Any:
        if self._model is None:
            self._model = _create_chat_openai(self._config, max_retries=0)
        return self._model

    def _judge_model_instance(self) -> ChatModel | Any:
        model = self._model_instance()
        if self._model_was_injected or not _is_direct_openai_endpoint(
            self._config.base_url
        ):
            return model
        if self._judge_model is None:
            self._judge_model = model.bind(prompt_cache_key=_JUDGE_PROMPT_CACHE_KEY)
        return self._judge_model


def _is_direct_openai_endpoint(base_url: str) -> bool:
    try:
        parsed = urlsplit(base_url)
        return (
            parsed.scheme == "https"
            and parsed.hostname == "api.openai.com"
            and parsed.port is None
            and parsed.username is None
            and parsed.password is None
            and parsed.path.rstrip("/") == "/v1"
            and not parsed.query
            and not parsed.fragment
        )
    except ValueError:
        return False


def _required_text(case: Mapping[str, Any], name: str, index: int) -> str:
    value = case.get(name)
    if not isinstance(value, str) or not value.strip():
        raise EvaluationValidationError(
            f"case {index} {name} must be a non-empty string"
        )
    return value


def _validate_strategy(strategy: str) -> None:
    if strategy not in EVALUATION_RETRIEVAL_STRATEGIES:
        allowed = ", ".join(EVALUATION_RETRIEVAL_STRATEGIES)
        raise EvaluationValidationError(f"strategy must be one of: {allowed}")


def _load_judge_template(path: Path) -> _JudgeTemplate:
    try:
        contents = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise EvaluationError(f"unable to read judge prompt: {exc}") from exc
    if not contents.startswith(_SYSTEM_HEADER) or _USER_HEADER not in contents:
        raise EvaluationError(
            "judge prompt is missing its system or user prompt section"
        )
    system_section, user_section = contents.split(_USER_HEADER, maxsplit=1)
    system_prompt = system_section.removeprefix(_SYSTEM_HEADER).strip("\n")
    user_prompt = user_section.strip("\n")
    required = {
        "{{query}}",
        '{{ground_truth_or_"NOT PROVIDED"}}',
        "{{generated_answer}}",
        "{{reference_answer}}",
    }
    if _CHUNK_TEMPLATE not in user_prompt or any(
        placeholder not in user_prompt for placeholder in required
    ):
        raise EvaluationError("judge prompt is missing required runtime placeholders")
    return _JudgeTemplate(system_prompt, user_prompt)


def _serialize_chunks(chunks: Sequence[Any]) -> list[dict[str, Any]]:
    if not 1 <= len(chunks) <= MAX_AGGREGATE_SOURCE_COUNT:
        raise ValueError(
            "multihop retrieval must return between 1 and "
            f"{MAX_AGGREGATE_SOURCE_COUNT} aggregate chunks"
        )
    serialized: list[dict[str, Any]] = []
    for position, chunk in enumerate(chunks, start=1):
        record = {
            "rank": _chunk_value(chunk, "rank"),
            "text": _chunk_value(chunk, "text"),
            "score": _chunk_value(chunk, "score"),
            "paper_id": _chunk_value(chunk, "paper_id"),
            "chunk_index": _chunk_value(chunk, "chunk_index"),
            "filename": _chunk_value(chunk, "filename"),
            "title": _chunk_value(chunk, "title"),
            "page": _chunk_value(chunk, "page"),
        }
        _validate_chunk(record, position)
        serialized.append(record)
    return serialized


def _chunk_value(chunk: Any, name: str) -> Any:
    return chunk.get(name) if isinstance(chunk, Mapping) else getattr(chunk, name, None)


def _validate_chunk(chunk: Mapping[str, Any], position: int) -> None:
    if chunk["rank"] != position:
        raise ValueError("retrieved chunk ranks must be consecutive from 1")
    if not isinstance(chunk["text"], str) or not chunk["text"].strip():
        raise ValueError("retrieved chunk text must be non-empty")
    score = chunk["score"]
    if (
        isinstance(score, bool)
        or not isinstance(score, Real)
        or not math.isfinite(score)
    ):
        raise ValueError("retrieved chunk score must be finite")
    for field in ("paper_id", "filename", "title"):
        if not isinstance(chunk[field], str) or not chunk[field].strip():
            raise ValueError(f"retrieved chunk {field} must be non-empty")
    for field, minimum in (("chunk_index", 0), ("page", 1)):
        value = chunk[field]
        if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
            raise ValueError(f"retrieved chunk {field} is invalid")


def _ranked_chunk_text(chunks: Sequence[Any]) -> str:
    return "\n".join(
        f"{position}. {_chunk_value(chunk, 'text')}"
        for position, chunk in enumerate(chunks, start=1)
    )


def _judge_messages(template: _JudgeTemplate, user_prompt: str) -> list[Any]:
    human_message, _, system_message, _ = _load_message_types()
    return [
        system_message(content=template.system_prompt),
        human_message(content=user_prompt),
    ]


def _render_judge_prompt(
    template: _JudgeTemplate,
    case: EvaluationCase,
    chunks: Sequence[Any],
    answer: str,
) -> str:
    ground_truth = (
        json.dumps(list(case.sources), ensure_ascii=False)
        if case.sources
        else "NOT PROVIDED"
    )
    values = {
        "{{query}}": case.question,
        '{{ground_truth_or_"NOT PROVIDED"}}': ground_truth,
        "{{generated_answer}}": answer,
        "{{reference_answer}}": case.grading_notes,
    }
    prompt = template.user_prompt.replace(_CHUNK_TEMPLATE, _ranked_chunk_text(chunks))
    return _PLACEHOLDERS.sub(lambda match: values[match.group(0)], prompt)


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key: {key}")
        value[key] = item
    return value


def _reject_json_constant(value: str) -> Any:
    raise ValueError(f"invalid JSON constant: {value}")


def _validate_criterion(name: str, value: Any) -> None:
    if not isinstance(value, dict):
        raise JudgeOutputError(f"{name} must be an object")
    fields = set(_BASE_JUDGE_FIELDS)
    extra = _EXTRA_JUDGE_FIELDS.get(name)
    if extra is not None:
        fields.add(extra)
    if set(value) != fields:
        raise JudgeOutputError(f"{name} has missing or unexpected fields")
    score = value["score"]
    if isinstance(score, bool) or not isinstance(score, int) or not 0 <= score <= 5:
        raise JudgeOutputError(f"{name}.score must be an integer from 0 to 5")
    if value["confidence"] not in {"high", "medium", "low"}:
        raise JudgeOutputError(f"{name}.confidence is invalid")
    if not isinstance(value["reasoning"], str) or not value["reasoning"].strip():
        raise JudgeOutputError(f"{name}.reasoning must be a non-empty string")
    _validate_string_list(name, "flags", value["flags"])
    if extra == "relevant_chunk_ranks":
        ranks = value[extra]
        if not isinstance(ranks, list) or any(
            isinstance(rank, bool)
            or not isinstance(rank, int)
            or not 1 <= rank <= MAX_AGGREGATE_SOURCE_COUNT
            for rank in ranks
        ):
            raise JudgeOutputError(f"{name}.{extra} must contain chunk ranks")
    elif extra == "chunks_used":
        used = value[extra]
        if not isinstance(used, list) or any(
            not _valid_chunk_reference(item) for item in used
        ):
            raise JudgeOutputError(f"{name}.{extra} must contain chunk references")
    elif extra is not None:
        _validate_string_list(name, extra, value[extra])


def _validate_string_list(name: str, field: str, value: Any) -> None:
    if not isinstance(value, list) or any(
        not isinstance(item, str) or not item.strip() for item in value
    ):
        raise JudgeOutputError(f"{name}.{field} must be a list of non-empty strings")


def _valid_chunk_reference(value: Any) -> bool:
    if isinstance(value, int) and not isinstance(value, bool):
        return 1 <= value <= MAX_AGGREGATE_SOURCE_COUNT
    return isinstance(value, str) and bool(value.strip())


def _raise_if_terminal(exc: Exception) -> None:
    """Do not turn transport shutdown or wrapped cancellation into case retries."""
    seen: set[int] = set()
    current: BaseException | None = exc
    while current is not None and id(current) not in seen:
        seen.add(id(current))
        if isinstance(current, (CancelledError, BrokenPipeError)):
            raise current
        current = current.__cause__


__all__ = [
    "DEFAULT_JUDGE_PROMPT_PATH",
    "EvaluationCase",
    "EvaluationError",
    "EvaluationRetriever",
    "EvaluationRunner",
    "EvaluationValidationError",
    "JUDGE_CRITERIA",
    "JudgeOutputError",
    "load_test_cases",
    "parse_judge_output",
    "parse_test_cases",
    "validate_test_cases",
]
