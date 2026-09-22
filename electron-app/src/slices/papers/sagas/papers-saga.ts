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
  type IndexStreamEvent,
} from "../../../lib/api";
import {
  bm25Unavailable,
  setPaperCapabilities,
  setIndexReadiness,
  loadPapers,
  paperIndexFailed,
  paperIndexPaperStarted,
  paperIndexProgress,
  paperIndexStarted,
  paperIndexSucceeded,
  papersFailed,
  papersLoading,
  resetIndices,
  resetIndicesFailed,
  resetIndicesSucceeded,
  runPaperIndex,
  setBm25Statuses,
  setPaperStatuses,
  setPapers,
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

// Library/index data only. PDF read and render work belongs to the mounted reader.
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
          const { statuses, readiness } = yield* call(api.paperStatuses);
          yield* put(setIndexReadiness(readiness));
          if (readiness === "ready") yield* put(setPaperStatuses(statuses));
        }
      } catch {
        // Keep statusesKnown false without preventing PDF browsing.
      }
      try {
        if (commands.includes("bm25_status")) {
          const bm25 = yield* call(api.bm25Status);
          if (bm25.readiness === "ready") yield* put(setBm25Statuses(bm25.paper_ids));
          else yield* put(setIndexReadiness(bm25.readiness));
        }
      } catch {
        // Keep bm25Known false.
      }
    } catch (error) {
      yield* put(papersFailed(String(error)));
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
          break;
        } else if (event.event === "error") {
          yield* put(paperIndexFailed(event.message));
          break;
        }
      }
    } catch (error) {
      yield* put(paperIndexFailed(String(error)));
    } finally {
      channel.close();
      // A cancelled/lost terminal does not imply rollback. Refresh authoritative
      // status after every outcome, never leave a failed run's green attempt dots.
      try {
        const { readiness, statuses } = yield* call(api.paperStatuses);
        yield* put(setIndexReadiness(readiness));
        if (readiness === "ready") yield* put(setPaperStatuses(statuses));
        const bm25 = yield* call(api.bm25Status);
        if (bm25.readiness === "ready") yield* put(setBm25Statuses(bm25.paper_ids));
        else yield* put(setIndexReadiness(bm25.readiness));
      } catch {
        yield* put(setIndexReadiness("unavailable"));
        yield* put(bm25Unavailable());
      }
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
      yield* put(loadPapers());
    }
  });
}
