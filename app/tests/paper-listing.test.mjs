import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { createServer } from "vite";

let vite;
let filterPaperList;
let paperListEntries;

before(async () => {
  vite = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)),
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ filterPaperList, paperListEntries } = await vite.ssrLoadModule(
    "/src/lib/paper-listing.ts",
  ));
});

after(async () => vite.close());

const papers = [
  {
    id: "paper-a",
    filename: "paper-a.pdf",
    title: "Retrieval Systems",
    authors: ["Stored Author"],
    topics: ["Stored Topic"],
    metadata_status: "available",
    size_bytes: 1,
  },
  {
    id: "paper-b",
    filename: "paper-b.pdf",
    title: "Graph Databases",
    authors: [],
    topics: ["Semantic Retrieval"],
    metadata_status: "missing",
    size_bytes: 2,
  },
];

test("filters title, authors, and topics with case-insensitive substring matching", () => {
  const entries = paperListEntries(papers);

  assert.deepEqual(filterPaperList(entries, "system").map((entry) => entry.paper.id), ["paper-a"]);
  assert.deepEqual(filterPaperList(entries, "STORED AUTH").map((entry) => entry.paper.id), ["paper-a"]);
  assert.deepEqual(filterPaperList(entries, "retriev").map((entry) => entry.paper.id), ["paper-a", "paper-b"]);
  assert.deepEqual(filterPaperList(entries, "missing"), []);
});