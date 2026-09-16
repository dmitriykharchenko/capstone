"""Utilities for splitting documents into paragraph-sized chunks."""

from __future__ import annotations

import re

# text-embedding-3-small accepts 8,191 tokens. UTF-8 bytes provide a
# dependency-free conservative upper bound for byte-pair tokenization.
DEFAULT_MAX_CHUNK_BYTES = 8_000
_SENTENCE_END = re.compile(r"[.!?](?:[\"'’”)\]]*)(?=\s|$)")

__all__ = [
    "DEFAULT_MAX_CHUNK_BYTES",
    "split_into_paragraphs",
    "split_oversized_chunk",
    "chunk_text",
    "split_paragraphs",
]


def split_into_paragraphs(text: str) -> list[str]:
    """Split *text* into non-empty, whitespace-trimmed paragraphs.

    Paragraphs are separated by one or more blank lines.  Newline styles are
    normalized for boundary detection, while each paragraph's internal
    content and its ordering are preserved.  An empty or whitespace-only
    document produces an empty list.

    Args:
        text: The document to split.

    Returns:
        A list of paragraph strings in their original order.

    Raises:
        TypeError: If ``text`` is not a string.
    """
    if not isinstance(text, str):
        raise TypeError("text must be a string")

    newline = r"(?:\r\n|\n|\r(?!\n))"
    sections = re.split(rf"{newline}[^\S\r\n]*(?:{newline}[^\S\r\n]*)+", text)
    return [section.strip() for section in sections if section.strip()]


def split_oversized_chunk(
    text: str, *, max_bytes: int = DEFAULT_MAX_CHUNK_BYTES
) -> list[str]:
    """Split one chunk within a UTF-8 byte budget, preferring sentence ends."""
    if not isinstance(text, str):
        raise TypeError("text must be a string")
    if isinstance(max_bytes, bool) or not isinstance(max_bytes, int):
        raise TypeError("max_bytes must be an integer")
    if max_bytes < 1:
        raise ValueError("max_bytes must be positive")
    if not text.strip():
        return []
    if len(text.encode("utf-8")) <= max_bytes:
        return [text]

    boundaries = [match.end() for match in _SENTENCE_END.finditer(text)]
    chunks: list[str] = []
    start = 0
    while start < len(text):
        while start < len(text) and text[start].isspace():
            start += 1
        if start == len(text):
            break
        hard_end = _max_fitting_end(text, start, max_bytes)
        sentence_ends = [end for end in boundaries if start < end <= hard_end]
        end = sentence_ends[-1] if sentence_ends else hard_end
        chunk = text[start:end].strip()
        if chunk:
            chunks.append(chunk)
        start = end
    return chunks


def _max_fitting_end(text: str, start: int, max_bytes: int) -> int:
    """Return the furthest character boundary fitting ``max_bytes``."""
    low, high = start + 1, len(text)
    if len(text[start:low].encode("utf-8")) > max_bytes:
        raise ValueError("max_bytes is too small for a UTF-8 character")
    while low < high:
        middle = (low + high + 1) // 2
        if len(text[start:middle].encode("utf-8")) <= max_bytes:
            low = middle
        else:
            high = middle - 1
    return low


def chunk_text(text: str) -> list[str]:
    """Alias for :func:`split_into_paragraphs`."""
    return split_into_paragraphs(text)


def split_paragraphs(text: str) -> list[str]:
    """Alias for :func:`split_into_paragraphs` for paragraph-oriented callers."""
    return split_into_paragraphs(text)
