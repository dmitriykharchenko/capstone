from src.paper_search import PaperSearchIndex


class FakeCollection:
    def __init__(self, result):
        self.result = result
        self.queries = []

    def query(self, **kwargs):
        self.queries.append(kwargs)
        return self.result


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

    assert [chunk.rank for chunk in chunks] == [1, 2, 3, 4, 5]
    assert chunks[0].text == "full text 0\nwith formatting"
    assert chunks[1].score == 0.9091
    assert chunks[2].paper_id == "paper-2"
    assert chunks[2].chunk_index == 2
    assert chunks[2].filename == "paper-2.pdf"
    assert chunks[2].title == "Paper 2"
    assert chunks[2].page == 3
    assert collection.queries == [{
        "query_embeddings": [[1.0, 2.0]],
        "n_results": 5,
        "include": ["metadatas", "documents", "distances"],
    }]


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