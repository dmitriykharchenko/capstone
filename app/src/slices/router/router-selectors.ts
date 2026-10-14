import { store } from "../../store";

export const selectActiveRoute = store.createSelector(
  (state) => state.router.activeRoute,
);
