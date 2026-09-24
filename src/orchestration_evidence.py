"""Deterministic evidence packing with explicit question coverage and byte caps."""

from dataclasses import asdict, dataclass, replace

from .orchestration_state import EvidenceState, integer
from .orchestration_tools import ToolError, bounded_json, evidence_key, normalized

COMPRESSION_PROMPT_RESERVE = 1024


@dataclass(frozen=True)
class EvidenceBudget:
    final_bytes: int = 12000
    compression_input_bytes: int = 16000
    summary_bytes: int = 4096
    max_records: int = 16

    def __post_init__(self):
        integer(self.final_bytes, 8192, 24000)
        integer(self.compression_input_bytes, 8192, 24000)
        integer(self.summary_bytes, 256, 4096)
        integer(self.max_records, 8, 32)


@dataclass(frozen=True)
class RankedEvidence:
    evidence: EvidenceState
    question_ids: tuple[str, ...]


@dataclass(frozen=True)
class EvidencePacket:
    records: tuple[RankedEvidence, ...]
    compression_required: bool


def evidence_payload(
    records: tuple[RankedEvidence, ...], *, snippets=True
) -> list[dict]:
    result = []
    for rank, record in enumerate(records, 1):
        value = asdict(record.evidence)
        value.pop("question_id")
        if not snippets:
            value.pop("snippet")
        result.append({**value, "rank": rank, "question_ids": record.question_ids})
    return result


def _size(records):
    return len(
        bounded_json({"evidence": evidence_payload(records)}, 1_048_576).encode()
    )


def prepare_evidence(
    values: tuple[EvidenceState, ...], budget: EvidenceBudget = EvidenceBudget()
) -> EvidencePacket:
    if (
        type(values) is not tuple
        or len(values) > 128
        or type(budget) is not EvidenceBudget
    ):
        raise ToolError("Invalid final evidence")
    # Merge connected duplicate groups, retaining the best-ranked representative
    # and the union of question coverage, even for a duplicate bridging groups.
    groups = []
    for item in values:
        if type(item) is not EvidenceState or item.question_id is None:
            raise ToolError("Final evidence must belong to an answered question")
        item.__post_init__()
    for item in sorted(values, key=evidence_key):
        keys = {(item.paper_id, item.chunk_index), normalized(item.snippet)}
        questions = {item.question_id}
        representatives = [item]
        remaining = []
        for representative, existing_keys, covered in groups:
            if keys & existing_keys:
                keys |= existing_keys
                questions |= covered
                representatives.append(representative)
            else:
                remaining.append((representative, existing_keys, covered))
        groups = remaining + [(min(representatives, key=evidence_key), keys, questions)]
    ranked = tuple(
        RankedEvidence(e, tuple(sorted(q)))
        for e, _, q in sorted(groups, key=lambda g: evidence_key(g[0]))
    )
    if _size(ranked) + 32 <= budget.final_bytes and len(ranked) <= budget.max_records:
        return EvidencePacket(ranked, False)
    # First take the highest-ranked record for every covered question, then fill
    # spare space in rank order. Never drop a question merely due to a low score.
    required = set()
    for question in sorted({q for item in ranked for q in item.question_ids}):
        required.add(
            next(i for i, item in enumerate(ranked) if question in item.question_ids)
        )
    selected = [ranked[i] for i in sorted(required)]
    if len(selected) > budget.max_records:
        raise ToolError("Evidence coverage exceeds budget")

    # Uniformly bounded snippets reserve room for coverage and source metadata.
    def compact(item):
        snippet = item.evidence.snippet.encode()[:256].decode("utf-8", errors="ignore")
        return replace(item, evidence=replace(item.evidence, snippet=snippet))

    selected = [compact(item) for item in selected]
    if (
        _size(tuple(selected)) + COMPRESSION_PROMPT_RESERVE
        > budget.compression_input_bytes
    ):
        raise ToolError("Evidence coverage exceeds budget")
    for index, item in enumerate(ranked):
        if index in required or len(selected) >= budget.max_records:
            continue
        candidate = sorted(
            [*selected, compact(item)], key=lambda r: evidence_key(r.evidence)
        )
        if (
            _size(tuple(candidate)) + COMPRESSION_PROMPT_RESERVE
            <= budget.compression_input_bytes
        ):
            selected = candidate
    return EvidencePacket(
        tuple(sorted(selected, key=lambda r: evidence_key(r.evidence))), True
    )
