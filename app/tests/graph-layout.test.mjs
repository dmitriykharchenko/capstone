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

const nodes = [
  { id: "topic-shared", kind: "topic", label: "Shared topic" },
  { id: "paper-shared", kind: "paper", label: "Shared paper" },
  { id: "author-b", kind: "author", label: "Bob" },
  { id: "paper-b", kind: "paper", label: "Beta paper" },
  { id: "topic-a", kind: "topic", label: "Alpha topic" },
  { id: "author-a", kind: "author", label: "Ada" },
  { id: "paper-a", kind: "paper", label: "Alpha paper" },
];
const edges = [
  { sourceId: "paper-shared", targetId: "author-b", kind: "authored_by" },
  { sourceId: "paper-b", targetId: "author-b", kind: "authored_by" },
  { sourceId: "paper-shared", targetId: "topic-shared", kind: "has_topic" },
  { sourceId: "paper-a", targetId: "topic-a", kind: "has_topic" },
  { sourceId: "paper-a", targetId: "author-a", kind: "authored_by" },
  { sourceId: "paper-shared", targetId: "author-a", kind: "authored_by" },
  { sourceId: "paper-b", targetId: "topic-shared", kind: "has_topic" },
];

test("traverses stable author neighborhoods and deduplicates first encounters", () => {
  const layout = layoutGraph(nodes, edges);
  assert.deepEqual(layout.nodes.map((node) => node.id), [
    "author-a", "paper-a", "paper-shared", "topic-a", "topic-shared", "author-b", "paper-b",
  ]);
  assert.equal(layout.nodes.filter((node) => node.id === "paper-shared").length, 1);
  assert.equal(layout.nodes.filter((node) => node.id === "topic-shared").length, 1);
  const byId = Object.fromEntries(layout.nodes.map((node) => [node.id, node]));
  assert.ok(byId["author-a"].x < byId["paper-a"].x && byId["paper-a"].x < byId["topic-a"].x);
  assert.ok(byId["author-b"].x > byId["topic-a"].x + 176);
});

test("is deterministic and independent of node, edge, and edge-direction input order", () => {
  const first = layoutGraph(nodes, edges);
  const reversedEdges = [...edges].reverse().map((edge) => ({ ...edge, sourceId: edge.targetId, targetId: edge.sourceId }));
  assert.deepEqual(layoutGraph([...nodes].reverse(), reversedEdges), first);
  assert.deepEqual(layoutGraph(nodes, edges), first);
});

test("keeps authorless, disconnected, and dense layouts spaced inside their bounds", () => {
  const denseNodes = Array.from({ length: 24 }, (_, index) => ({
    id: `node-${index}`, kind: index < 12 ? "paper" : "topic", label: `Node ${index}`,
  }));
  const denseEdges = Array.from({ length: 12 }, (_, index) => ({
    sourceId: `node-${index}`, targetId: `node-${index + 12}`, kind: "has_topic",
  }));
  for (const [graphNodes, graphEdges] of [
    [[], []],
    [[{ id: "paper-only", kind: "paper", label: "Only" }], []],
    [denseNodes, denseEdges],
  ]) {
    const layout = layoutGraph(graphNodes, graphEdges);
    assert.ok(layout.width >= 520 && layout.width <= 2400 && layout.height >= 320);
    for (const node of layout.nodes) {
      assert.ok(node.x >= 48 && node.x + 176 <= layout.width - 48);
      assert.ok(node.y >= 48 && node.y + 56 <= layout.height - 48);
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