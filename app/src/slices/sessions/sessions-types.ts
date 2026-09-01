import type { SessionInfo } from "../../lib/api";

export interface SessionsState {
  items: SessionInfo[];
  loading: boolean;
  error: string | null;
}

export const initialSessionsState: SessionsState = {
  items: [],
  loading: false,
  error: null,
};
