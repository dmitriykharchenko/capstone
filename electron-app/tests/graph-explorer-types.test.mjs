import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
let vite;
let GraphPayloadError, mergeGraphNeighborhoods, parseGraphNeighborhoodPayload;
let connectedNodes, layoutGraph;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ GraphPayloadError, mergeGraphNeighborhoods, parseGraphNeighborhoodPayload } =
    await vite.ssrLoadModule("/src/lib/graph-types.ts"));
  ({ connectedNodes, layoutGraph } = await vite.ssrLoadModule("/src/lib/graph-layout.ts"));
});

after(async () => vite.close());

const raw = {
  enabled: true,
  available: true,
  query: "neighborhood",
  neighborhood: {
    nodes: [
      { node_id: "topic-z", kind: "topic", label: "Z topic" },
      { node_id: "paper-b", kind: "paper", label: "B paper" },
      { node_id: "author-a", kind: "author", label: "A author" },
    ],
    edges: [
      { source_id: "paper-b", target_id: "topic-z", kind: "has_topic" },
      { source_id: "paper-b", target_id: "author-a", kind: "authored_by" },
    ],
  },
};

test("validates and normalizes deterministic graph payload order", () => {
  const parsed = parseGraphNeighborhoodPayload(raw);
  assert.deepEqual(parsed.nodes.map((node) => node.id), ["author-a", "paper-b", "topic-z"]);
  assert.deepEqual(parsed.edges.map((edge) => edge.kind), ["authored_by", "has_topic"]);
  assert.equal(parsed.reason, null);
});

test("rejects malformed node and edge payloads at the frontend boundary", () => {
  assert.throws(
    () => parseGraphNeighborhoodPayload({ ...raw, neighborhood: { nodes: [], edges: raw.neighborhood.edges } }),
    GraphPayloadError,
  );
  assert.throws(
    () => parseGraphNeighborhoodPayload({ ...raw, query: "raw_cozoscript" }),
    /query must be neighborhood/,
  );
  assert.throws(
    () => parseGraphNeighborhoodPayload({
      ...raw,
      neighborhood: {
        ...raw.neighborhood,
        edges: [{ ...raw.neighborhood.edges[0], kind: "authorship" }],
      },
    }),
    GraphPayloadError,
  );
});

test("merges duplicate neighborhoods and lays out nodes stably from connectivity", () => {
  const parsed = parseGraphNeighborhoodPayload(raw);
  const merged = mergeGraphNeighborhoods([parsed, parsed]);
  const first = layoutGraph(merged.nodes, merged.edges);
  const second = layoutGraph([...merged.nodes].reverse(), [...merged.edges].reverse());

  assert.equal(merged.nodes.length, 3);
  assert.equal(merged.edges.length, 2);
  assert.deepEqual(first, second);
  assert.deepEqual(first, second);
  assert.deepEqual(
    connectedNodes("paper-b", merged.nodes, merged.edges).map((node) => node.id),
    ["author-a", "topic-z"],
  );
});