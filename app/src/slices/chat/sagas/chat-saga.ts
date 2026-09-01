import { call, put, select, takeLatest, takeEvery } from "typed-redux-saga";
import { api } from "../../../lib/api";
import {
  appendMessage,
  chatFailed,
  chatStarted,
  chatSucceeded,
  loadSession,
  sendMessage,
  setMessages,
} from "../chat-slice";

// Send one chat turn: optimistically append the user message, invoke the Rust
// command (which proxies the Python CLI), then record the reply or the error.
export function* chatSaga() {
  yield* takeLatest(sendMessage, function* (action) {
    const text = action.payload[0];
    if (!text.trim()) {
      return;
    }
    const sessionId = (yield* select((state) => state.chat.sessionId)) as string;
    yield* put(appendMessage({ role: "user", content: text }));
    yield* put(chatStarted());
    try {
      const result = yield* call(api.chat, sessionId, text);
      yield* put(chatSucceeded(result.reply, result.chat_mode));
    } catch (error) {
      yield* put(chatFailed(String(error)));
    }
  });

  yield* takeEvery(loadSession, function* (action) {
    const sessionId = action.payload[0];
    try {
      const result = yield* call(api.getMessages, sessionId);
      yield* put(setMessages(result.messages));
    } catch (error) {
      yield* put(chatFailed(String(error)));
    }
  });
}
