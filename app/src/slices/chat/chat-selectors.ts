import { store } from "../../store";

export const selectMessages = store.createSelector((state) => state.chat.messages);
export const selectPending = store.createSelector((state) => state.chat.pending);
export const selectSessionId = store.createSelector((state) => state.chat.sessionId);
export const selectChatMode = store.createSelector((state) => state.chat.chatMode);
export const selectChatError = store.createSelector((state) => state.chat.error);
