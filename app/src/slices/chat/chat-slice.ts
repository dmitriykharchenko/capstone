import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { ChatMessage, ChatSource, ChatSubquery } from "../../lib/api";
import { initialChatState, type ChatState } from "./chat-types";

// User asked to send a message; the saga performs the invoke and dispatches the
// outcome actions below.
export const sendMessage = createAction<[text: string]>("chat/sendMessage");
export const selectSession = createAction<[sessionId: string]>("chat/selectSession");
export const createSession = createAction("chat/createSession");
export const setSessionId = createAction<[sessionId: string]>("chat/setSessionId");
export const loadSession = createAction<[sessionId: string]>("chat/loadSession");
export const chatStarted = createAction("chat/started");
export const chatPlanReceived = createAction<[subqueries: ChatSubquery[]]>("chat/planReceived");
export const chatSubqueryStarted = createAction<[subquery: ChatSubquery]>(
  "chat/subqueryStarted",
);
export const chatSubanswerReceived = createAction<
  [id: string, index: number, query: string, answer: string, sources: ChatSource[]]
>("chat/subanswerReceived");
export const appendMessage = createAction<[message: ChatMessage]>("chat/appendMessage");
export const setMessages = createAction<[messages: ChatMessage[]]>("chat/setMessages");
// One streamed token delta; the first delta of a turn opens the assistant
// message, later deltas extend it.
export const chatChunk = createAction<[delta: string]>("chat/chunk");
export const chatSucceeded = createAction<
  [reply: string, chatMode: string, sources: ChatSource[]]
>("chat/succeeded");
export const chatFailed = createAction<[error: string]>("chat/failed");
export const chatHydrationFailed = createAction<[error: string]>("chat/hydrationFailed");

export const chatReducer: StoreReducerFunction<ChatState> = createReducer<ChatState>(
  initialChatState,
)
  .with(selectSession, (state: ChatState) => ({
    ...state,
    pending: false,
    streaming: false,
    activeTurn: null,
    error: null,
  }))
  .with(createSession, (state: ChatState) => ({
    ...state,
    pending: false,
    streaming: false,
    activeTurn: null,
    error: null,
  }))
  .with(setSessionId, (state: ChatState, action: StoreAction<[string]>) => ({
    ...state,
    sessionId: action.payload[0],
    messages: [],
    pending: false,
    streaming: false,
    activeTurn: null,
    error: null,
  }))
  .with(setMessages, (state: ChatState, action: StoreAction<[ChatMessage[]]>) => ({
    ...state,
    messages: action.payload[0],
    pending: false,
    streaming: false,
    activeTurn: null,
  }))
  .with(appendMessage, (state: ChatState, action: StoreAction<[ChatMessage]>) => ({
    ...state,
    messages: [...state.messages, action.payload[0]],
  }))
  .with(chatStarted, (state: ChatState) => ({
    ...state,
    pending: true,
    streaming: false,
    activeTurn: { subqueries: [], finalAnswer: "" },
    error: null,
  }))
  .with(chatPlanReceived, (state: ChatState, action: StoreAction<[ChatSubquery[]]>) => {
    if (!state.pending) return state;
    return {
      ...state,
      activeTurn: {
        subqueries: action.payload[0]
          .map((subquery) => ({
            ...subquery,
            status: "pending" as const,
            answer: null,
            sources: [],
          }))
          .sort((left, right) => left.index - right.index),
        finalAnswer: state.activeTurn?.finalAnswer ?? "",
      },
    };
  })
  .with(chatSubqueryStarted, (state: ChatState, action: StoreAction<[ChatSubquery]>) => {
    if (!state.pending) return state;
    const subquery = action.payload[0];
    const current = state.activeTurn ?? { subqueries: [], finalAnswer: "" };
    const found = current.subqueries.some((item) => item.id === subquery.id);
    const subqueries = (found
      ? current.subqueries.map((item) =>
          item.id === subquery.id ? { ...item, ...subquery, status: "active" as const } : item,
        )
      : [
          ...current.subqueries,
          { ...subquery, status: "active" as const, answer: null, sources: [] },
        ]
    ).sort((left, right) => left.index - right.index);
    return { ...state, activeTurn: { ...current, subqueries } };
  })
  .with(
    chatSubanswerReceived,
    (state: ChatState, action: StoreAction<[string, number, string, string, ChatSource[]]>) => {
      if (!state.pending) return state;
      const [id, index, query, answer, sources] = action.payload;
      const current = state.activeTurn ?? { subqueries: [], finalAnswer: "" };
      const completed = { id, index, query, answer, sources, status: "complete" as const };
      const found = current.subqueries.some((item) => item.id === id);
      const subqueries = (found
        ? current.subqueries.map((item) => (item.id === id ? completed : item))
        : [...current.subqueries, completed]
      ).sort((left, right) => left.index - right.index);
      return { ...state, activeTurn: { ...current, subqueries } };
    },
  )
  .with(chatChunk, (state: ChatState, action: StoreAction<[string]>) => {
    if (!state.pending) return state;
    const delta = action.payload[0];
    const current = state.activeTurn ?? { subqueries: [], finalAnswer: "" };
    return {
      ...state,
      streaming: true,
      activeTurn: { ...current, finalAnswer: current.finalAnswer + delta },
    };
  })
  .with(
    chatSucceeded,
    (state: ChatState, action: StoreAction<[string, string, ChatSource[]]>) => {
      if (!state.pending) return state;
      const [reply, chatMode, sources] = action.payload;
      const messages = [...state.messages, { role: "assistant", content: reply, sources }];
      return {
        ...state,
        pending: false,
        streaming: false,
        activeTurn: null,
        chatMode,
        messages,
      };
    },
  )
  .with(chatFailed, (state: ChatState, action: StoreAction<[string]>) => {
    if (!state.pending) return state;
    const last = state.messages[state.messages.length - 1];
    const messages =
      state.streaming && last?.role === "assistant"
        ? state.messages.slice(0, -1)
        : state.messages;
    return {
      ...state,
      messages,
      pending: false,
      streaming: false,
      activeTurn: null,
      error: action.payload[0],
    };
  })
  .with(chatHydrationFailed, (state: ChatState, action: StoreAction<[string]>) => {
    const last = state.messages[state.messages.length - 1];
    const messages =
      state.streaming && last?.role === "assistant"
        ? state.messages.slice(0, -1)
        : state.messages;
    return {
      ...state,
      messages,
      pending: false,
      streaming: false,
      activeTurn: null,
      error: action.payload[0],
    };
  });
