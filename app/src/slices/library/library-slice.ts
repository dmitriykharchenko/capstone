import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { LibrarySelection } from "../../lib/api";
import { initialLibraryState, type LibraryState } from "./library-types";

export const libraryLoading = createAction("library/loading");
export const libraryLoaded = createAction<[library: LibrarySelection | null]>(
  "library/loaded",
);
export const libraryLoadFailed = createAction<[error: string]>(
  "library/loadFailed",
);
export const chooseLibrary = createAction("library/choose");
export const libraryChoicePreserved = createAction<
  [library: LibrarySelection | null]
>("library/choicePreserved");
export const libraryChoiceFailed = createAction<[error: string]>(
  "library/choiceFailed",
);

export const libraryReducer: StoreReducerFunction<LibraryState> =
  createReducer<LibraryState>(initialLibraryState)
    .with(libraryLoading, (state: LibraryState) => ({
      ...state,
      status: "loading",
      error: null,
    }))
    .with(
      libraryLoaded,
      (state: LibraryState, action: StoreAction<[LibrarySelection | null]>) => ({
        ...state,
        current: action.payload[0],
        status: "ready",
        error: null,
      }),
    )
    .with(
      libraryLoadFailed,
      (state: LibraryState, action: StoreAction<[string]>) => ({
        ...state,
        status: "error",
        error: action.payload[0],
      }),
    )
    .with(chooseLibrary, (state: LibraryState) => ({
      ...state,
      choosing: true,
      error: null,
    }))
    .with(
      libraryChoicePreserved,
      (state: LibraryState, action: StoreAction<[LibrarySelection | null]>) => ({
        ...state,
        current: action.payload[0],
        status: "ready",
        choosing: false,
        error: null,
      }),
    )
    .with(
      libraryChoiceFailed,
      (state: LibraryState, action: StoreAction<[string]>) => ({
        ...state,
        choosing: false,
        error: action.payload[0],
      }),
    );