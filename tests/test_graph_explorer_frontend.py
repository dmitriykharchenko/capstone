from pathlib import Path


ROOT = Path(__file__).parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text()


def test_graph_transport_is_allow_listed_typed_and_validated():
    rust = read("app/src-tauri/src/lib.rs")
    graph_rust = read("app/src-tauri/src/graph.rs")
    api = read("app/src/lib/api.ts")

    assert "async fn graph_neighborhood" in rust
    assert "graph::neighborhood_request" in rust
    assert '"command": "graph_query"' in graph_rust
    assert '"query": "neighborhood"' in graph_rust
    assert "graph_neighborhood," in rust
    assert 'invoke<unknown>("graph_neighborhood"' in api
    assert "parseGraphNeighborhoodPayload" in api
    assert 'GraphEdgeKind = "authored_by" | "has_topic"' in read(
        "app/src/lib/graph-types.ts"
    )
    assert "authorship" not in read("app/src/lib/graph-types.ts")
    assert "paper_topic" not in read("app/src/lib/graph-types.ts")


def test_graph_state_and_saga_follow_store_ownership_patterns():
    store = read("app/src/store.ts")
    saga = read("app/src/slices/graph-explorer/sagas/graph-explorer-saga.ts")

    assert "graphExplorer: graphExplorerReducer" in store
    assert "graphExplorerSaga," in store
    assert "takeLatest(setPapers" in saga
    assert "takeLatest(paperIndexSucceeded" in saga
    assert ".sort()" in saga
    assert "api.graphNeighborhood" in saga


def test_graph_component_is_accessible_and_owns_a_dedicated_page():
    app = read("app/src/App.svelte")
    papers = read("app/src/components/PapersList.svelte")
    graph = read("app/src/components/GraphExplorer.svelte")

    assert "<PdfViewer />" in papers
    assert "<GraphExplorer />" not in papers
    assert 'value="graph"' in app
    assert "<GraphExplorer />" in app
    assert "runPaperSearch" in papers
    assert "runPaperIndex" in papers
    for text in (
        "Loading paper graph",
        "No graph data",
        "Graph dependency unavailable",
        "Graph store unavailable",
        "Malformed graph data",
        "Graph transport error",
        "Retry",
        "Selected node connections",
        "aria-pressed",
        "authored by",
        "has topic",
    ):
        assert text in graph
    assert "Show graph" not in graph
    assert "Hide graph" not in graph
    assert "prompt" not in graph.lower()
    assert "model output" not in graph.lower()