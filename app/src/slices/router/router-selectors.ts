import { store } from "../../store";

export const selectActiveRoute = store.createSelector(
  (state) => state.router.activeRoute,
);
export const selectRouteInitialized = store.createSelector(
  (state) => state.router.initialized,
);