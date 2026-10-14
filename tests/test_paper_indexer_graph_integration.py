import threading
from pathlib import Path


from src.indexing.paper_indexer import ExtractedPage, PaperIndexer
from src.indexing.paper_metadata import PaperMetadata
from src.retrieval.paper_search import PaperSearchIndex


class RecordingCollection:
    def __init__(self):
        self.deletes = []
        self.upserts = []

    def delete(self, **kwargs):
        self.deletes.append(kwargs)

    def upsert(self, **kwargs):
        self.upserts.append(kwargs)


class MemoryGraph:
    def __init__(self):
        self.papers = {}
        self.replacements = []
        self.removals = []
        self.fail_replace = False
        self.fail_remove = False
        self._lock = threading.Lock()

    def replace_paper(self, paper_id, title, authors, topics):
        if self.fail_replace:
            raise RuntimeError("graph unavailable")
        value = (title, tuple(authors), tuple(topics))
        with self._lock:
            self.papers[paper_id] = value
            self.replacements.append((paper_id, *value))

    def remove_paper(self, paper_id):
        if self.fail_remove:
            raise RuntimeError("graph unavailable")
        with self._lock:
            self.removals.append(paper_id)
            return self.papers.pop(paper_id, None) is not None


def touch_pdf(directory: Path, name: str = "paper.pdf") -> Path:
    path = directory / name
    path.write_bytes(b"%PDF-1.4\n")
    return path


def metadata(title="Graph Paper", authors=("Ada", "Grace"), topics=None):
    return PaperMetadata(
        title,
        authors,
        topics or ("graphs", "retrieval", "databases"),
    )


def indexer(tmp_path, collection, graph, extractor, extractor_metadata=metadata):
    return PaperIndexer(
        tmp_path,
        collection=collection,
        graph_store=graph,
        extractor=extractor,
        metadata_extractor=extractor_metadata,
        batch_embedder=lambda texts: [[float(len(text))] for text in texts],
    )


def test_enriched_metadata_remains_backward_compatible_with_paper_search(tmp_path):
    pdf = touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    indexer(
        tmp_path,
        collection,
        graph,
        lambda _path: [ExtractedPage(1, "graph evidence")],
        lambda _text: metadata(),
    ).index_paper(pdf)
    write = collection.upserts[0]

    class QueryCollection:
        def query(self, **_kwargs):
            return {
                "metadatas": [write["metadatas"]],
                "documents": [write["documents"]],
                "distances": [[0.1]],
            }

    matches = PaperSearchIndex(
        collection=QueryCollection(), embedder=lambda _text: [1.0]
    ).search("graph")

    assert matches[0].paper_id == "paper"
    assert matches[0].title == "Graph Paper"
    assert matches[0].filename == "paper.pdf" and matches[0].page == 1
