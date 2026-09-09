import pytest

from src.paper_search import PaperSearchError, PaperSearchIndex


class FakeCollection:
    def __init__(self, result):
        self.result = result
        self.queries = []

    def query(self, **kwargs):
        self.queries.append(kwargs)
        return self.result


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


def test_retrieve_chunks_preserves_rank_full_text_and_provenance():
    collection = FakeCollection(raw_result(5))
    index = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0, 2.0])

    chunks = index.retrieve_chunks("question")

    # No candidate shares terms with the query, so BM25 contributes nothing and
    # the hybrid rank falls back to the vector-similarity order.
    assert [chunk.rank for chunk in chunks] == [1, 2, 3, 4, 5]
    assert chunks[0].text == "full text 0\nwith formatting"
    assert chunks[0].score == 0.5
    assert [chunk.score for chunk in chunks] == sorted(
        (chunk.score for chunk in chunks), reverse=True
    )
    assert chunks[2].paper_id == "paper-2"
    assert chunks[2].chunk_index == 2
    assert chunks[2].filename == "paper-2.pdf"
    assert chunks[2].title == "Paper 2"
    assert chunks[2].page == 3
    # The candidate budget is widened past n_results so BM25 can resurface
    # lexically strong chunks that vector search ranked lower.
    assert collection.queries == [{
        "query_embeddings": [[1.0, 2.0]],
        "n_results": 25,
        "include": ["metadatas", "documents", "distances"],
    }]


def test_retrieve_chunks_hybrid_ranks_strong_lexical_match_above_vector():
    result = {
        "metadatas": [[
            {"paper_id": "paper-a", "chunk_index": 0, "filename": "paper-a.pdf",
             "title": "Paper A", "page": 1},
            {"paper_id": "paper-b", "chunk_index": 1, "filename": "paper-b.pdf",
             "title": "Paper B", "page": 2},
            {"paper_id": "paper-c", "chunk_index": 2, "filename": "paper-c.pdf",
             "title": "Paper C", "page": 3},
        ]],
        "documents": [[
            "graph neural network survey",
            "transformer attention transformer attention model",
            "clustering methods overview",
        ]],
        "distances": [[0.05, 0.4, 0.8]],
    }
    collection = FakeCollection(result)
    index = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0])

    chunks = index.retrieve_chunks("transformer attention")

    # paper-b has a weaker vector score than paper-a but a strong lexical match,
    # so the hybrid blend lifts it to the top and re-numbers the ranks.
    assert [chunk.paper_id for chunk in chunks] == ["paper-b", "paper-a", "paper-c"]
    assert [chunk.rank for chunk in chunks] == [1, 2, 3]
    assert chunks[0].score > chunks[1].score > chunks[2].score


def test_retrieve_chunks_honors_limit_and_empty_results():
    collection = FakeCollection(raw_result())
    index = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0])
    assert len(index.retrieve_chunks("question", n_results=3)) == 3

    empty = FakeCollection({"metadatas": [[]], "documents": [[]], "distances": [[]]})
    assert PaperSearchIndex(
        collection=empty, embedder=lambda text: [1.0]
    ).retrieve_chunks("question") == []


def test_existing_paper_search_still_collapses_to_one_match_per_paper():
    result = raw_result(3)
    result["metadatas"][0][1]["paper_id"] = "paper-0"
    collection = FakeCollection(result)
    index = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0])

    matches = index.search("question", n_results=2)

    assert [match.paper_id for match in matches] == ["paper-0", "paper-2"]
    assert all("snippet" in match.__dataclass_fields__ for match in matches)


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


def test_bm25_evaluation_scores_the_complete_corpus():
    records = [
        corpus_record("a", "no matching terms", 0),
        corpus_record("b", "needle appears in a lower vector candidate", 1),
    ]
    collection = CorpusCollection(records, evaluation_query(records, [0.1, 0.9]))
    matches = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0]).retrieve_evaluation_chunks(
        "needle", "bm25"
    )
    assert matches[0].paper_id == "paper-b"
    assert collection.queries == []
    assert collection.get_calls == 1


def test_hybrid_fusion_has_deterministic_provenance_tie_breaking():
    records = [corpus_record("b", "needle", 0), corpus_record("a", "other", 0)]
    collection = CorpusCollection(records, evaluation_query([records[1], records[0]], [0.1, 0.5]))
    matches = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0]).retrieve_evaluation_chunks(
        "needle", "hybrid"
    )
    assert [match.paper_id for match in matches] == ["paper-a", "paper-b"]
    assert [match.rank for match in matches] == [1, 2]
    assert matches[0].score == matches[1].score == 0.5


@pytest.mark.parametrize("strategy", ["", "dense", "BM25", None])
def test_evaluation_retrieval_rejects_invalid_strategy(strategy):
    collection = CorpusCollection([], {"ids": [[]], "metadatas": [[]], "documents": [[]], "distances": [[]]})
    index = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0])
    with pytest.raises(ValueError, match="strategy must be one of"):
        index.retrieve_evaluation_chunks("question", strategy)


def test_evaluation_retrieval_rejects_incomplete_provenance():
    collection = CorpusCollection([], {"ids": [[]], "metadatas": [[]], "documents": [[]], "distances": [[]]})
    collection.get = lambda: {"ids": ["a"], "documents": ["text"], "metadatas": [{"paper_id": "paper-a"}]}
    index = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0])
    with pytest.raises(PaperSearchError, match="chunk_index"):
        index.retrieve_evaluation_chunks("question", "bm25")


def test_evaluation_matches_include_full_text_and_provenance_and_cap_at_five():
    records = [corpus_record(str(index), f"text {index}", index) for index in range(6)]
    collection = CorpusCollection(records, evaluation_query(records[:5], [index / 10 for index in range(5)]))
    matches = PaperSearchIndex(collection=collection, embedder=lambda text: [1.0]).retrieve_evaluation_chunks(
        "question", "bm25", n_results=20
    )
    assert len(matches) == 5
    assert [match.rank for match in matches] == list(range(1, 6))
    assert matches[0].text == "text 0"
    assert (matches[0].paper_id, matches[0].filename, matches[0].title, matches[0].page) == (
        "paper-0", "paper-0.pdf", "Paper 0", 1
    )