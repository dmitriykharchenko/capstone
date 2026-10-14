import {
  call,
  put,
  select,
  take,
  takeLatest,
  takeLeading,
} from "typed-redux-saga";
import { eventChannel, END, type EventChannel } from "redux-saga";
import {
  api,
  pdfBytesToObjectUrl,
  type IndexStreamEvent,
} from "../../../lib/api";
import {
  loadPapers,
  openPaper,
  paperIndexFailed,
  paperIndexPaperStarted,
  paperIndexProgress,
  paperIndexStarted,
  paperIndexSucceeded,
  papersFailed,
  papersLoading,
  pdfFailed,
  pdfLoading,
  resetIndices,
  resetIndicesFailed,
  resetIndicesSucceeded,
  runPaperIndex,
  setBm25Statuses,
  setPaperStatuses,
  setPapers,
  setPdfUrl,
} from "../papers-slice";

function createIndexStreamChannel(): EventChannel<IndexStreamEvent> {
  return eventChannel<IndexStreamEvent>((emit) => {
    api
      .indexPapersStream((event) => emit(event))
      .catch((error: unknown) =>
        emit({ event: "error", message: String(error) }),
      )
      .finally(() => emit(END));
    return () => {};
  });
}

// List the PDFs on disk (native Rust command) and, on open, fetch the bytes and
// build an object URL for inline rendering. The previous URL is revoked first so
// switching papers does not leak blob URLs.
export function* papersSaga() {
  yield* takeLatest(loadPapers, function* () {
    yield* put(papersLoading());
    try {
      const papers = yield* call(api.listPapers);
      yield* put(setPapers(papers));
      // Hydrate per-paper indicators from persisted outcomes so an already
      // indexed paper stays marked after a restart. Best-effort: a failure
      // here must not surface as a papers-load error.
      try {
        const { statuses } = yield* call(api.paperStatuses);
        yield* put(setPaperStatuses(statuses));
      } catch {
        // Ignore; indicators simply fall back to "not indexed".
      }
      // Hydrate the per-paper BM25 dot from the persisted lexical index.
      // Best-effort: a failure leaves every BM25 dot as "not indexed".
      try {
        const bm25 = yield* call(api.bm25Status);
        yield* put(setBm25Statuses(bm25.paper_ids));
      } catch {
        // Ignore; BM25 dots simply fall back to "not indexed".
      }
    } catch (error) {
      yield* put(papersFailed(String(error)));
    }
  });

  yield* takeLatest(openPaper, function* (action) {
    const paperId = action.payload[0];
    const previous = (yield* select((state) => state.papers.pdfUrl)) as
      | string
      | null;
    if (previous) {
      URL.revokeObjectURL(previous);
    }
    yield* put(pdfLoading(paperId));
    try {
      const pdf = yield* call(api.readPdf, paperId);
      const url = yield* call(pdfBytesToObjectUrl, pdf);
      yield* put(setPdfUrl(paperId, url));
    } catch (error) {
      yield* put(pdfFailed(String(error)));
    }
  });

  // Ignore duplicate requests until the active stream closes. Unlike
  // `takeLatest`, this never leaves a cancelled worker's native command running
  // concurrently with a replacement run.
  yield* takeLeading(runPaperIndex, function* () {
    yield* put(paperIndexStarted(0));
    const channel = createIndexStreamChannel();
    try {
      while (true) {
        const event = yield* take(channel);
        if (event.event === "start") {
          yield* put(paperIndexStarted(event.total));
        } else if (event.event === "started") {
          yield* put(paperIndexPaperStarted(event.paperId, event.filename));
        } else if (event.event === "progress") {
          yield* put(
            paperIndexProgress(
              event.completed,
              event.total,
              event.paperId,
              event.filename,
              event.status,
              event.error,
            ),
          );
        } else if (event.event === "done") {
          yield* put(
            paperIndexSucceeded(
              event.completed,
              event.total,
              event.indexed,
              event.empty,
              event.failed,
              event.skipped,
            ),
          );
          // The BM25 index is rebuilt at the end of the run; refresh the
          // per-paper BM25 dots to reflect the new coverage. Best-effort.
          try {
            const bm25 = yield* call(api.bm25Status);
            yield* put(setBm25Statuses(bm25.paper_ids));
          } catch {
            // Ignore; the dots keep their previous coverage.
          }
        } else if (event.event === "error") {
          yield* put(paperIndexFailed(event.message));
        }
      }
    } catch (error) {
      yield* put(paperIndexFailed(String(error)));
    }
  });

  // Reset every paper index, then reload papers so all indicators re-hydrate
  // from the now-empty persisted state. `takeLeading` ignores duplicate clicks
  // while a reset is in flight.
  yield* takeLeading(resetIndices, function* () {
    try {
      yield* call(api.resetAllIndices);
      yield* put(resetIndicesSucceeded());
      yield* put(loadPapers());
    } catch (error) {
      yield* put(resetIndicesFailed(String(error)));
    }
  });
}
