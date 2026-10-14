import { store } from "../../store";

export const selectSessions = store.createSelector(
  (state) => state.sessions.items,
);
