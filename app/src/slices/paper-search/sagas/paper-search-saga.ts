import { call, put, take, takeLatest } from "typed-redux-saga";
import { eventChannel, END, type EventChannel } from "redux-saga";
import { api, type IndexStreamEvent } from "../../../lib/api";
import {
  runPaperSearch,
  paperSearchFailed,
  paperSearchStarted,
  paperSearchSucceeded,
  runPaperIndex,
  paperIndexStarted,
  paperIndexProgress,
  paperIndexSucceeded,
  paperIndexFailed,
} from "../paper-search-slice";

// Bridge the Tauri streaming Channel into a redux-saga eventChannel: each
// backend `IndexStreamEvent` is forwarded as an emission, and the channel is
// closed (END) when the invoke promise settles or rejects.
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

// Rank indexed papers by semantic similarity via the Chroma-backed `documents`
// collection (Python CLI). When chromadb is unavailable the CLI returns
// enabled=false, which the reducer surfaces so the UI can explain search is off.
export function* paperSearchSaga() {
  yield* takeLatest(runPaperSearch, function* (action) {
    const query = action.payload[0];
    if (!query.trim()) {
      return;
    }
    yield* put(paperSearchStarted(query));
    try {
      const result = yield* call(api.searchPapers, query, 8);
      yield* put(
        paperSearchSucceeded(
          result.matches,
          result.enabled,
          result.reason ?? null,
        ),
      );
    } catch (error) {
      yield* put(paperSearchFailed(String(error)));
    }
  });

  // Index (or re-index) the papers directory into Chroma. This embeds every
  // chunk of every PDF, so it can take a while for large corpora; the streaming
  // channel drives a determinate progress bar until the terminal event arrives.
  yield* takeLatest(runPaperIndex, function* () {
    yield* put(paperIndexStarted(0));
    const channel = createIndexStreamChannel();
    try {
      while (true) {
        const event = yield* take(channel);
        if (event.event === "start") {
          yield* put(paperIndexStarted(event.total));
        } else if (event.event === "progress") {
          yield* put(
            paperIndexProgress(event.completed, event.total, event.filename),
          );
        } else if (event.event === "done") {
          yield* put(
            paperIndexSucceeded(event.indexed, event.empty, event.failed),
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
