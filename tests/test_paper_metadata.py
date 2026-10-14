import json
from types import SimpleNamespace

import pytest

from src.indexing.paper_metadata import (
    MetadataOutputError,
    MetadataValidationError,
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
