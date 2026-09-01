import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { SessionInfo } from "../../lib/api";
import { initialSessionsState, type SessionsState } from "./sessions-types";

export const loadSessions = createAction("sessions/load");
export const sessionsLoading = createAction("sessions/loading");
export const setSessions = createAction<[sessions: SessionInfo[]]>("sessions/set");
export const sessionsFailed = createAction<[error: string]>("sessions/failed");

export const sessionsReducer: StoreReducerFunction<SessionsState> =
  createReducer<SessionsState>(initialSessionsState)
    .with(sessionsLoading, (state: SessionsState) => ({
      ...state,
      loading: true,
      error: null,
    }))
    .with(setSessions, (state: SessionsState, action: StoreAction<[SessionInfo[]]>) => ({
      ...state,
      loading: false,
      items: action.payload[0],
    }))
    .with(sessionsFailed, (state: SessionsState, action: StoreAction<[string]>) => ({
      ...state,
      loading: false,
      error: action.payload[0],
    }));
