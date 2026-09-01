import type { SearchMatch } from "../../lib/api";

export interface SearchState {
  query: string;
  matches: SearchMatch[];
  searching: boolean;
  enabled: boolean;
  reason: string | null;
  error: string | null;
  hasSearched: boolean;
}

export const initialSearchState: SearchState = {
  query: "",
  matches: [],
  searching: false,
  enabled: true,
  reason: null,
  error: null,
  hasSearched: false,
};
