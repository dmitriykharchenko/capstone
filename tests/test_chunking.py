import pytest

from src.chunking import split_into_paragraphs


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