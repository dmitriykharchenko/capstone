import pytest

from src.chunking import split_into_paragraphs, split_oversized_chunk


def test_splits_on_single_and_multiple_blank_lines() -> None:
    text = "First paragraph.\n\nSecond paragraph.\n \n\nThird paragraph."

    assert split_into_paragraphs(text) == [
        "First paragraph.",
        "Second paragraph.",
        "Third paragraph.",
    ]


def test_trims_surrounding_whitespace_without_changing_content() -> None:
    text = "  First line\ncontinued.  \n\n\tSecond paragraph\n  with spacing. \t"

    assert split_into_paragraphs(text) == [
        "First line\ncontinued.",
        "Second paragraph\n  with spacing.",
    ]


def test_empty_and_whitespace_only_input_produce_no_chunks() -> None:
    assert split_into_paragraphs("") == []
    assert split_into_paragraphs(" \t\n\n  ") == []


def test_preserves_paragraph_order() -> None:
    assert split_into_paragraphs("third\n\nfirst\n\nsecond") == [
        "third",
        "first",
        "second",
    ]


def test_accepts_common_newline_styles() -> None:
    assert split_into_paragraphs("one\r\ntwo\r\n\r\nthree") == [
        "one\r\ntwo",
        "three",
    ]


def test_rejects_non_string_input() -> None:
    with pytest.raises(TypeError, match="text must be a string"):
        split_into_paragraphs(None)  # type: ignore[arg-type]


def test_oversized_chunk_splits_greedily_at_sentence_boundaries() -> None:
    text = "Alpha one. Beta two. Gamma three."

    chunks = split_oversized_chunk(text, max_bytes=22)

    assert chunks == ["Alpha one. Beta two.", "Gamma three."]
    assert all(len(chunk.encode("utf-8")) <= 22 for chunk in chunks)


def test_sentence_longer_than_budget_uses_deterministic_hard_split() -> None:
    text = "abcdefghij"

    assert split_oversized_chunk(text, max_bytes=4) == ["abcd", "efgh", "ij"]


def test_byte_budget_handles_unicode_and_preserves_small_chunks_exactly() -> None:
    assert split_oversized_chunk("ééé", max_bytes=4) == ["éé", "é"]
    assert split_oversized_chunk("  already small  ", max_bytes=30) == [
        "  already small  "
    ]
