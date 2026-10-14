
from src.retrieval.paper_search import PaperSearchIndex


class FakeCollection:
    def __init__(self, result):
        self.result = result
        self.queries = []

    def query(self, **kwargs):
        self.queries.append(kwargs)
        return self.result


class FactDocumentCollection(FakeCollection):
    def __init__(self, result, records):
        super().__init__(result)
        self.records = records
        self.get_calls = []

    def get(self, *, ids, include):
        self.get_calls.append((ids, include))
        return {
            "ids": ids,
            "documents": [self.records[identifier]["text"] for identifier in ids],
            "metadatas": [self.records[identifier]["metadata"] for identifier in ids],
        }


class FactCollection:
    def __init__(self, result):
        self.result = result
        self.queries = []

    def query(self, **kwargs):
        self.queries.append(kwargs)
        return self.result


class FactGraph:
    def __init__(self, chunks_by_fact, facts_by_chunk):
        self.chunks_by_fact = chunks_by_fact
        self.facts_by_chunk = facts_by_chunk
        self.fact_calls = []

    def chunks_for_facts(self, fact_ids):
        return {fact_id: self.chunks_by_fact[fact_id] for fact_id in fact_ids}

    def facts_for_chunks(self, chunk_ids):
        self.fact_calls.append(chunk_ids)
        return {chunk_id: self.facts_by_chunk[chunk_id] for chunk_id in chunk_ids}


class FactValue:
    def __init__(self, fact_type, value, statement, unit=None):
        self.fact_type = fact_type
        self.value = value
        self.statement = statement
        self.unit = unit


class CorpusCollection:
    def __init__(self, records, query_result):
        self.records = records
        self.query_result = query_result
        self.queries = []
        self.get_calls = 0

    def get(self):
        self.get_calls += 1
        return {
            "ids": [record["id"] for record in self.records],
            "documents": [record["text"] for record in self.records],
            "metadatas": [
                {key: record[key] for key in ("paper_id", "chunk_index", "filename", "title", "page")}
                for record in self.records
            ],
        }

    def query(self, **kwargs):
        self.queries.append(kwargs)
        return self.query_result


def corpus_record(identifier, text, index):
    return {
        "id": identifier,
        "text": text,
        "paper_id": f"paper-{identifier}",
        "chunk_index": index,
        "filename": f"paper-{identifier}.pdf",
        "title": f"Paper {identifier}",
        "page": index + 1,
    }


def evaluation_query(records, distances):
    return {
        "ids": [[record["id"] for record in records]],
        "metadatas": [[
            {key: record[key] for key in ("paper_id", "chunk_index", "filename", "title", "page")}
            for record in records
        ]],
        "documents": [[record["text"] for record in records]],
        "distances": [distances],
    }


def raw_result(count=7):
    return {
        "metadatas": [[
            {
                "paper_id": f"paper-{index}",
                "chunk_index": index,
                "filename": f"paper-{index}.pdf",
                "title": f"Paper {index}",
                "page": index + 1,
            }
            for index in range(count)
        ]],
        "documents": [[f"full text {index}\nwith formatting" for index in range(count)]],
        "distances": [[index / 10 for index in range(count)]],
    }


def test_fact_vector_hits_add_connected_chunks_and_hydrate_facts():
    direct_chunk_id = "paper-a:chunk:000000"
    fact_chunk_id = "paper-b:chunk:000001"
    result = evaluation_query(
        [{**corpus_record("a", "general background", 0), "id": direct_chunk_id}], [0.8]
    )
    collection = FactDocumentCollection(
        result,
        {
            fact_chunk_id: {
                "text": "the method uses a named dataset",
                "metadata": {
                    "paper_id": "paper-b",
                    "chunk_index": 1,
                    "filename": "paper-b.pdf",
                    "title": "Paper B",
                    "page": 2,
                },
            }
        },
    )
    fact = FactValue("dataset", "Named Dataset", "The method uses Named Dataset.")
    facts = FactCollection({"ids": [["fact:dataset"]], "distances": [[0.1]]})
    graph = FactGraph({"fact:dataset": (fact_chunk_id,)}, {fact_chunk_id: (fact,)})
    index = PaperSearchIndex(
        collection=collection,
        facts_collection=facts,
        graph_store=graph,
        embedder=lambda _text: [1.0],
    )

    chunks = index.retrieve_chunks("Which dataset?", n_results=1)

    assert [chunk.chunk_id for chunk in chunks] == [fact_chunk_id]
    assert chunks[0].facts == (fact,)
    assert collection.get_calls == [([fact_chunk_id], ["metadatas", "documents"])]
    assert facts.queries[0]["include"] == ["distances"]
    assert graph.fact_calls == [[fact_chunk_id]]


def test_fact_vector_hits_apply_to_explicit_vector_retrieval():
    direct_chunk_id = "paper-a:chunk:000000"
    fact_chunk_id = "paper-b:chunk:000001"
    result = evaluation_query(
        [{**corpus_record("a", "general background", 0), "id": direct_chunk_id}], [0.8]
    )
    collection = FactDocumentCollection(
        result,
        {
            fact_chunk_id: {
                "text": "the method uses a named dataset",
                "metadata": {
                    "paper_id": "paper-b",
                    "chunk_index": 1,
                    "filename": "paper-b.pdf",
                    "title": "Paper B",
                    "page": 2,
                },
            }
        },
    )
    fact = FactValue("dataset", "Named Dataset", "The method uses Named Dataset.")
    index = PaperSearchIndex(
        collection=collection,
        facts_collection=FactCollection(
            {"ids": [["fact:dataset"]], "distances": [[0.1]]}
        ),
        graph_store=FactGraph(
            {"fact:dataset": (fact_chunk_id,)}, {fact_chunk_id: (fact,)}
        ),
        embedder=lambda _text: [1.0],
    )

    chunks = index.retrieve_evaluation_chunks("Which dataset?", "vector", n_results=1)

    assert [(chunk.chunk_id, chunk.facts) for chunk in chunks] == [(fact_chunk_id, (fact,))]


def test_evaluation_strategies_are_selectable_and_separate_from_chat_retrieval():
    records = [corpus_record("a", "unrelated text", 0), corpus_record("b", "needle needle", 1)]
    collection = CorpusCollection(records, evaluation_query(records, [0.1, 0.8]))
    index = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0])

    vector = index.retrieve_evaluation_chunks("needle", "vector")
    bm25 = index.retrieve_evaluation_chunks("needle", "bm25")
    assert [match.paper_id for match in vector] == ["paper-a", "paper-b"]
    assert [match.paper_id for match in bm25] == ["paper-b", "paper-a"]
    assert collection.get_calls == 1
    # The existing chat method retains its widened candidate query and hybrid rank.
    chat_collection = FakeCollection(raw_result(5))
    PaperSearchIndex(collection=chat_collection, embedder=lambda text: [1.0]).retrieve_chunks("question")
    assert chat_collection.queries[0]["n_results"] == 25


def test_hybrid_fusion_has_deterministic_provenance_tie_breaking():
    records = [corpus_record("b", "needle", 0), corpus_record("a", "other", 0)]
    collection = CorpusCollection(records, evaluation_query([records[1], records[0]], [0.1, 0.5]))
    matches = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0]).retrieve_evaluation_chunks(
        "needle", "hybrid"
    )
    assert [match.paper_id for match in matches] == ["paper-a", "paper-b"]
    assert [match.rank for match in matches] == [1, 2]
    assert matches[0].score == matches[1].score == 0.5


class CountingCorpusCollection(CorpusCollection):
    """A corpus collection that also reports a chunk count for staleness checks."""

    def __init__(self, records, query_result, count):
        super().__init__(records, query_result)
        self._count = count

    def count(self):
        return self._count


def test_refresh_bm25_index_persists_and_is_reused_from_disk(tmp_path):
    records = [corpus_record("a", "unrelated", 0), corpus_record("b", "needle needle", 1)]
    builder_collection = CountingCorpusCollection(
        records, evaluation_query(records, [0.1, 0.8]), count=2
    )
    path = tmp_path / "bm25_index.json"
    PaperSearchIndex(
        collection=builder_collection, embedder=lambda text: [1.0], bm25_index_path=path
    ).refresh_bm25_index()
    assert path.exists()

    class ExplodingGetCollection:
        def query(self, **kwargs):
            raise AssertionError("query must not run for the bm25 strategy")

        def get(self):
            raise AssertionError("corpus must not be scanned when a fresh index exists")

        def count(self):
            return 2

    index = PaperSearchIndex(
        collection=ExplodingGetCollection(), embedder=lambda text: [1.0], bm25_index_path=path
    )
    matches = index.retrieve_evaluation_chunks("needle", "bm25")
    assert [match.paper_id for match in matches] == ["paper-b", "paper-a"]
