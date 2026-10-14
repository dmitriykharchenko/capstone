from pathlib import Path

import pytest

from src.indexing.fact_extraction import (
    FACT_EXTRACTION_VERSION,
    Fact,
    FactExtractor,
    FactOutputError,
)
from src.indexing.paper_indexer import ExtractedPage, PaperIndexer
from src.indexing.paper_metadata import PaperMetadata


class RecordingCollection:
    def __init__(self, records=None):
        self.records = dict(records or {})
        self.deletes = []
        self.upserts = []
        self.gets = []

    def delete(self, **kwargs):
        self.deletes.append(kwargs)
        if "ids" in kwargs:
            for record_id in kwargs["ids"]:
                self.records.pop(record_id, None)
        else:
            paper_id = kwargs["where"]["paper_id"]
            self.records = {
                record_id: record
                for record_id, record in self.records.items()
                if record[1].get("paper_id") != paper_id
            }

    def upsert(self, **kwargs):
        self.upserts.append(kwargs)
        for index, record_id in enumerate(kwargs["ids"]):
            self.records[record_id] = (
                kwargs["documents"][index],
                kwargs["metadatas"][index],
            )

    def get(self, **kwargs):
        self.gets.append(kwargs)
        rows = list(self.records.items())
        if "where" in kwargs:
            paper_id = kwargs["where"]["paper_id"]
            rows = [row for row in rows if row[1][1].get("paper_id") == paper_id]
        return {
            "ids": [row[0] for row in rows],
            "documents": [row[1][0] for row in rows],
            "metadatas": [row[1][1] for row in rows],
        }


class MemoryFactGraph:
    def __init__(self):
        self.papers = {}
        self.versions = {}
        self.chunks = {}
        self.facts_by_chunk = {}

    def replace_paper(self, paper_id, title, authors, topics, **kwargs):
        self.papers[paper_id] = (title, tuple(authors), tuple(topics))
        if kwargs.get("clear_facts"):
            self._replace_facts(paper_id, (), {}, 1)
            self.versions.pop(paper_id, None)
        elif kwargs:
            self._replace_facts(
                paper_id,
                kwargs["chunks"],
                kwargs["facts_by_chunk"],
                kwargs["fact_version"],
            )

    def replace_paper_facts(self, paper_id, chunks, facts_by_chunk, fact_version):
        assert paper_id in self.papers
        self._replace_facts(paper_id, chunks, facts_by_chunk, fact_version)

    def _replace_facts(self, paper_id, chunks, facts_by_chunk, fact_version):
        for chunk_id in [
            chunk_id
            for chunk_id, node in self.chunks.items()
            if node.paper_id == paper_id
        ]:
            self.chunks.pop(chunk_id)
            self.facts_by_chunk.pop(chunk_id, None)
        self.chunks.update({node.chunk_id: node for node in chunks})
        self.facts_by_chunk.update(
            {chunk_id: tuple(facts) for chunk_id, facts in facts_by_chunk.items()}
        )
        self.versions[paper_id] = fact_version

    def remove_paper(self, paper_id):
        existed = self.papers.pop(paper_id, None) is not None
        self.versions.pop(paper_id, None)
        self._replace_facts(paper_id, (), {}, 1)
        self.versions.pop(paper_id, None)
        return existed

    def fact_version_for_paper(self, paper_id):
        return self.versions.get(paper_id)

    def fact_ids_for_paper(self, paper_id):
        return tuple(
            dict.fromkeys(
                fact.fact_id
                for chunk_id, node in self.chunks.items()
                if node.paper_id == paper_id
                for fact in self.facts_by_chunk[chunk_id]
            )
        )

    def chunks_for_facts(self, fact_ids):
        return {
            fact_id: tuple(
                chunk_id
                for chunk_id in self.chunks
                if any(
                    fact.fact_id == fact_id for fact in self.facts_by_chunk[chunk_id]
                )
            )
            for fact_id in fact_ids
        }


def touch_pdf(directory: Path) -> Path:
    path = directory / "paper.pdf"
    path.write_bytes(b"%PDF-1.4\n")
    return path


def metadata(_text):
    return PaperMetadata("Paper", ("Ada",), ("facts", "graphs", "retrieval"))


def test_replacement_and_removal_delete_only_orphaned_fact_vectors(tmp_path):
    first = touch_pdf(tmp_path)
    second = tmp_path / "second.pdf"
    second.write_bytes(b"%PDF-1.4\n")
    documents = RecordingCollection()
    facts = RecordingCollection()
    graph = MemoryFactGraph()
    old = Fact.create("claim", "Old", "The original claim.")
    shared = Fact.create("method", "Shared", "Both papers use this method.")
    replacement = Fact.create("claim", "New", "The replacement claim.")
    pages = {"paper": "original", "second": "second"}
    extracted = {
        "original": (old, shared),
        "second": (shared,),
        "replacement": (replacement,),
    }

    def extract(path):
        text = pages[path.stem]
        return [] if not text else [ExtractedPage(1, text)]

    worker = PaperIndexer(
        tmp_path,
        collection=documents,
        facts_collection=facts,
        graph_store=graph,
        extractor=extract,
        metadata_extractor=metadata,
        fact_extractor=lambda chunks: tuple(extracted[text] for text in chunks),
        batch_embedder=lambda texts: [[float(len(text))] for text in texts],
    )

    worker.index_paper(first)
    worker.index_paper(second)
    pages["paper"] = "replacement"
    worker.index_paper(first)

    assert facts.deletes[-1] == {"ids": [old.fact_id]}
    assert shared.fact_id in facts.records
    assert replacement.fact_id in facts.records

    pages["paper"] = ""
    assert worker.index_paper(first).status == "empty"

    assert facts.deletes[-1] == {"ids": [replacement.fact_id]}
    assert shared.fact_id in facts.records


def test_skipped_paper_backfills_from_stored_chunks_once_without_chunk_mutation(
    tmp_path,
):
    touch_pdf(tmp_path)
    documents = RecordingCollection(
        {
            "paper:chunk:000001": (
                "second",
                {"paper_id": "paper", "chunk_index": 1, "page": 2},
            ),
            "paper:chunk:000000": (
                "first",
                {"paper_id": "paper", "chunk_index": 0, "page": 1},
            ),
        }
    )
    facts = RecordingCollection()
    graph = MemoryFactGraph()
    graph.replace_paper("paper", "Paper", ("Ada",), ("facts",))
    calls = []

    def extract_facts(chunks):
        calls.append(tuple(chunks))
        return tuple((Fact.create("claim", text, f"Claim: {text}"),) for text in chunks)

    worker = PaperIndexer(
        tmp_path,
        collection=documents,
        facts_collection=facts,
        graph_store=graph,
        extractor=lambda _path: pytest.fail("skipped PDF must not be extracted"),
        metadata_extractor=metadata,
        fact_extractor=extract_facts,
        batch_embedder=lambda texts: [[float(len(text))] for text in texts],
        max_workers=1,
    )

    first = worker.index_directory({"paper"}).outcomes[0]
    second = worker.index_directory({"paper"}).outcomes[0]

    assert first.status == "indexed"
    assert second.status == "skipped"
    assert first.error is second.error is None
    assert calls == [("first", "second")]
    assert graph.fact_version_for_paper("paper") == FACT_EXTRACTION_VERSION
    assert documents.deletes == documents.upserts == []
    assert len(documents.gets) == 1
    assert len(facts.upserts) == 1


def test_exhausted_malformed_fact_output_indexes_vectors_with_safe_warning(tmp_path):
    paper = touch_pdf(tmp_path)
    documents = RecordingCollection()
    facts = RecordingCollection()
    graph = MemoryFactGraph()
    model_calls = []
    embedding_calls = []

    class MalformedFactModel:
        def invoke(self, _messages):
            model_calls.append(None)
            return type("Response", (), {"content": "not json"})()

    def embed(texts):
        embedding_calls.append(tuple(texts))
        return [[float(len(text))] for text in texts]

    worker = PaperIndexer(
        tmp_path,
        collection=documents,
        facts_collection=facts,
        graph_store=graph,
        extractor=lambda _path: [ExtractedPage(1, "evidence")],
        metadata_extractor=metadata,
        fact_extractor=FactExtractor(MalformedFactModel()),
        batch_embedder=embed,
    )

    outcome = worker.index_paper(paper)

    assert len(model_calls) == 2
    assert embedding_calls == [("evidence",)]
    assert outcome.status == "indexed"
    assert outcome.chunk_count == 1
    assert outcome.error is not None
    assert "Fact enrichment unavailable" in outcome.error
    assert len(outcome.error) <= 100
    assert "\n" not in outcome.error
    assert len(documents.upserts) == 1
    assert facts.deletes == facts.upserts == []
    assert graph.papers["paper"][0] == "Paper"
    assert graph.fact_version_for_paper("paper") is None
    assert graph.fact_ids_for_paper("paper") == ()


def test_vector_only_index_recovers_fact_enrichment_on_later_index(tmp_path):
    paper = touch_pdf(tmp_path)
    documents = RecordingCollection()
    facts = RecordingCollection()
    graph = MemoryFactGraph()
    responses = iter(
        (
            "not json",
            "still not json",
            '{"chunks":[{"chunk_index":0,"facts":[{"fact_type":"claim",'
            '"value":"Recovered","statement":"The claim recovered.","unit":null}]}]}',
        )
    )

    class RecoveringModel:
        def invoke(self, _messages):
            return type("Response", (), {"content": next(responses)})()

    worker = PaperIndexer(
        tmp_path,
        collection=documents,
        facts_collection=facts,
        graph_store=graph,
        extractor=lambda _path: [ExtractedPage(1, "evidence")],
        metadata_extractor=metadata,
        fact_extractor=FactExtractor(RecoveringModel()),
        batch_embedder=lambda texts: [[float(len(text))] for text in texts],
    )

    degraded = worker.index_paper(paper)
    recovered = worker.index_paper(paper)

    assert degraded.error is not None
    assert recovered.status == "indexed"
    assert recovered.error is None
    assert graph.fact_version_for_paper("paper") == FACT_EXTRACTION_VERSION
    assert len(graph.fact_ids_for_paper("paper")) == 1
    assert len(facts.upserts) == 1


def test_malformed_replacement_clears_stale_links_and_only_orphaned_vectors(tmp_path):
    paper = touch_pdf(tmp_path)
    second = tmp_path / "second.pdf"
    second.write_bytes(b"%PDF-1.4\n")
    documents = RecordingCollection()
    facts = RecordingCollection()
    graph = MemoryFactGraph()
    old = Fact.create("claim", "Old", "The old claim.")
    shared = Fact.create("method", "Shared", "Both papers use this method.")
    pages = {"paper": "original", "second": "second"}

    def extract(path):
        return [ExtractedPage(1, pages[path.stem])]

    def extract_facts(chunks):
        if chunks == ["replacement"]:
            raise FactOutputError("provider payload with unsafe details")
        return tuple(
            (old, shared) if text == "original" else (shared,) for text in chunks
        )

    worker = PaperIndexer(
        tmp_path,
        collection=documents,
        facts_collection=facts,
        graph_store=graph,
        extractor=extract,
        metadata_extractor=metadata,
        fact_extractor=extract_facts,
        batch_embedder=lambda texts: [[float(len(text))] for text in texts],
    )
    worker.index_paper(paper)
    worker.index_paper(second)

    pages["paper"] = "replacement"
    outcome = worker.index_paper(paper)

    assert outcome.status == "indexed"
    assert "unsafe details" not in outcome.error
    assert documents.records["paper:chunk:000000"][0] == "replacement"
    assert graph.fact_version_for_paper("paper") is None
    assert graph.fact_ids_for_paper("paper") == ()
    assert facts.deletes[-1] == {"ids": [old.fact_id]}
    assert shared.fact_id in facts.records


def test_failed_fact_backfill_preserves_vectors_and_recovers_later(tmp_path):
    touch_pdf(tmp_path)
    documents = RecordingCollection(
        {
            "paper:chunk:000000": (
                "stored evidence",
                {"paper_id": "paper", "chunk_index": 0, "page": 1},
            )
        }
    )
    facts = RecordingCollection()
    graph = MemoryFactGraph()
    graph.replace_paper("paper", "Paper", ("Ada",), ("facts",))
    malformed = True

    def extract_facts(chunks):
        if malformed:
            raise FactOutputError("malformed")
        return tuple((Fact.create("claim", text, f"Claim: {text}"),) for text in chunks)

    worker = PaperIndexer(
        tmp_path,
        collection=documents,
        facts_collection=facts,
        graph_store=graph,
        extractor=lambda _path: pytest.fail("skipped PDF must not be extracted"),
        metadata_extractor=metadata,
        fact_extractor=extract_facts,
        batch_embedder=lambda texts: [[float(len(text))] for text in texts],
        max_workers=1,
    )

    failed = worker.index_directory({"paper"}).outcomes[0]
    malformed = False
    recovered = worker.index_directory({"paper"}).outcomes[0]

    assert failed.status == recovered.status == "indexed"
    assert failed.chunk_count == recovered.chunk_count == 1
    assert failed.error is not None
    assert recovered.error is None
    assert documents.deletes == documents.upserts == []
    assert graph.fact_version_for_paper("paper") == FACT_EXTRACTION_VERSION


def test_non_output_fact_failure_remains_fatal(tmp_path):
    touch_pdf(tmp_path)
    documents = RecordingCollection()
    graph = MemoryFactGraph()

    def fail(_chunks):
        raise RuntimeError("provider unavailable")

    worker = PaperIndexer(
        tmp_path,
        collection=documents,
        facts_collection=RecordingCollection(),
        graph_store=graph,
        extractor=lambda _path: [ExtractedPage(1, "evidence")],
        metadata_extractor=metadata,
        fact_extractor=fail,
        batch_embedder=lambda texts: [[float(len(text))] for text in texts],
        max_workers=1,
    )

    outcome = worker.index_directory().outcomes[0]

    assert outcome.status == "failed"
    assert outcome.error == "RuntimeError: provider unavailable"
    assert documents.deletes == documents.upserts == []
