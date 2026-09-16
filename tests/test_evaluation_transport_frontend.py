from pathlib import Path


ROOT = Path(__file__).parents[1]


def test_eval_reducer_and_saga_are_registered_without_ui_or_router_coupling():
    store = (ROOT / "app/src/store.ts").read_text()
    saga = (ROOT / "app/src/slices/evaluation/sagas/evaluation-saga.ts").read_text()

    assert "evaluation: evaluationReducer" in store
    assert "evaluationSaga" in store
    assert "takeLeading(runEvaluation" in saga
    assert "api.runEvaluation" in saga
    assert "App.svelte" not in saga
    assert "router" not in saga.lower()


def test_eval_transport_sends_only_canonical_hybrid_and_parsed_cases():
    api = (ROOT / "app/src/lib/api.ts").read_text()
    rust = (ROOT / "app/src-tauri/src/lib.rs").read_text()
    command = rust[
        rust.index("async fn evaluate_stream") : rust.index("/// Build and run")
    ]

    assert "retrievalType: EVALUATION_WIRE_RETRIEVAL_TYPE" in api
    assert (
        'EVALUATION_RETRIEVAL_TYPE = "multihop-hybrid" as const'
        in (ROOT / "app/src/lib/evaluation-types.ts").read_text()
    )
    assert (
        'EVALUATION_WIRE_RETRIEVAL_TYPE = "hybrid" as const'
        in (ROOT / "app/src/lib/evaluation-types.ts").read_text()
    )
    assert '"command": "evaluate"' in command
    assert '"retrieval_type": retrieval_type' in command
    assert '"test_cases": test_cases' in command
    for forbidden in (
        "api_key",
        "database_path",
        "chroma_directory",
        "papers_directory",
    ):
        assert forbidden not in command
