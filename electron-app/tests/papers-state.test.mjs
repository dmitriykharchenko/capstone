import assert from "node:assert/strict";
import test from "node:test";

import {
  beginPaperIndex,
  completePaperIndex,
  hydratePaperStatuses,
  initialPapersState,
  markPaperIndexing,
  recordPaperIndexProgress,
  rejectPaperIndex,
} from "../src/slices/papers/papers-types.ts";

test("paper indexing advances without disturbing paper browsing", () => {
  const browsing = {
    ...initialPapersState,
    selectedId: "paper-1",
  };
  const started = beginPaperIndex(browsing, 3);
  const progressed = recordPaperIndexProgress(
    started,
    1,
    3,
    "paper",
    "paper.pdf",
    "indexed",
  );

  assert.equal(progressed.indexing, true);
  assert.equal(progressed.indexCompleted, 1);
  assert.equal(progressed.indexCurrentFile, "paper.pdf");
  assert.equal(progressed.indexCurrentStatus, "indexed");
  assert.equal(progressed.selectedId, "paper-1");
  assert.equal('pdfUrl' in progressed, false);
});

test("per-paper indicators move from indexing to a terminal outcome", () => {
  const started = beginPaperIndex(initialPapersState, 2);
  assert.deepEqual(started.indexStatuses, {});

  const indexing = markPaperIndexing(started, "paper-a", "paper-a.pdf");
  assert.equal(indexing.indexStatuses["paper-a"], "indexing");
  assert.equal(indexing.indexCurrentFile, "paper-a.pdf");

  const done = recordPaperIndexProgress(
    indexing,
    1,
    2,
    "paper-a",
    "paper-a.pdf",
    "indexed",
  );
  assert.equal(done.indexStatuses["paper-a"], "indexed");

  const nextIndexing = markPaperIndexing(done, "paper-b", "paper-b.pdf");
  assert.equal(nextIndexing.indexStatuses["paper-a"], "indexed");
  assert.equal(nextIndexing.indexStatuses["paper-b"], "indexing");
});

test("beginning a fresh run clears prior per-paper indicators", () => {
  const withStatuses = markPaperIndexing(
    beginPaperIndex(initialPapersState, 1),
    "paper-a",
    "paper-a.pdf",
  );
  const restarted = beginPaperIndex(withStatuses, 2);
  assert.deepEqual(restarted.indexStatuses, {});
});

test("paper indexing records terminal counts including partial failures", () => {
  const finished = completePaperIndex(
    beginPaperIndex(initialPapersState, 5),
    5,
    5,
    2,
    1,
    1,
    1,
  );

  assert.equal(finished.indexing, false);
  assert.equal(finished.indexedCount, 2);
  assert.equal(finished.indexEmptyCount, 1);
  assert.equal(finished.indexFailedCount, 1);
  assert.equal(finished.indexSkippedCount, 1);
  assert.equal(finished.indexCompleted, 5);
});

test("an already-indexed paper reports a skipped indicator this run", () => {
  const skipped = recordPaperIndexProgress(
    beginPaperIndex(initialPapersState, 1),
    1,
    1,
    "paper-a",
    "paper-a.pdf",
    "skipped",
  );

  assert.equal(skipped.indexStatuses["paper-a"], "skipped");
});

test("hydration restores persisted terminal indicators and ignores others", () => {
  const hydrated = hydratePaperStatuses(initialPapersState, [
    { paper_id: "paper-a", status: "indexed" },
    { paper_id: "paper-b", status: "empty" },
    { paper_id: "paper-c", status: "failed" },
    { paper_id: "paper-d", status: "indexing" },
    { paper_id: "paper-e", status: "bogus" },
  ]);

  assert.deepEqual(hydrated.indexStatuses, {
    "paper-a": "indexed",
    "paper-b": "empty",
    "paper-c": "failed",
  });
});

test("hydration merges over existing indicators without dropping them", () => {
  const withExisting = markPaperIndexing(
    beginPaperIndex(initialPapersState, 1),
    "paper-live",
    "paper-live.pdf",
  );
  const hydrated = hydratePaperStatuses(withExisting, [
    { paper_id: "paper-persisted", status: "indexed" },
  ]);

  assert.equal(hydrated.indexStatuses["paper-live"], "indexing");
  assert.equal(hydrated.indexStatuses["paper-persisted"], "indexed");
});

test("transport failures leave paper indexing retryable", () => {
  const failed = rejectPaperIndex(
    recordPaperIndexProgress(
      beginPaperIndex(initialPapersState, 2),
      1,
      2,
      "paper",
      "paper.pdf",
      "indexed",
    ),
    "bridge disconnected",
  );
  const retry = beginPaperIndex(failed, 2);

  assert.equal(failed.indexing, false);
  assert.equal(failed.indexError, "bridge disconnected");
  assert.equal(retry.indexing, true);
  assert.equal(retry.indexError, null);
  assert.equal(retry.indexCompleted, 0);
});