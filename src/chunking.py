"""Utilities for splitting documents into paragraph-sized chunks."""

from __future__ import annotations

import re

__all__ = ["split_into_paragraphs", "chunk_text", "split_paragraphs"]


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
    sections = re.split(
        rf"{newline}[^\S\r\n]*(?:{newline}[^\S\r\n]*)+", text
    )
    return [section.strip() for section in sections if section.strip()]


def chunk_text(text: str) -> list[str]:
    """Alias for :func:`split_into_paragraphs`."""
    return split_into_paragraphs(text)


def split_paragraphs(text: str) -> list[str]:
    """Alias for :func:`split_into_paragraphs` for paragraph-oriented callers."""
    return split_into_paragraphs(text)