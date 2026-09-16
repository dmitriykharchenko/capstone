import assert from "node:assert/strict";
import test from "node:test";

import {
  beginGraphLoad,
  completeGraphLoad,
  failGraphLoad,
  initialGraphExplorerState,
} from "../src/slices/graph-explorer/graph-explorer-types.ts";

const paper = { id: "paper-a", kind: "paper", label: "Paper A" };

test("graph load state is in-memory, deterministic, and preserves valid selection", () => {
  const loading = beginGraphLoad(initialGraphExplorerState);
  const loaded = completeGraphLoad({ ...loading, selectedNodeId: paper.id }, [paper], []);
  assert.equal(loading.status, "loading");
  assert.equal(loaded.status, "success");
  assert.equal(loaded.selectedNodeId, paper.id);

  const emptied = completeGraphLoad(loaded, [], []);
  assert.equal(emptied.status, "empty");
  assert.equal(emptied.selectedNodeId, null);
});

test("dependency, store, malformed, and transport failures remain distinct", () => {
  for (const status of [
    "dependency-unavailable",
    "store-unavailable",
    "malformed-payload",
    "transport-error",
  ]) {
    const failed = failGraphLoad(
      { ...initialGraphExplorerState, nodes: [paper], selectedNodeId: paper.id },
      status,
      "retryable",
    );
    assert.equal(failed.status, status);
    assert.equal(failed.message, "retryable");
    assert.deepEqual(failed.nodes, []);
    assert.equal(failed.selectedNodeId, null);
  }
});