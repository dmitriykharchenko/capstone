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
  closePaper,
  bm25Unavailable,
  setPaperCapabilities,
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
    const request = api.indexPapersStream((event) => emit(event));
    request
      .catch((error: unknown) =>
        emit({ event: "error", message: String(error) }),
      )
      .finally(() => emit(END));
    return () => request.cancel?.();
  });
}

// List the PDFs on disk (native Rust command) and, on open, fetch the bytes and
// build an object URL for inline rendering. The previous URL is revoked first so
// switching papers does not leak blob URLs.
export function* papersSaga() {
  yield* takeLatest(loadPapers, function* () {
    yield* put(papersLoading());
    try {
      const health = yield* call(api.health);
      const commands = health.commands.filter((command) => command.available).map((command) => command.command);
      yield* put(setPaperCapabilities(commands));
      if (!commands.includes("list_papers")) {
        yield* put(papersFailed("PDF browsing is unavailable. Check the trusted CAPSTONE_PAPERS_DIR and application resource/data roots, then restart the app."));
        return;
      }
      const papers = yield* call(api.listPapers);
      yield* put(setPapers(papers));
      // Absent or failed status reads mean UNKNOWN, never "not indexed".
      try {
        if (commands.includes("paper_statuses")) {
          const { statuses } = yield* call(api.paperStatuses);
          yield* put(setPaperStatuses(statuses));
        }
      } catch {
        // Keep statusesKnown false without preventing PDF browsing.
      }
      try {
        if (commands.includes("bm25_status")) {
          const bm25 = yield* call(api.bm25Status);
          yield* put(setBm25Statuses(bm25.paper_ids));
        }
      } catch {
        // Keep bm25Known false.
      }
    } catch (error) {
      yield* put(papersFailed(String(error)));
    }
  });

  yield* takeLatest([openPaper, closePaper], function* (action) {
    if (action.type === closePaper.type || !action.payload) return;
    const paperId = action.payload[0];
    let url: string | undefined;
    yield* put(pdfLoading(paperId));
    try {
      const pdf = yield* call(api.readPdf, paperId);
      url = yield* call(pdfBytesToObjectUrl, pdf);
      yield* put(setPdfUrl(paperId, url));
      // Retain ownership until switch, close, unmount or saga shutdown.
      yield* take(closePaper);
    } catch {
      yield* put(pdfFailed("Cannot open this PDF. It may be missing, empty, corrupt, unsafe, unsupported, or larger than 64 MiB. Check that the file is readable and the native backend is available, then retry."));
    } finally {
      if (url) URL.revokeObjectURL(url);
    }
  });

  // Ignore duplicate requests until the active stream closes. Unlike
  // `takeLatest`, this never leaves a cancelled worker's native command running
  // concurrently with a replacement run.
  yield* takeLeading(runPaperIndex, function* () {
    const commands = yield* select((state) => state.papers.availableCommands);
    if (!commands.includes("index_papers_stream")) {
      yield* put(paperIndexFailed("Paper indexing is unavailable in this build."));
      return;
    }
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
            yield* put(bm25Unavailable());
          }
        } else if (event.event === "error") {
          yield* put(paperIndexFailed(event.message));
        }
      }
    } catch (error) {
      yield* put(paperIndexFailed(String(error)));
    } finally {
      channel.close();
    }
  });

  // Reset every paper index, then reload papers so all indicators re-hydrate
  // from the now-empty persisted state. `takeLeading` ignores duplicate clicks
  // while a reset is in flight.
  yield* takeLeading(resetIndices, function* () {
    const commands = yield* select((state) => state.papers.availableCommands);
    if (!commands.includes("reset_all_indices")) {
      yield* put(resetIndicesFailed("Reset indices is unavailable in this build."));
      return;
    }
    try {
      yield* call(api.resetAllIndices);
      yield* put(resetIndicesSucceeded());
      yield* put(loadPapers());
    } catch (error) {
      yield* put(resetIndicesFailed(String(error)));
    }
  });
}
