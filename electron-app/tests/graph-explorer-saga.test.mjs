import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga } from "redux-saga";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
let vite, loadGraphForPapers, GraphPayloadError;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ loadGraphForPapers } = await vite.ssrLoadModule(
    "/src/slices/graph-explorer/sagas/graph-explorer-saga.ts",
  ));
  ({ GraphPayloadError } = await vite.ssrLoadModule("/src/lib/graph-types.ts"));
});

after(async () => vite.close());

const paper = (id) => ({ id, filename: `${id}.pdf`, title: id, size_bytes: 10 });
const response = (node) => ({
  enabled: true,
  available: true,
  reason: null,
  nodes: [node],
  edges: [],
});

async function run(papers, graphNeighborhood) {
  const actions = [];
  await runSaga(
    { dispatch: (action) => actions.push(action) },
    loadGraphForPapers,
    papers,
    { graphNeighborhood },
  ).toPromise();
  return actions;
}

test("requests sorted unique paper neighborhoods and merges graph state", async () => {
  const requested = [];
  const actions = await run([paper("paper-b"), paper("paper-a"), paper("paper-a")], async (id, depth) => {
    requested.push([id, depth]);
    return response({ id, kind: "paper", label: id });
  });

  assert.deepEqual(requested, [["paper-a", 1], ["paper-b", 1]]);
  assert.deepEqual(actions.map((action) => action.type), [
    "graphExplorer/loading",
    "graphExplorer/loaded",
  ]);
  assert.deepEqual(actions[1].payload[0].map((node) => node.id), ["paper-a", "paper-b"]);
});

test("passes canonical edge kinds into loaded graph state", async () => {
  const actions = await run([paper("paper")], async (id) => ({
    ...response({ id, kind: "paper", label: id }),
    nodes: [
      { id, kind: "paper", label: id },
      { id: "author-a", kind: "author", label: "Ada" },
      { id: "topic-a", kind: "topic", label: "Graphs" },
    ],
    edges: [
      { sourceId: id, targetId: "author-a", kind: "authored_by" },
      { sourceId: id, targetId: "topic-a", kind: "has_topic" },
    ],
  }));

  assert.deepEqual(actions[1].payload[1].map((edge) => edge.kind), ["authored_by", "has_topic"]);
});

test("surfaces dependency and store unavailability without transport leakage", async () => {
  for (const unavailable of [
    { enabled: false, available: false, reason: "dependency missing", status: "dependency-unavailable" },
    { enabled: true, available: false, reason: "store closed", status: "store-unavailable" },
  ]) {
    const actions = await run([paper("paper")], async () => ({
      ...unavailable,
      nodes: [],
      edges: [],
    }));
    assert.equal(actions.at(-1).type, "graphExplorer/failed");
    assert.equal(actions.at(-1).payload[0], unavailable.status);
  }
});

test("distinguishes malformed payload validation from transport failure", async () => {
  const malformed = await run([paper("paper")], async () => {
    throw new GraphPayloadError("bad edge");
  });
  const transport = await run([paper("paper")], async () => {
    throw new Error("secret backend detail");
  });

  assert.deepEqual(malformed.at(-1).payload, ["malformed-payload", "Graph data was malformed."]);
  assert.equal(transport.at(-1).payload[0], "transport-error");
  assert.equal(transport.at(-1).payload[1].includes("secret"), false);
});