import { call, fork, put, race, select, take, takeLatest, takeEvery, takeLeading } from "typed-redux-saga";
import { eventChannel, END, type EventChannel } from "redux-saga";
import { api, type ChatStreamEvent } from "../../../lib/api";
import { navigateTo, replaceWith, routeChanged } from "../../router/router-slice";
import type { ChatRoute } from "../../router/router-types";
import { loadSessions, sessionsFailed, setSessions } from "../../sessions/sessions-slice";
import {
  appendMessage,
  chatChunk,
  chatCoverageReceived,
  chatFailed,
  chatHydrationFailed,
  chatPlanReceived,
  chatStarted,
  chatSubanswerReceived,
  chatSubqueryStarted,
  chatSucceeded,
  createSession,
  loadSession,
  selectSession,
  sendMessage,
  setMessages,
  setSessionId,
} from "../chat-slice";

export interface ChatRouteServices {
  listSessions: typeof api.listSessions;
  getMessages: typeof api.getMessages;
  createSessionId(): string;
  afterRender(): Promise<void>;
  scrollToExchange(exchangeId: number): void;
}

const browserChatRouteServices: ChatRouteServices = {
  listSessions: api.listSessions,
  getMessages: api.getMessages,
  createSessionId: () => `session-${crypto.randomUUID()}`,
  afterRender: () =>
    new Promise<void>((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
    ),
  scrollToExchange: (exchangeId) => {
    document
      .querySelector(`[data-exchange-id="${exchangeId}"]`)
      ?.scrollIntoView({ block: "center" });
  },
};

function* hydrateChatRoute(route: ChatRoute, services: ChatRouteServices) {
  if (!route.sessionId) {
    try {
      const result = yield* call(services.listSessions);
      yield* put(setSessions(result.sessions));
      const sessionId =
        result.sessions[0]?.session_id ?? (yield* call(services.createSessionId));
      yield* put(replaceWith({ name: "chat", sessionId }));
    } catch (error) {
      yield* put(sessionsFailed(String(error)));
    }
    return;
  }

  yield* put(setSessionId(route.sessionId));
  try {
    const sessions = yield* call(services.listSessions);
    yield* put(setSessions(sessions.sessions));
  } catch (error) {
    yield* put(sessionsFailed(String(error)));
  }

  try {
    const result = yield* call(services.getMessages, route.sessionId);
    yield* put(setMessages(result.messages));
    if (route.exchangeId === undefined) return;
    if (!result.messages.some((message) => message.exchange_id === route.exchangeId)) {
      yield* put(replaceWith({ name: "chat", sessionId: route.sessionId }));
      return;
    }
    yield* call(services.afterRender);
    yield* call(services.scrollToExchange, route.exchangeId);
  } catch (error) {
    yield* put(chatHydrationFailed(String(error)));
  }
}

export function* chatRouteSaga(
  services: ChatRouteServices = browserChatRouteServices,
) {
  yield* takeLatest(routeChanged, function* (action) {
    const route = action.payload[0];
    if (route.name === "chat") yield* call(hydrateChatRoute, route, services);
  });
  yield* takeLatest(selectSession, function* (action) {
    yield* put(navigateTo({ name: "chat", sessionId: action.payload[0] }));
  });
  yield* takeLatest(createSession, function* () {
    const sessionId = yield* call(services.createSessionId);
    yield* put(navigateTo({ name: "chat", sessionId }));
  });
}

// Bridge the cancellable desktop stream into a redux-saga eventChannel: each
// backend `ChatStreamEvent` is forwarded as an emission, and the channel is
// closed (END) when the invoke promise settles or rejects.
export type ChatStreamService = typeof api.chatStream;

function createChatStreamChannel(
  sessionId: string,
  message: string,
  chatStream: ChatStreamService,
  turnId: string,
): EventChannel<ChatStreamEvent> {
  return eventChannel<ChatStreamEvent>((emit) => {
    let terminalEventSeen = false;
    const request = chatStream(sessionId, message, (event) => {
      terminalEventSeen ||= event.event === "done" || event.event === "error";
      emit(event);
    }, turnId);
    request
      .catch((error: unknown) => {
        if (!terminalEventSeen) {
          terminalEventSeen = true;
          emit({ event: "error", message: String(error) });
        }
      })
      .finally(() => {
        if (!terminalEventSeen) {
          emit({
            event: "error",
            message:
              "Chat outcome is unknown because the final response was not received. Check saved history before explicitly starting a new turn; no turn was replayed.",
          });
        }
        emit(END);
      });
    return () => request.cancel?.();
  });
}

// Send one chat turn: optimistically append the user message, open the
// streaming channel, apply each token delta as it arrives, then finalize with
// the authoritative reply (or record the error).
export function* streamChatTurn(
  action: ReturnType<typeof sendMessage>,
  chatStream: ChatStreamService,
) {
  const text = action.payload[0];
  if (!text.trim()) {
    return;
  }
  const sessionId = (yield* select((state) => state.chat.sessionId)) as string;
  const turnId = crypto.randomUUID();
  yield* put(appendMessage({ role: "user", content: text, turn_id: turnId }));
  yield* put(chatStarted());
  const channel = createChatStreamChannel(sessionId, text, chatStream, turnId);
  // `take` on a channel terminates this loop once the channel emits END
  // (dispatched after the streaming invoke settles).
  try {
    while (true) {
      const { event, reset } = yield* race({
        event: take(channel),
        reset: take([selectSession, createSession, setSessionId, loadSession]),
      });
      if (reset || !event) return;
      if (event.event === "plan") {
        yield* put(chatPlanReceived(event.subqueries));
      } else if (event.event === "coverage") {
        yield* put(chatCoverageReceived(event.status, event.total, event.subqueries));
      } else if (event.event === "subquery") {
        yield* put(
          chatSubqueryStarted(
            { id: event.id, index: event.index, query: event.query },
            event.total,
          ),
        );
      } else if (event.event === "subanswer") {
        yield* put(
          chatSubanswerReceived(
            event.id,
            event.index,
            event.query,
            event.answer,
            event.sources,
            event.total,
          ),
        );
      } else if (event.event === "delta") {
        yield* put(chatChunk(event.delta));
      } else if (event.event === "done") {
        yield* put(chatSucceeded(event.reply, event.chatMode, event.sources, {
          turn_id: event.turnId, exchange_id: event.exchangeId,
          history_index_status: event.historyIndexStatus,
          history_index_status_observed_at: event.historyIndexStatusObservedAt,
        }));
        yield* put(loadSessions());
        break;
      } else if (event.event === "error") {
        yield* put(chatFailed(event.message));
        break;
      }
    }
  } catch (error) {
    yield* put(chatFailed(String(error)));
  } finally {
    channel.close();
  }
}

export function* chatSaga(chatStream: ChatStreamService = api.chatStream) {
  yield* takeLeading(sendMessage, function* (action) {
    yield* streamChatTurn(action, chatStream);
  });

  yield* takeEvery(loadSession, function* (action) {
    const sessionId = action.payload[0];
    try {
      const result = yield* call(api.getMessages, sessionId);
      yield* put(setMessages(result.messages));
    } catch (error) {
      yield* put(chatHydrationFailed(String(error)));
    }
  });

  yield* fork(chatRouteSaga);
}
