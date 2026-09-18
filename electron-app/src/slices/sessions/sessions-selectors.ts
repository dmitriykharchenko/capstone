import { store } from "../../store";

export const selectSessions = store.createSelector(
  (state) => state.sessions.items,
);
export const selectSessionsLoading = store.createSelector(
  (state) => state.sessions.loading,
);
export const selectSessionsError = store.createSelector(
  (state) => state.sessions.error,
);
