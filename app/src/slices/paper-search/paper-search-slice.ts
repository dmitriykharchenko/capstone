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
export const paperIndexStarted = createAction<[total: number]>(
  "paperSearch/indexStarted",
);
export const paperIndexProgress = createAction<
  [completed: number, total: number, currentFile: string]
>("paperSearch/indexProgress");
export const paperIndexSucceeded = createAction<
  [indexed: number, empty: number, failed: number]
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
    .with(
      paperIndexStarted,
      (state: PaperSearchState, action: StoreAction<[number]>) => ({
        ...state,
        indexing: true,
        indexError: null,
        indexCompleted: 0,
        indexTotal: action.payload[0],
        indexCurrentFile: null,
        indexFailedCount: 0,
        indexEmptyCount: 0,
      }),
    )
    .with(
      paperIndexProgress,
      (
        state: PaperSearchState,
        action: StoreAction<[number, number, string]>,
      ) => ({
        ...state,
        indexCompleted: action.payload[0],
        indexTotal: action.payload[1],
        indexCurrentFile: action.payload[2],
      }),
    )
    .with(
      paperIndexSucceeded,
      (
        state: PaperSearchState,
        action: StoreAction<[number, number, number]>,
      ) => ({
        ...state,
        indexing: false,
        indexedCount: action.payload[0],
        indexEmptyCount: action.payload[1],
        indexFailedCount: action.payload[2],
        indexCurrentFile: null,
        enabled: true,
      }),
    )
    .with(
      paperIndexFailed,
      (state: PaperSearchState, action: StoreAction<[string]>) => ({
        ...state,
        indexing: false,
        indexCurrentFile: null,
        indexError: action.payload[0],
      }),
    );
