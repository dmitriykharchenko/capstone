import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
let vite;
let toFlowEdges;
let toFlowNodes;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ toFlowEdges, toFlowNodes } = await vite.ssrLoadModule("/src/lib/graph-flow.ts"));
});

after(async () => vite.close());

const nodes = [
  { id: "topic-a", kind: "topic", label: "Retrieval" },
  { id: "paper-a", kind: "paper", label: "Paper A" },
  { id: "author-a", kind: "author", label: "Ada" },
];

test("adapts graph nodes to stable, typed flow positions and activation callbacks", () => {
  const activated = [];
  const flowNodes = toFlowNodes(nodes, (node) => activated.push(node.id));

  assert.deepEqual(flowNodes.map(({ id, type, position }) => [id, type, position]), [
    ["author-a", "graph", { x: 92, y: 42 }],
    ["paper-a", "graph", { x: 322, y: 42 }],
    ["topic-a", "graph", { x: 552, y: 42 }],
  ]);
  flowNodes.find((node) => node.id === "paper-a").data.onActivate();
  assert.deepEqual(activated, ["paper-a"]);
});

test("adapts canonical edge kinds to labeled flow edges", () => {
  const flowEdges = toFlowEdges([
    { sourceId: "paper-a", targetId: "author-a", kind: "authored_by" },
    { sourceId: "paper-a", targetId: "topic-a", kind: "has_topic" },
  ]);

  assert.deepEqual(
    flowEdges.map(({ id, source, target, sourceHandle, targetHandle, label, type }) => [
      id, source, target, sourceHandle, targetHandle, label, type,
    ]),
    [
      ["authored_by:paper-a:author-a", "paper-a", "author-a", "authored_by", "authored_by", "authored by", "straight"],
      ["has_topic:paper-a:topic-a", "paper-a", "topic-a", "has_topic", "has_topic", "has topic", "straight"],
    ],
  );
});

test("keeps paper routing and keyboard-accessible node activation in the component boundary", async () => {
  const graph = await readFile(new URL("../src/components/GraphExplorer.svelte", import.meta.url), "utf8");
  const node = await readFile(new URL("../src/components/GraphNode.svelte", import.meta.url), "utf8");

  assert.match(graph, /openPaper\(node\.id\)/);
  assert.match(graph, /navigateTo\(routeForName\("papers"\)\)/);
  assert.match(graph, /onnodeclick=/);
  assert.match(graph, /fitView/);
  assert.match(graph, /<Controls/);
  assert.match(graph, /panOnDrag/);
  assert.match(graph, /zoomOnScroll/);
  assert.match(node, /<button/);
  assert.match(node, /aria-label=/);
  assert.match(node, /aria-pressed=/);
  assert.match(node, /stopPropagation/);
});