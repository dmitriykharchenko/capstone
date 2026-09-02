import type { PaperInfo } from "../../lib/api";

export interface PapersState {
  items: PaperInfo[];
  loading: boolean;
  error: string | null;
  selectedId: string | null;
  pdfUrl: string | null;
  pdfLoading: boolean;
  indexing: boolean;
  indexCompleted: number;
  indexTotal: number;
  indexCurrentFile: string | null;
  indexCurrentStatus: string | null;
  indexedCount: number | null;
  indexEmptyCount: number;
  indexFailedCount: number;
  indexError: string | null;
}

export const initialPapersState: PapersState = {
  items: [],
  loading: false,
  error: null,
  selectedId: null,
  pdfUrl: null,
  pdfLoading: false,
  indexing: false,
  indexCompleted: 0,
  indexTotal: 0,
  indexCurrentFile: null,
  indexCurrentStatus: null,
  indexedCount: null,
  indexEmptyCount: 0,
  indexFailedCount: 0,
  indexError: null,
};

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
    indexError: null,
  };
}

export function recordPaperIndexProgress(
  state: PapersState,
  completed: number,
  total: number,
  filename: string,
  status: string,
): PapersState {
  return {
    ...state,
    indexCompleted: completed,
    indexTotal: total,
    indexCurrentFile: filename,
    indexCurrentStatus: status,
  };
}

export function completePaperIndex(
  state: PapersState,
  completed: number,
  total: number,
  indexed: number,
  empty: number,
  failed: number,
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
