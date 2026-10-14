"""Deterministic, atomic persistence for completed RAG evaluation runs."""

from __future__ import annotations

import json
import os
import tempfile
from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from uuid import uuid4

from .diagnostics import sanitize_result_diagnostics
from .metrics import aggregate_metrics
from ..cli.config import default_evaluation_results_path

EVALUATION_RESULTS_SCHEMA = "rag-evaluation-results"
EVALUATION_RESULTS_SCHEMA_VERSION = 3
CANONICAL_RETRIEVAL_IDENTIFIER = "multihop-hybrid"
DEFAULT_EVALUATION_RESULTS_PATH = default_evaluation_results_path()


def evaluation_results_artifact(
    *,
    input_cases: Sequence[Mapping[str, Any]],
    retrieval_strategy: str,
    results: Sequence[Mapping[str, Any]],
    aggregate_scores: Mapping[str, float | None],
    succeeded: int,
    failed: int,
) -> dict[str, Any]:
    """Build the versioned artifact for one terminal evaluation run."""

    return {
        "schema": EVALUATION_RESULTS_SCHEMA,
        "schema_version": EVALUATION_RESULTS_SCHEMA_VERSION,
        "metadata": {
            "retrieval_identifier": CANONICAL_RETRIEVAL_IDENTIFIER,
            "retrieval_strategy": retrieval_strategy,
        },
        "input_cases": [dict(case) for case in input_cases],
        "results": [sanitize_result_diagnostics(dict(result)) for result in results],
        "aggregate_scores": dict(aggregate_scores),
        "aggregate_metrics": aggregate_metrics(results),
        "summary": {
            "total": len(input_cases),
            "succeeded": succeeded,
            "failed": failed,
        },
    }


def serialize_evaluation_results(artifact: Mapping[str, Any]) -> str:
    """Serialize an artifact as stable, strict UTF-8 JSON text."""

    return (
        json.dumps(
            artifact,
            allow_nan=False,
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )


def persist_evaluation_results(
    path: str | Path,
    *,
    input_cases: Sequence[Mapping[str, Any]],
    retrieval_strategy: str,
    results: Sequence[Mapping[str, Any]],
    aggregate_scores: Mapping[str, float | None],
    succeeded: int,
    failed: int,
) -> Path:
    """Publish a new dated sibling of the filename template; never replace a file."""

    template = Path(path)
    now = datetime.now(timezone.utc)
    timestamp = f"{now.year:04d}" + now.strftime("-%m-%dT%H-%M-%S.%fZ")
    target = template.with_name(
        f"{template.stem}_{timestamp}_{uuid4()}{template.suffix}"
    )
    target.parent.mkdir(parents=True, exist_ok=True)
    artifact = evaluation_results_artifact(
        input_cases=input_cases,
        retrieval_strategy=retrieval_strategy,
        results=results,
        aggregate_scores=aggregate_scores,
        succeeded=succeeded,
        failed=failed,
    )
    contents = serialize_evaluation_results(artifact)
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            "w",
            encoding="utf-8",
            newline="\n",
            dir=target.parent,
            prefix=".eval_results.",
            suffix=".tmp",
            delete=False,
        ) as handle:
            temporary_path = Path(handle.name)
            handle.write(contents)
            handle.flush()
            os.fsync(handle.fileno())
        # Linking the complete, synced inode is atomic and fails for *any* existing
        # destination entry. Unlike replace/rename, a collision cannot erase history.
        os.link(temporary_path, target, follow_symlinks=False)
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
    return target


__all__ = [
    "CANONICAL_RETRIEVAL_IDENTIFIER",
    "DEFAULT_EVALUATION_RESULTS_PATH",
    "EVALUATION_RESULTS_SCHEMA",
    "EVALUATION_RESULTS_SCHEMA_VERSION",
    "evaluation_results_artifact",
    "persist_evaluation_results",
    "serialize_evaluation_results",
]
