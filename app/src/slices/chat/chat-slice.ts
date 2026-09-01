import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { ChatMessage } from "../../lib/api";
import { initialChatState, type ChatState } from "./chat-types";

// User asked to send a message; the saga performs the invoke and dispatches the
// outcome actions below.
export const sendMessage = createAction<[text: string]>("chat/sendMessage");
export const setSessionId = createAction<[sessionId: string]>("chat/setSessionId");
export const loadSession = createAction<[sessionId: string]>("chat/loadSession");
export const chatStarted = createAction("chat/started");
export const appendMessage = createAction<[message: ChatMessage]>("chat/appendMessage");
export const setMessages = createAction<[messages: ChatMessage[]]>("chat/setMessages");
export const chatSucceeded = createAction<[reply: string, chatMode: string]>(
  "chat/succeeded",
);
export const chatFailed = createAction<[error: string]>("chat/failed");

export const chatReducer: StoreReducerFunction<ChatState> = createReducer<ChatState>(
  initialChatState,
)
  .with(setSessionId, (state: ChatState, action: StoreAction<[string]>) => ({
    ...state,
    sessionId: action.payload[0],
    messages: [],
    error: null,
  }))
  .with(setMessages, (state: ChatState, action: StoreAction<[ChatMessage[]]>) => ({
    ...state,
    messages: action.payload[0],
  }))
  .with(appendMessage, (state: ChatState, action: StoreAction<[ChatMessage]>) => ({
    ...state,
    messages: [...state.messages, action.payload[0]],
  }))
  .with(chatStarted, (state: ChatState) => ({
    ...state,
    pending: true,
    error: null,
  }))
  .with(
    chatSucceeded,
    (state: ChatState, action: StoreAction<[string, string]>) => ({
      ...state,
      pending: false,
      chatMode: action.payload[1],
      messages: [
        ...state.messages,
        { role: "assistant", content: action.payload[0] },
      ],
    }),
  )
  .with(chatFailed, (state: ChatState, action: StoreAction<[string]>) => ({
    ...state,
    pending: false,
    error: action.payload[0],
  }));
