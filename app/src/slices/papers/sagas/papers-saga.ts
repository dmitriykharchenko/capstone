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
  paperIndexProgress,
  paperIndexStarted,
  paperIndexSucceeded,
  papersFailed,
  papersLoading,
  pdfFailed,
  pdfLoading,
  runPaperIndex,
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
        } else if (event.event === "progress") {
          yield* put(
            paperIndexProgress(
              event.completed,
              event.total,
              event.filename,
              event.status,
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
            ),
          );
        } else if (event.event === "error") {
          yield* put(paperIndexFailed(event.message));
        }
      }
    } catch (error) {
      yield* put(paperIndexFailed(String(error)));
    }
  });
}
