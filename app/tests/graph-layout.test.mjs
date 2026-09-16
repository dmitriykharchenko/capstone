import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
let layoutGraph;
let vite;

before(async () => {
  vite = await createServer({ root: appRoot, appType: "custom", logLevel: "silent", server: { middlewareMode: true } });
  ({ layoutGraph } = await vite.ssrLoadModule("/src/lib/graph-layout.ts"));
});

after(async () => vite.close());

const nodes = Array.from({ length: 12 }, (_, index) => ({
  id: `node-${index}`,
  kind: index % 3 === 0 ? "paper" : index % 3 === 1 ? "author" : "topic",
  label: `Node ${index}`,
}));
const edges = nodes.slice(1, 8).map((node) => ({ sourceId: "node-0", targetId: node.id, kind: node.kind === "topic" ? "has_topic" : "authored_by" }));

function distance(layout, leftId, rightId) {
  const left = layout.nodes.find((node) => node.id === leftId);
  const right = layout.nodes.find((node) => node.id === rightId);
  return Math.hypot(left.x - right.x, left.y - right.y);
}

test("handles empty and single-node graphs deterministically inside bounds", () => {
  assert.deepEqual(layoutGraph([], []), { width: 520, height: 320, nodes: [] });
  const single = layoutGraph([nodes[0]], []);
  assert.deepEqual(single.nodes.map(({ id, x, y }) => [id, x, y]), [["node-0", 172, 132]]);
});

test("is deterministic and independent of node and edge input order", () => {
  const first = layoutGraph(nodes, edges);
  const second = layoutGraph([...nodes].reverse(), [...edges].reverse());
  assert.deepEqual(first, second);
  assert.deepEqual(layoutGraph(nodes, edges), first);
});

test("connectivity attracts linked nodes more than the disconnected layout", () => {
  const linked = layoutGraph(nodes, edges);
  const disconnected = layoutGraph(nodes, []);
  const linkedMean = edges.reduce((sum, edge) => sum + distance(linked, edge.sourceId, edge.targetId), 0) / edges.length;
  const disconnectedMean = edges.reduce((sum, edge) => sum + distance(disconnected, edge.sourceId, edge.targetId), 0) / edges.length;
  assert.ok(linkedMean < disconnectedMean, `${linkedMean} should be less than ${disconnectedMean}`);
});

test("keeps disconnected and dense graphs collision-spaced within bounded coordinates", () => {
  for (const graphEdges of [[], edges, nodes.flatMap((source, index) => nodes.slice(index + 1).map((target) => ({ sourceId: source.id, targetId: target.id, kind: "authored_by" })))]) {
    const layout = layoutGraph(nodes, graphEdges);
    assert.ok(layout.width <= 2400 && layout.height <= 2400);
    for (const node of layout.nodes) {
      assert.ok(node.x >= 48 && node.x <= layout.width - 224);
      assert.ok(node.y >= 48 && node.y <= layout.height - 104);
    }
    for (let left = 0; left < layout.nodes.length; left += 1) {
      for (let right = left + 1; right < layout.nodes.length; right += 1) {
        const dx = Math.abs(layout.nodes[left].x - layout.nodes[right].x);
        const dy = Math.abs(layout.nodes[left].y - layout.nodes[right].y);
        assert.ok(dx >= 200 || dy >= 80, `${layout.nodes[left].id} overlaps ${layout.nodes[right].id}`);
      }
    }
  }
});