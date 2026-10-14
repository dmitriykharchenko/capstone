import { store } from "../../store";
import { hasActiveLibraryWork } from "./library-types";

export const selectLibrary = store.createSelector((state) => state.library.current);
export const selectLibraryStatus = store.createSelector(
  (state) => state.library.status,
);
export const selectLibraryChoosing = store.createSelector(
  (state) => state.library.choosing,
);
export const selectLibraryError = store.createSelector(
  (state) => state.library.error,
);
export const selectLibraryWorkActive = store.createSelector((state) =>
  hasActiveLibraryWork(state),
);