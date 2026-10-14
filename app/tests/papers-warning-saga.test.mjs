import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga, stdChannel } from "redux-saga";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
const settle = () => new Promise((resolve) => setImmediate(resolve));
let vite, api, papersSaga, runPaperIndex;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ api } = await vite.ssrLoadModule("/src/lib/api.ts"));
  ({ papersSaga } = await vite.ssrLoadModule(
    "/src/slices/papers/sagas/papers-saga.ts",
  ));
  ({ runPaperIndex } = await vite.ssrLoadModule(
    "/src/slices/papers/papers-slice.ts",
  ));
});

after(async () => vite.close());

test("paper saga preserves a nonfatal fact warning on progress", async () => {
  const warning = "Fact enrichment unavailable: invalid model output after retry.";
  const input = stdChannel();
  const actions = [];
  api.indexPapersStream = (onEvent) =>
    new Promise((resolve) => {
      setImmediate(() => {
        onEvent({ event: "start", completed: 0, total: 1 });
        onEvent({
          event: "progress",
          completed: 1,
          total: 1,
          paperId: "paper",
          filename: "paper.pdf",
          status: "indexed",
          chunkCount: 2,
          error: warning,
        });
        onEvent({
          event: "done",
          completed: 1,
          total: 1,
          indexed: 1,
          empty: 0,
          failed: 0,
          skipped: 0,
        });
        resolve();
      });
    });
  api.bm25Status = async () => ({ built: true, chunk_count: 2, paper_ids: ["paper"] });
  const task = runSaga(
    {
      channel: input,
      dispatch: (action) => actions.push(action),
      getState: () => ({ papers: { pdfUrl: null } }),
    },
    papersSaga,
  );

  await settle();
  input.put(runPaperIndex());
  await settle();
  await settle();

  const progress = actions.find((action) => action.type === "papers/indexProgress");
  assert.ok(progress, JSON.stringify(actions));
  assert.deepEqual(progress.payload, [1, 1, "paper", "paper.pdf", "indexed", warning]);
  task.cancel();
  await task.toPromise();
});