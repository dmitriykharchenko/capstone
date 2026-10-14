from pathlib import Path

from src.cli.config import CLIConfig
from src.cli.models import MockChatModel, build_paper_indexer, build_paper_search_index
from src.indexing.paper_metadata import PaperMetadataExtractor


def config(tmp_path: Path, api_key="test-key") -> CLIConfig:
    return CLIConfig(
        database_path=tmp_path / "history.sqlite3",
        papers_directory=tmp_path / "papers",
        chroma_directory=tmp_path / "chroma",
        chat_model="chat-model",
        chat_base_url="https://example.test/v1",
        embedding_model="embed-model",
        embedding_base_url="https://embed.example.test/v1",
        api_key=api_key,
        system_prompt=None,
    )


def test_search_builder_wires_fact_collection_and_graph(monkeypatch, tmp_path):
    captured = {}
    graph = object()
    facts_collection = object()

    monkeypatch.setattr("src.cli.models.chromadb_available", lambda: True)
    monkeypatch.setattr(
        "src.cli.models.CozoGraphStore",
        lambda path: captured.setdefault("graph_path", path) and graph,
    )
    monkeypatch.setattr(
        "src.cli.models.open_facts_collection",
        lambda path: captured.setdefault("facts_path", path) and facts_collection,
    )
    monkeypatch.setattr(
        "src.cli.models.PaperSearchIndex",
        lambda **options: captured.update(options) or object(),
    )

    assert build_paper_search_index(config(tmp_path)) is not None
    assert captured["graph_path"] == tmp_path / "paper_graph.db"
    assert captured["facts_path"] == tmp_path / "chroma"
    assert captured["graph_store"] is graph
    assert captured["facts_collection"] is facts_collection


def test_cli_builder_closes_graph_store_when_indexer_construction_fails(
    monkeypatch, tmp_path
):
    closed = []

    class FakeGraph:
        def close(self):
            closed.append(True)

    monkeypatch.setattr("src.cli.models.chromadb_available", lambda: True)
    monkeypatch.setattr("src.cli.models.CozoGraphStore", lambda _path: FakeGraph())
    monkeypatch.setattr(
        "src.cli.models.PaperIndexer",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(RuntimeError("broken")),
    )

    assert build_paper_indexer(config(tmp_path)) is None
    assert closed == [True]


def test_offline_cli_model_returns_valid_deterministic_paper_metadata():
    extractor = PaperMetadataExtractor(model=MockChatModel())

    result = extractor("A Local Paper\nBody")

    assert result.title == "A Local Paper"
    assert result.authors == ("Unknown author",)
    assert result.topics == ("research", "paper", "local corpus")
