import json
from types import SimpleNamespace

import pytest

from src.indexing import fact_extraction
from src.indexing.fact_extraction import (
    FACT_EXTRACTION_SYSTEM_PROMPT,
    FactConfigurationError,
    FactExtractor,
    FactOutputError,
)


class FakeModel:
    def __init__(self, response):
        self.response = response
        self.calls = []

    def invoke(self, messages):
        self.calls.append(messages)
        value = self.response(messages) if callable(self.response) else self.response
        return SimpleNamespace(content=value)


class StrictSchemaFakeModel(FakeModel):
    def __init__(self, response):
        super().__init__(response)
        self.response_formats = []

    def bind(self, *, response_format):
        self.response_formats.append(response_format)
        return self


def response_for(messages):
    text = messages[-1].content
    payload = text.partition("<chunks_json>\n")[2].partition("\n</chunks_json>")[0]
    chunks = json.loads(payload)
    return json.dumps(
        {
            "chunks": [
                {
                    "chunk_index": index,
                    "facts": [
                        {
                            "fact_type": "claim",
                            "value": f"Claim {index}",
                            "statement": chunk["text"],
                            "unit": None,
                        }
                    ],
                }
                for index, chunk in enumerate(chunks)
            ]
        }
    )


@pytest.mark.parametrize(
    "payload",
    [
        "not json",
        '{"chunks":[],"extra":true}',
        '{"chunks":[{"chunk_index":0,"facts":[]}]}',
        json.dumps(
            {
                "chunks": [
                    {
                        "chunk_index": 0,
                        "facts": [
                            {
                                "fact_type": "opinion",
                                "value": "x",
                                "statement": "x",
                                "unit": None,
                            }
                        ],
                    },
                    {"chunk_index": 1, "facts": []},
                ]
            }
        ),
    ],
)
def test_invalid_model_output_fails_strictly(payload):
    model = FakeModel(payload)

    with pytest.raises(FactOutputError):
        FactExtractor(model).extract(["one", "two"])

    assert len(model.calls) == 2


def test_extractor_requests_provider_strict_json_schema_when_supported():
    model = StrictSchemaFakeModel(response_for)

    FactExtractor(model).extract(["one"])

    assert len(model.response_formats) == 1
    schema = model.response_formats[0]["json_schema"]
    assert schema["name"] == "fact_extraction"
    assert schema["strict"] is True
    assert schema["schema"]["additionalProperties"] is False


def test_default_model_without_strict_json_schema_support_fails_clearly(monkeypatch):
    class InvokeOnlyModel:
        def invoke(self, _messages):
            pytest.fail("unsupported model must not be invoked")

    monkeypatch.setattr(
        fact_extraction, "_create_chat_openai", lambda _config: InvokeOnlyModel()
    )

    with pytest.raises(FactConfigurationError, match="strict JSON schema binding"):
        FactExtractor().extract(["one"])


def test_malformed_output_retries_once_and_recovers():
    responses = iter(["not json", response_for])

    def response(messages):
        value = next(responses)
        return value(messages) if callable(value) else value

    model = FakeModel(response)

    result = FactExtractor(model).extract(["one"])

    assert len(model.calls) == 2
    assert result[0][0].value == "Claim 0"


def test_extractor_batches_bounded_untrusted_chunks_and_preserves_order():
    model = FakeModel(response_for)
    chunks = [f"chunk {index} " + ("x" * 13_000) for index in range(5)]

    result = FactExtractor(model).extract(chunks)

    assert len(model.calls) == 3
    assert [facts[0].value for facts in result] == [
        "Claim 0",
        "Claim 1",
        "Claim 0",
        "Claim 1",
        "Claim 0",
    ]
    assert all(call[0].content == FACT_EXTRACTION_SYSTEM_PROMPT for call in model.calls)
    assert all("untrusted" in call[0].content for call in model.calls)
    assert all(len(facts[0].statement) == 12_000 for facts in result)
