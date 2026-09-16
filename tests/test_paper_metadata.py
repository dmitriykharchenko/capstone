import json
from types import SimpleNamespace

import pytest

from src.paper_metadata import (
    MAX_METADATA_TEXT_CHARS,
    MetadataConfigurationError,
    MetadataIncompleteError,
    MetadataModelError,
    MetadataOutputError,
    MetadataValidationError,
    PaperMetadata,
    PaperMetadataExtractor,
    parse_metadata_output,
)


def payload(**overrides):
    value = {
        "title": "  A   Useful Paper  ",
        "authors": [" Alice   Smith ", "alice smith", "Bob Jones"],
        "topics": [" Retrieval ", "RAG", "machine   learning"],
    }
    value.update(overrides)
    return json.dumps(value)


class FakeModel:
    def __init__(self, response):
        self.response = response
        self.calls = []

    def invoke(self, messages):
        self.calls.append(messages)
        return self.response


def test_valid_output_is_normalized_and_immutable():
    result = PaperMetadataExtractor(FakeModel(SimpleNamespace(content=payload()))).extract(
        "paper text"
    )

    assert result == PaperMetadata(
        "A Useful Paper", ("Alice Smith", "Bob Jones"), ("retrieval", "rag", "machine learning")
    )
    with pytest.raises((AttributeError, TypeError)):
        result.title = "changed"


def test_duplicate_values_keep_deterministic_first_seen_order():
    result = parse_metadata_output(
        payload(
            authors=["Zed", " anna  lee ", "ZED", "ANNA LEE", "Bob"],
            topics=["Topic C", "topic a", "TOPIC C", "Topic B", "topic a"],
        )
    )

    assert result.authors == ("Zed", "anna lee", "Bob")
    assert result.topics == ("topic c", "topic a", "topic b")


def test_blank_authors_and_topics_are_skipped_before_deduplication():
    result = parse_metadata_output(
        payload(
            authors=[" Alice ", "  ", "Bob"],
            topics=["One", "\n", " Two ", "Three"],
        )
    )

    assert result.authors == ("Alice", "Bob")
    assert result.topics == ("one", "two", "three")


@pytest.mark.parametrize(
    "authors,topics",
    [
        ([" ", "\n"], ["one", "two", "three"]),
        (["Alice"], [" ", "\n"]),
    ],
)
def test_all_blank_metadata_lists_still_fail_validation(authors, topics):
    with pytest.raises(MetadataOutputError):
        parse_metadata_output(payload(authors=authors, topics=topics))


def test_prompt_contains_a_bounded_deterministic_text_representation():
    model = FakeModel(SimpleNamespace(content=payload()))
    PaperMetadataExtractor(model).extract("  first   line\n\n" + "x" * 20_000)

    prompt = model.calls[0][1].content
    evidence = prompt.split("<paper_text>\n", 1)[1].split("\n</paper_text>", 1)[0]
    assert evidence.startswith("first line\n")
    assert len(evidence) == MAX_METADATA_TEXT_CHARS
    assert "x" * 20_001 not in prompt


@pytest.mark.parametrize(
    "text",
    [
        "",
        "   \n\n  ",
        "{\"title\": \"x\", \"authors\": [], \"topics\": [\"a\", \"b\", \"c\"]}",
        payload(topics=["one", "two"]),
        payload(topics=[str(index) for index in range(11)]),
    ],
)
def test_empty_or_out_of_range_values_fail_explicitly(text):
    with pytest.raises((MetadataValidationError, MetadataOutputError)):
        if text.startswith("{"):
            parse_metadata_output(text)
        else:
            PaperMetadataExtractor(FakeModel(SimpleNamespace(content=payload()))).extract(text)


@pytest.mark.parametrize(
    "text",
    [
        "",
        "not json",
        '{"title":"x","authors":["a"],"topics":["a","b","c"]',
        '{"title":"x","authors":["a"],"topics":["a","b","c"],"extra":1}',
        '{"title":"x","authors":["a"],"topics":["a","b",NaN]}',
    ],
)
def test_malformed_extra_empty_and_truncated_outputs_are_rejected(text):
    with pytest.raises(MetadataOutputError):
        parse_metadata_output(text)


def test_duplicate_json_keys_are_rejected():
    with pytest.raises(MetadataOutputError, match="duplicate JSON key"):
        parse_metadata_output(
            '{"title":"x","authors":["a"],"topics":["a","b","c"],'
            '"topics":["d","e","f"]}'
        )


def test_provider_output_limit_is_distinct_from_malformed_json():
    response = SimpleNamespace(
        content=payload(), response_metadata={"finish_reason": "length"}
    )
    with pytest.raises(MetadataIncompleteError, match="incomplete"):
        PaperMetadataExtractor(FakeModel(response)).extract("paper")


def test_model_failures_are_clear_and_do_not_require_credentials():
    class BrokenModel:
        def invoke(self, _messages):
            raise RuntimeError("offline failure")

    with pytest.raises(MetadataModelError, match="invocation failed"):
        PaperMetadataExtractor(BrokenModel()).extract("paper")


def test_invalid_model_injection_is_rejected():
    with pytest.raises(MetadataConfigurationError, match="invoke"):
        PaperMetadataExtractor(object())