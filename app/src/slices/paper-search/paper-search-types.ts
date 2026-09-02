import type { PaperMatch } from "../../lib/api";

export interface PaperSearchState {
  query: string;
  matches: PaperMatch[];
  searching: boolean;
  enabled: boolean;
  reason: string | null;
  error: string | null;
  hasSearched: boolean;
  indexing: boolean;
  indexedCount: number | null;
  indexError: string | null;
  // Determinate progress for the streaming index run.
  indexCompleted: number;
  indexTotal: number;
  indexCurrentFile: string | null;
  indexFailedCount: number;
  indexEmptyCount: number;
}

export const initialPaperSearchState: PaperSearchState = {
  query: "",
  matches: [],
  searching: false,
  enabled: true,
  reason: null,
  error: null,
  hasSearched: false,
  indexing: false,
  indexedCount: null,
  indexError: null,
  indexCompleted: 0,
  indexTotal: 0,
  indexCurrentFile: null,
  indexFailedCount: 0,
  indexEmptyCount: 0,
};
