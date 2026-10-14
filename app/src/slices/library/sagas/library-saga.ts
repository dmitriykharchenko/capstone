import { call, put, select, takeLeading } from "typed-redux-saga";
import { api } from "../../../lib/api";
import {
  chooseLibrary,
  libraryChoiceFailed,
  libraryChoicePreserved,
  libraryLoaded,
  libraryLoadFailed,
  libraryLoading,
} from "../library-slice";
import { hasActiveLibraryWork } from "../library-types";

export interface LibraryServices {
  getCurrentLibrary: typeof api.getCurrentLibrary;
  chooseLibrary: typeof api.chooseLibrary;
  reloadApp(): void;
}

const browserLibraryServices: LibraryServices = {
  getCurrentLibrary: api.getCurrentLibrary,
  chooseLibrary: api.chooseLibrary,
  reloadApp: () => {
    window.history.replaceState(null, "", "/papers");
    window.location.reload();
  },
};

export function* initializeLibrary(
  services: LibraryServices = browserLibraryServices,
) {
  yield* put(libraryLoading());
  try {
    const library = yield* call(services.getCurrentLibrary);
    yield* put(libraryLoaded(library));
    return true;
  } catch (error) {
    yield* put(libraryLoadFailed(String(error)));
    return false;
  }
}

export function* librarySaga(
  services: LibraryServices = browserLibraryServices,
) {
  yield* takeLeading(chooseLibrary, function* () {
    const state = yield* select((current) => current);
    if (hasActiveLibraryWork(state)) {
      yield* put(
        libraryChoiceFailed(
          "Finish the active chat, index, search, PDF load, reset, or evaluation before changing folders.",
        ),
      );
      return;
    }
    try {
      const result = yield* call(services.chooseLibrary);
      if (result.changed) {
        yield* call(services.reloadApp);
        return;
      }
      yield* put(libraryChoicePreserved(result.library));
    } catch (error) {
      yield* put(libraryChoiceFailed(String(error)));
    }
  });
}