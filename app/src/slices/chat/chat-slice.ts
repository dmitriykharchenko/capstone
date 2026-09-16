import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type {
  ChatCoverageStatus,
  ChatMessage,
  ChatSource,
  ChatSubquery,
} from "../../lib/api";
import { initialChatState, type ChatState, type ChatTurnProgress } from "./chat-types";

// User asked to send a message; the saga performs the invoke and dispatches the
// outcome actions below.
export const sendMessage = createAction<[text: string]>("chat/sendMessage");
export const selectSession = createAction<[sessionId: string]>("chat/selectSession");
export const createSession = createAction("chat/createSession");
export const setSessionId = createAction<[sessionId: string]>("chat/setSessionId");
export const loadSession = createAction<[sessionId: string]>("chat/loadSession");
export const chatStarted = createAction("chat/started");
export const chatPlanReceived = createAction<[subqueries: ChatSubquery[]]>("chat/planReceived");
export const chatCoverageReceived = createAction<
  [status: ChatCoverageStatus, total: number, subqueries: ChatSubquery[]]
>("chat/coverageReceived");
export const chatSubqueryStarted = createAction<[subquery: ChatSubquery, total?: number]>(
  "chat/subqueryStarted",
);
export const chatSubanswerReceived = createAction<
  [
    id: string,
    index: number,
    query: string,
    answer: string,
    sources: ChatSource[],
    total?: number,
  ]
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

const emptyTurn = (): ChatTurnProgress => ({
  subqueries: [],
  initialCount: 0,
  total: 0,
  coverageStatus: null,
  finalAnswer: "",
});

const validSubquery = (subquery: ChatSubquery) =>
  Number.isInteger(subquery.index) &&
  subquery.index > 0 &&
  subquery.index <= 7 &&
  subquery.id === `subquery-${subquery.index}` &&
  Boolean(subquery.query.trim());

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
    activeTurn: emptyTurn(),
    error: null,
  }))
  .with(chatPlanReceived, (state: ChatState, action: StoreAction<[ChatSubquery[]]>) => {
    if (!state.pending) return state;
    const current = state.activeTurn ?? emptyTurn();
    const planned = [...action.payload[0]].sort((left, right) => left.index - right.index);
    if (
      current.subqueries.length ||
      planned.length < 1 ||
      planned.length > 5 ||
      planned.some(
        (subquery, position) =>
          !validSubquery(subquery) ||
          subquery.index !== position + 1 ||
          planned.some((other, index) => index !== position && other.id === subquery.id),
      )
    ) return state;
    return {
      ...state,
      activeTurn: {
        subqueries: planned
          .map((subquery) => ({
            ...subquery,
            kind: "initial" as const,
            status: "pending" as const,
            answer: null,
            sources: [],
          })),
        initialCount: planned.length,
        total: planned.length,
        coverageStatus: null,
        finalAnswer: current.finalAnswer,
      },
    };
  })
  .with(
    chatCoverageReceived,
    (
      state: ChatState,
      action: StoreAction<[ChatCoverageStatus, number, ChatSubquery[]]>,
    ) => {
    if (!state.pending) return state;
    const [status, total, followups] = action.payload;
    const current = state.activeTurn ?? emptyTurn();
    const initialComplete =
      current.subqueries.length === current.initialCount &&
      current.subqueries.every((item) => item.status === "complete");
    if (!initialComplete || !Number.isInteger(total) || total < current.initialCount || total > 7) {
      return state;
    }
    if (status === "reviewing" || status === "complete") {
      if (
        followups.length ||
        total !== current.initialCount ||
        (status === "complete" && current.coverageStatus !== "reviewing")
      ) return state;
      return { ...state, activeTurn: { ...current, coverageStatus: status } };
    }
    if (
      status !== "followup" ||
      current.coverageStatus !== "reviewing" ||
      followups.length < 1 ||
      followups.length > 2 ||
      total !== current.initialCount + followups.length ||
      followups.some(
        (subquery, offset) =>
          !validSubquery(subquery) ||
          subquery.index !== current.initialCount + offset + 1 ||
          [...current.subqueries, ...followups.slice(0, offset)].some(
            (prior) => prior.query.trim().toLowerCase() === subquery.query.trim().toLowerCase(),
          ),
      )
    ) return state;
    return {
      ...state,
      activeTurn: {
        ...current,
        coverageStatus: status,
        total,
        subqueries: [
          ...current.subqueries,
          ...followups.map((subquery) => ({
            ...subquery,
            kind: "followup" as const,
            status: "pending" as const,
            answer: null,
            sources: [],
          })),
        ],
      },
    };
    },
  )
  .with(
    chatSubqueryStarted,
    (state: ChatState, action: StoreAction<[ChatSubquery, number?]>) => {
    if (!state.pending) return state;
    const subquery = action.payload[0];
    const current = state.activeTurn ?? emptyTurn();
    const total = action.payload[1] ?? Math.max(current.total, subquery.index);
    if (!validSubquery(subquery) || total < subquery.index || total > 7) return state;
    const found = current.subqueries.some((item) => item.id === subquery.id);
    if (
      (found && current.subqueries.some((item) => item.id === subquery.id &&
        (item.index !== subquery.index || item.query !== subquery.query || item.status === "complete"))) ||
      (!found && subquery.index !== current.subqueries.length + 1)
    ) return state;
    const subqueries = (found
      ? current.subqueries.map((item) =>
          item.id === subquery.id ? { ...item, ...subquery, status: "active" as const } : item,
        )
      : [
          ...current.subqueries,
          {
            ...subquery,
            kind: subquery.index > current.initialCount ? "followup" as const : "initial" as const,
            status: "active" as const,
            answer: null,
            sources: [],
          },
        ]
    ).sort((left, right) => left.index - right.index);
    return { ...state, activeTurn: { ...current, total: Math.max(current.total, total), subqueries } };
    },
  )
  .with(
    chatSubanswerReceived,
    (
      state: ChatState,
      action: StoreAction<[
        string,
        number,
        string,
        string,
        ChatSource[],
        number?,
      ]>,
    ) => {
      if (!state.pending) return state;
      const [id, index, query, answer, sources, eventTotal] = action.payload;
      const current = state.activeTurn ?? emptyTurn();
      const total = eventTotal ?? current.total;
      if (
        id !== `subquery-${index}` ||
        !answer.trim() ||
        total !== current.total ||
        !current.subqueries.some(
          (item) => item.id === id && item.index === index && item.query === query && item.status !== "complete",
        )
      ) return state;
      const prior = current.subqueries.find((item) => item.id === id)!;
      const completed = { ...prior, answer, sources, status: "complete" as const };
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
    const current = state.activeTurn ?? emptyTurn();
    const hopsComplete =
      current.subqueries.length === current.total &&
      current.subqueries.every((item) => item.status === "complete");
    if (current.subqueries.length && (!hopsComplete || current.coverageStatus === "reviewing")) {
      return state;
    }
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
