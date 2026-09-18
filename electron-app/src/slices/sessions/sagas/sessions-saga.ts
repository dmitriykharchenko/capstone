import { call, put, takeLatest } from "typed-redux-saga";
import { api } from "../../../lib/api";
import {
  loadSessions,
  sessionsFailed,
  sessionsLoading,
  setSessions,
} from "../sessions-slice";

// Reload the session list on demand from the SQLite history via the CLI bridge.
export function* sessionsSaga() {
  yield* takeLatest(loadSessions, function* () {
    yield* put(sessionsLoading());
    try {
      const result = yield* call(api.listSessions);
      yield* put(setSessions(result.sessions));
    } catch (error) {
      yield* put(sessionsFailed(String(error)));
    }
  });
}
