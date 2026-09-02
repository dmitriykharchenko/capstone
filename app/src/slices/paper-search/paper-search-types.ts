import type { PaperMatch } from "../../lib/api";

export interface PaperSearchState {
  query: string;
  matches: PaperMatch[];
  searching: boolean;
  enabled: boolean;
  reason: string | null;
  error: string | null;
  hasSearched: boolean;
}

export const initialPaperSearchState: PaperSearchState = {
  query: "",
  matches: [],
  searching: false,
  enabled: true,
  reason: null,
  error: null,
  hasSearched: false,
};
