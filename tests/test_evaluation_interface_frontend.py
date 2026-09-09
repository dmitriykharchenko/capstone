import re
from pathlib import Path


ROOT = Path(__file__).parents[1]


def test_evaluation_interface_dispatches_validated_intent_without_transport_or_url_ownership():
    component = (ROOT / "app/src/components/RagEvals.svelte").read_text()
    validation = component.index("parseEvaluationTestCases(text)")
    dispatch = component.index("store.dispatch(runEvaluation", validation)

    assert validation < dispatch
    assert 'accept=".json"' in component
    assert re.findall(r'<SelectItem value="([^"]+)"', component) == [
        "vector",
        "bm25",
        "hybrid",
    ]
    assert "api.runEvaluation" not in component
    assert "window.location" not in component
    assert "window.history" not in component


def test_evaluation_route_and_accessible_result_contract_are_present():
    app = (ROOT / "app/src/App.svelte").read_text()
    routes = (ROOT / "app/src/slices/router/router-types.ts").read_text()
    component = (ROOT / "app/src/components/RagEvals.svelte").read_text()

    assert '{ id: "evals", label: "RAG Evals" }' in app
    assert 'case "/evals"' in routes
    assert 'role="progressbar"' in component
    assert 'role="alert"' in component
    assert "<details" in component
    for label in (
        "Context precision",
        "Context recall",
        "Relevant chunk ranking",
        "Context utilization",
        "Semantic similarity",
        "Completeness",
        "Hallucination rate",
    ):
        assert label in component