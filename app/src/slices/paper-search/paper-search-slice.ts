import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { PaperMatch } from "../../lib/api";
import {
  initialPaperSearchState,
  type PaperSearchState,
} from "./paper-search-types";

export const runPaperSearch = createAction<[query: string]>("paperSearch/run");
export const paperSearchStarted = createAction<[query: string]>(
  "paperSearch/started",
);
export const paperSearchSucceeded = createAction<
  [matches: PaperMatch[], enabled: boolean, reason: string | null]
>("paperSearch/succeeded");
export const paperSearchFailed = createAction<[error: string]>(
  "paperSearch/failed",
);
export const clearPaperSearch = createAction("paperSearch/clear");

export const runPaperIndex = createAction("paperSearch/runIndex");
export const paperIndexStarted = createAction("paperSearch/indexStarted");
export const paperIndexSucceeded = createAction<
  [indexed: number, enabled: boolean, reason: string | null]
>("paperSearch/indexSucceeded");
export const paperIndexFailed = createAction<[error: string]>(
  "paperSearch/indexFailed",
);

export const paperSearchReducer: StoreReducerFunction<PaperSearchState> =
  createReducer<PaperSearchState>(initialPaperSearchState)
    .with(
      paperSearchStarted,
      (state: PaperSearchState, action: StoreAction<[string]>) => ({
        ...state,
        query: action.payload[0],
        searching: true,
        error: null,
      }),
    )
    .with(
      paperSearchSucceeded,
      (
        state: PaperSearchState,
        action: StoreAction<[PaperMatch[], boolean, string | null]>,
      ) => ({
        ...state,
        searching: false,
        hasSearched: true,
        matches: action.payload[0],
        enabled: action.payload[1],
        reason: action.payload[2],
      }),
    )
    .with(
      paperSearchFailed,
      (state: PaperSearchState, action: StoreAction<[string]>) => ({
        ...state,
        searching: false,
        hasSearched: true,
        error: action.payload[0],
      }),
    )
    .with(clearPaperSearch, (state: PaperSearchState) => ({
      ...initialPaperSearchState,
      indexedCount: state.indexedCount,
    }))
    .with(paperIndexStarted, (state: PaperSearchState) => ({
      ...state,
      indexing: true,
      indexError: null,
    }))
    .with(
      paperIndexSucceeded,
      (
        state: PaperSearchState,
        action: StoreAction<[number, boolean, string | null]>,
      ) => ({
        ...state,
        indexing: false,
        indexedCount: action.payload[0],
        enabled: action.payload[1],
        indexError: action.payload[2],
      }),
    )
    .with(
      paperIndexFailed,
      (state: PaperSearchState, action: StoreAction<[string]>) => ({
        ...state,
        indexing: false,
        indexError: action.payload[0],
      }),
    );
