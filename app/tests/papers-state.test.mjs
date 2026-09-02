import assert from "node:assert/strict";
import test from "node:test";

import {
  beginPaperIndex,
  completePaperIndex,
  initialPapersState,
  recordPaperIndexProgress,
  rejectPaperIndex,
} from "../src/slices/papers/papers-types.ts";

test("paper indexing advances without disturbing paper browsing", () => {
  const browsing = {
    ...initialPapersState,
    selectedId: "paper-1",
    pdfUrl: "blob:paper-1",
  };
  const started = beginPaperIndex(browsing, 3);
  const progressed = recordPaperIndexProgress(
    started,
    1,
    3,
    "paper.pdf",
    "indexed",
  );

  assert.equal(progressed.indexing, true);
  assert.equal(progressed.indexCompleted, 1);
  assert.equal(progressed.indexCurrentFile, "paper.pdf");
  assert.equal(progressed.indexCurrentStatus, "indexed");
  assert.equal(progressed.selectedId, "paper-1");
  assert.equal(progressed.pdfUrl, "blob:paper-1");
});

test("paper indexing records terminal counts including partial failures", () => {
  const finished = completePaperIndex(
    beginPaperIndex(initialPapersState, 4),
    4,
    4,
    2,
    1,
    1,
  );

  assert.equal(finished.indexing, false);
  assert.equal(finished.indexedCount, 2);
  assert.equal(finished.indexEmptyCount, 1);
  assert.equal(finished.indexFailedCount, 1);
  assert.equal(finished.indexCompleted, 4);
});

test("transport failures leave paper indexing retryable", () => {
  const failed = rejectPaperIndex(
    recordPaperIndexProgress(
      beginPaperIndex(initialPapersState, 2),
      1,
      2,
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