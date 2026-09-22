import type { PaperInfo, PaperStatusInfo } from "../../lib/api";

// Per-paper indexing state tracked while a run streams. A paper with no entry
// is "not indexed"; the component treats a missing entry as "queued" while a
// run is active. Terminal outcomes ("indexed"/"empty"/"failed") persist after
// the run so the indicators keep reflecting the last result. "skipped" marks a
// paper left untouched this run because it was already indexed.
export type PaperIndexStatus =
  | "queued"
  | "indexing"
  | "indexed"
  | "empty"
  | "failed"
  | "skipped";

export interface PapersState {
  items: PaperInfo[];
  loading: boolean;
  error: string | null;
  selectedId: string | null;
  pdfUrl: string | null;
  pdfLoading: boolean;
  pdfError: string | null;
  availableCommands: string[];
  statusesKnown: boolean;
  bm25Known: boolean;
  indexing: boolean;
  indexCompleted: number;
  indexTotal: number;
  indexCurrentFile: string | null;
  indexCurrentStatus: string | null;
  indexedCount: number | null;
  indexEmptyCount: number;
  indexFailedCount: number;
  indexSkippedCount: number;
  indexError: string | null;
  indexStatuses: Record<string, PaperIndexStatus>;
  // Paper ids covered by the persisted BM25 index, refreshed on load and after
  // each indexing run so the per-paper BM25 dot reflects the lexical index.
  bm25PaperIds: string[];
  // True while a reset-all-indices run is in flight.
  resetting: boolean;
}

export const initialPapersState: PapersState = {
  items: [],
  loading: false,
  error: null,
  selectedId: null,
  pdfUrl: null,
  pdfLoading: false,
  pdfError: null,
  availableCommands: [],
  statusesKnown: false,
  bm25Known: false,
  indexing: false,
  indexCompleted: 0,
  indexTotal: 0,
  indexCurrentFile: null,
  indexCurrentStatus: null,
  indexedCount: null,
  indexEmptyCount: 0,
  indexFailedCount: 0,
  indexSkippedCount: 0,
  indexError: null,
  indexStatuses: {},
  bm25PaperIds: [],
  resetting: false,
};

// Terminal outcomes worth restoring on startup. A run only persists these, but
// hydration guards against unexpected values so the map stays well-typed.
const HYDRATABLE_STATUSES: readonly PaperIndexStatus[] = [
  "indexed",
  "empty",
  "failed",
];

// Merge persisted per-paper outcomes into the indicator map so a restart shows
// the last known result instead of resetting every paper to "not indexed".
export function hydratePaperStatuses(
  state: PapersState,
  statuses: PaperStatusInfo[],
): PapersState {
  const indexStatuses: Record<string, PaperIndexStatus> = {
    ...state.indexStatuses,
  };
  for (const entry of statuses) {
    if ((HYDRATABLE_STATUSES as string[]).includes(entry.status)) {
      indexStatuses[entry.paper_id] = entry.status as PaperIndexStatus;
    }
  }
  return { ...state, indexStatuses, statusesKnown: true };
}

export function beginPaperIndex(state: PapersState, total: number): PapersState {
  return {
    ...state,
    indexing: true,
    indexCompleted: 0,
    indexTotal: total,
    indexCurrentFile: null,
    indexCurrentStatus: null,
    indexedCount: null,
    indexEmptyCount: 0,
    indexFailedCount: 0,
    indexSkippedCount: 0,
    indexError: null,
    indexStatuses: {},
  };
}

// Mark the paper the backend is now processing as "indexing"; the remaining
// papers stay queued until their own start/outcome events arrive.
export function markPaperIndexing(
  state: PapersState,
  paperId: string,
  filename: string,
): PapersState {
  return {
    ...state,
    indexCurrentFile: filename,
    indexCurrentStatus: "indexing",
    indexStatuses: { ...state.indexStatuses, [paperId]: "indexing" },
  };
}

export function recordPaperIndexProgress(
  state: PapersState,
  completed: number,
  total: number,
  paperId: string,
  filename: string,
  status: string,
): PapersState {
  return {
    ...state,
    indexCompleted: completed,
    indexTotal: total,
    indexCurrentFile: filename,
    indexCurrentStatus: status,
    indexStatuses: {
      ...state.indexStatuses,
      [paperId]: status as PaperIndexStatus,
    },
  };
}

export function completePaperIndex(
  state: PapersState,
  completed: number,
  total: number,
  indexed: number,
  empty: number,
  failed: number,
  skipped: number,
): PapersState {
  return {
    ...state,
    indexing: false,
    indexCompleted: completed,
    indexTotal: total,
    indexCurrentFile: null,
    indexCurrentStatus: null,
    indexedCount: indexed,
    indexEmptyCount: empty,
    indexFailedCount: failed,
    indexSkippedCount: skipped,
  };
}

export function rejectPaperIndex(
  state: PapersState,
  error: string,
): PapersState {
  return {
    ...state,
    indexing: false,
    indexError: error,
  };
}

// Replace the set of BM25-covered paper ids used to render the per-paper BM25
// dot; called after load and after each indexing run.
export function setBm25PaperIds(
  state: PapersState,
  paperIds: string[],
): PapersState {
  return { ...state, bm25PaperIds: paperIds, bm25Known: true };
}

export function beginReset(state: PapersState): PapersState {
  return { ...state, resetting: true, indexError: null };
}

// Clear every derived indexing indicator once the backend has wiped all
// indexes, so the UI immediately reflects an empty index; a follow-up load
// re-hydrates from the (now empty) persisted status and BM25 index.
export function completeReset(state: PapersState): PapersState {
  return {
    ...state,
    resetting: false,
    indexStatuses: {},
    bm25PaperIds: [],
    indexedCount: null,
    indexEmptyCount: 0,
    indexFailedCount: 0,
    indexSkippedCount: 0,
  };
}

export function rejectReset(state: PapersState, error: string): PapersState {
  return { ...state, resetting: false, indexError: error };
}
