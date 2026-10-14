# Frontend reference

[Documentation home](README.md) · [Architecture](architecture.md) · [Getting started](getting-started.md) · [Configuration](configuration.md)

**Source inspection date: 2026-10-14.** This guide describes the current working-tree source, including uncommitted changes. It does not assert clean-checkout parity, a successful build, or observed desktop behavior. No application, provider, live store, or application test is run for this documentation pass.

## Frontend boundary

The Svelte frontend in `app/` uses a Themis store, typed Redux sagas, Tailwind styling and four top-level pages. [main.ts](../app/src/main.ts) mounts [App.svelte](../app/src/App.svelte). [api.ts](../app/src/lib/api.ts) uses Tauri `invoke` and `Channel` for native paper/PDF access and Python-backed workflows. Ordinary Chat/index/evaluation unsubscribe callbacks are no-ops: closing an event channel does not cancel the invocation. No shell-wide command inventory gate is present.

See [native desktop](native-desktop.md) for process and security boundaries, [PDF viewer](pdf-viewer.md) for rendering ownership, and [orchestration](orchestration.md) for the distinction between transport contracts and a finished recovery UI.

## Shell, lifecycle, and styling

`App.svelte` owns `store.init()`/disposal and starts the root `appSaga` on mount. That saga resolves `get_current_library` before it starts library-dependent sagas or dispatches `loadSessions()` and `loadPapers()`. An invalid or inaccessible saved selection stops those loads and presents a recovery view with **Open folder**, rather than silently reading default storage. Page components are conditionally mounted according to `selectActiveRoute()`, but the application store and sagas remain alive across tab switches.

[store.ts](../app/src/store.ts) instantiates one `Store` with `throttledSelectorFrequency: 64`. Reducer keys are `chat`, `sessions`, `papers`, `search`, `paperSearch`, `router`, `evaluation`, and `library`. Components subscribe through selectors and dispatch intent actions; request work belongs to sagas.

The visible tabs are **Chat**, **History Search**, **Papers**, and **RAG Evals**; **RAG Evals** sits at the right end of the header and is faded until hovered or active. The shell uses a full-height layout with a 38-pixel overlay header. Content headers and scroll regions use explicit stacking and padding. Shared controls use [app.css](../app/src/app.css) and `app/src/lib/components/ui/`.

The app has a single light theme; there is no dark mode or theme toggle.

## Routing contract

The route contract is defined in [router-types.ts](../app/src/slices/router/router-types.ts). `AppRoute` is a discriminated union; URL state is deliberately smaller than page state.

| Route | Supported parameters | What is not in the URL |
| --- | --- | --- |
| `/chat` | Optional `sessionId`; optional positive safe-integer `exchangeId` | Composer draft, pending stream, intermediate subanswers |
| `/history` | Optional `q` | Result entities and loading/error state |
| `/papers` | Optional `q` | Selected paper, PDF page/zoom, sidebar visibility |
| `/evals` | None | Uploaded cases, active run, results |

`parseRoute` drops unrecognized parameters and falls back to `{ name: "chat" }` for unknown paths. Empty parameters are omitted. `exchangeId` must consist of decimal digits and parse to a positive safe integer; invalid values disappear during canonicalization. `serializeRoute` uses `URLSearchParams` for escaping.

`routerSaga` in [router-saga.ts](../app/src/slices/router/sagas/router-saga.ts) is the browser-history owner:

1. Read the initial location, parse it, and replace a noncanonical URL.
2. Publish `routeChanged` so feature sagas hydrate their own state.
3. Handle `navigateTo` with `pushState`, avoiding duplicate history entries for the same URL.
4. Handle `replaceWith` with `replaceState` for canonicalization/default selection.
5. Consume `popstate` through `createPopstateChannel`; close that subscription when the saga stops.

Components request navigation rather than manipulate history. `routeForName` creates a bare route rather than restoring prior tab query parameters.

## Chat workflow and state

Sources: [Chat.svelte](../app/src/components/Chat.svelte), [chat-saga.ts](../app/src/slices/chat/sagas/chat-saga.ts), [chat-slice.ts](../app/src/slices/chat/chat-slice.ts).

### Session selection and history links

`chatRouteSaga` responds to `routeChanged`, `selectSession`, and `createSession`. For `/chat` without a session, `hydrateChatRoute` requests the session list, chooses its first item or creates `session-${crypto.randomUUID()}`, and replaces the URL before loading messages. A newly generated session ID is a local navigation identity, not evidence that a persisted conversation already exists.

For a specific session it sets `sessionId`, refreshes session metadata, then calls `getMessages`. A valid `exchangeId` is checked against the returned messages; after two animation frames, `scrollToExchange` targets the message's `data-exchange-id`. An unknown exchange is removed from the URL without scrolling. A session-list failure goes to the sessions slice; a message-hydration failure goes to `chatHydrationFailed`. The Chat component subscribes to the session list, but does not render the sessions slice's separate error/loading fields.

The session chooser preserves backend ordering, deduplicates IDs, and appends the current not-yet-persisted session if needed. Labels use the backend session title, with the raw ID as fallback. **New session** and the chooser are disabled while a turn is pending.

**Current source caveat:** [HistorySearch.svelte](../app/src/components/HistorySearch.svelte), `openMatch`, passes `String(match.exchange_id)` although `ChatRoute.exchangeId` and hydration use numbers. This type/value mismatch is not evidence of successful typechecking or click-to-scroll verification.

### Sending and rendering a turn

The composer rejects blank input, dispatches `sendMessage(text)`, then clears its local text. Enter submits; Shift+Enter leaves multiline input available. There is no visible Stop or Resume button. `chatSaga` uses `takeLeading(sendMessage, ...)`, so a duplicate send is ignored until the active worker finishes.

`streamChatTurn` optimistically appends the user message and calls `chatStarted`. Stream events are converted into reducer actions:

| Event | Reducer action / meaning |
| --- | --- |
| `plan` | `chatPlanReceived`: create initial subquery progress rows |
| `subquery` | `chatSubqueryStarted`: mark the identified hop active |
| `subanswer` | `chatSubanswerReceived`: attach answer and retrieved-source metadata |
| `coverage` | `chatCoverageReceived`: review coverage, complete it, or append follow-ups |
| `delta` | `chatChunk`: append to the provisional final answer |
| `done` | `chatSucceeded`: append the authoritative assistant message and clear the active turn; refresh sessions |
| `error` | `chatFailed`: end pending/streaming state and discard active-turn progress |

The reducer validates ordered `subquery-N` identities, one to five initial subqueries, at most two follow-ups, and at most seven total hops. It rejects malformed or out-of-order progress rather than blindly rendering it. Final-answer deltas are withheld while known hops are incomplete or coverage is still reviewing. Provisional text lives in `activeTurn.finalAnswer`, not in a committed assistant message.

The UI displays role-labelled, whitespace-preserving text rather than a Markdown renderer. Intermediate hops use collapsed `<details>` rows with status, answer, and retrieved chunks. A final assistant message can include source cards containing rank, title, filename, page, chunk index, score, and snippet. These cards are evidence displays, not clickable PDF navigation controls. See [retrieval and chat](retrieval-and-chat.md) for what those scores and sources represent.

Session/reset actions race the stream consumption loop so stale events stop being applied after a session change. Because sagas are app-level, simply leaving the Chat tab does not cancel the turn. The local draft, however, belongs to the component and is not durable across unmounts.

## Search pages and query-keyed collections

The two search domains are independent:

| Domain | Intent / saga | Entity identity | Backend result limit |
| --- | --- | --- | --- |
| History | `runSearch` / [searchSaga](../app/src/slices/search/sagas/search-saga.ts) | `chatHistorySearchId(query)` → `CHAT_HISTORY_SEARCH:` plus trimmed query | 5 |
| Papers | `runPaperSearch` / [paperSearchSaga](../app/src/slices/paper-search/sagas/paper-search-saga.ts) | `paperSearchId(query)` → `PAPERS_SEARCH:` plus trimmed query | 8 |

The identities are defined in [search-types.ts](../app/src/slices/search/search-types.ts) and [paper-search-types.ts](../app/src/slices/paper-search/paper-search-types.ts). Case is preserved; trimming is not case folding.

Submitting a query navigates first. The `routeChanged` handler trims/canonicalizes `q`, clears the visible search for an empty query, or starts the backend read. Loading the same URL, browser back/forward, and direct URL entry therefore use the same hydration path. Submitting an unchanged query while there is no error does not trigger a duplicate request; an errored query can be retried.

Successful results are upserted into `searchResultsCollection`, a Themis `Collection` keyed by `id`. [selectSearchMatches](../app/src/slices/search/search-selectors.ts) selects the entity for the active route query, not simply the last result. Clearing resets visible status while preserving stored entities. These are in-memory result collections, not an on-disk query cache; route hydration still performs backend reads. `searching`, `enabled`, `reason`, `error`, and `hasSearched` are slice-level status fields, not per-entity metadata.

History results show a session/exchange label and both sides of the saved exchange. Click, Enter, and Space request Chat navigation. Disabled semantic search, transport failure, and a successful empty search have separate messages. Paper results show title, filename, optional page, score, and snippet, and dispatch `openPaper(paper_id)`. A result's page is displayed but not passed as a PDF page-navigation target.

## Papers workflow and ownership

[PapersList.svelte](../app/src/components/PapersList.svelte) combines search, a collapsible paper sidebar, index controls, progress, and PDF viewer. Selection dispatches `openPaper` but is not a route parameter. The sidebar toggle has an accessible name/expanded state and the sidebar collapses when focus enters the PDF iframe.

The header displays the current research folder with a truncating path and full-path tooltip. **Open folder** invokes the native chooser and is disabled during tracked chat, session/paper/PDF loads, searches, indexing, reset, or evaluation work; the native host is the final race-safe busy authority. Cancel and same-folder selection preserve all frontend state. A changed selection first replaces the route with bare `/papers`, then reloads the renderer so paper/PDF selection, sessions/messages, query collections, index/evaluation progress, pending views, late saga responses, and route parameters from the previous library cannot reappear. The reload bootstraps the new native selection before any library data read and does not start indexing or provider work.

**Index papers** dispatches `runPaperIndex`. **Reset indices** dispatches `resetIndices` without a confirmation dialog. The tooltip names vector/BM25/status indexes; reset is destructive, not a display reset. Consult [paper indexing](paper-indexing.md) and [storage](data-storage.md).

`papersSaga` handles paper listing, best-effort status reads, streaming index progress, and reset. `takeLeading` prevents duplicate index workers and duplicate resets. The stream's `start`, `started`, `progress`, `done`, and `error` events drive aggregate progress and per-paper indicators. Status dots expose labels through tooltips and `aria-label`, with a separate BM25 dot; a running attempt can mark unvisited papers queued.

Listing is followed by best-effort `paperStatuses` and `bm25Status`; failed hydration leaves fallback not-indexed dots, not verified absence. Index/reset are disabled while either operation runs; there is no journal-readiness gate. Search intents reach the backend directly.

[papersSaga](../app/src/slices/papers/sagas/papers-saga.ts) reads whole-file bytes, revokes the prior Blob URL and stores a replacement. The iframe has no application-defined page/zoom/password controls; see [PDF viewer](pdf-viewer.md).

## RAG Evals page

The page offers **Add tests JSON**; it does not provide a selectable vector/BM25 comparison UI. `addTests` requires a `.json` filename, reads the local file, validates it with [parseEvaluationTestCases](../app/src/lib/evaluation-upload.ts), and dispatches `runEvaluation` immediately on success. There is no separate upload-then-start step.

Cases contain nonempty `question` and `grading_notes` strings plus a `sources` array whose entries must be nonempty strings; the upload parser accepts an empty `sources` array. There is no separate `gold_answer` field in `EvaluationTestCase`. Invalid file/JSON/schema feedback is local `uploadError`; request/stream failure lives in the evaluation slice. `evaluationSaga` uses `takeLeading` and processes result/progress events into indexed case rows and aggregate scores. Results survive tab switches because the store remains alive, but the page does not load prior result files into that store on restart.

The component shows expandable result rows, questions, generated answers, chunks, criterion explanations and seven aggregate means: context precision, context recall, relevant chunk ranking, context utilization, semantic similarity, completeness and hallucination rate. Grading notes/source lists are inputs, not displayed gold-answer sections. [evaluationScoreColor](../app/src/lib/evaluation-score.ts) colors collapsed subtitles; missing scores are not zeros.

The [component](../app/src/components/RagEvals.svelte) and [saga](../app/src/slices/evaluation/sagas/evaluation-saga.ts) handle `retryRoundStart`, `caseRetryStart` and `caseRetryResult`, show two deferred rounds and display `failureSummary`/`attempt_history`. [evaluation-diagnostics.ts](../app/src/lib/evaluation-diagnostics.ts) sanitizes before state insertion. See [evaluation](evaluation.md).

## What the visible UI does not provide

- No settings page for provider credentials, model names, storage roots or embedding dimensions; see [configuration](configuration.md).
- No durable question/agent/tool trace screen, pending-run list, resume, or discard controls. [orchestration-api.ts](../app/src/lib/orchestration-api.ts) defines `startOrchestration`, `resumeOrchestration`, `pendingOrchestration`, `discardOrchestration`, and `orchestrationTrace`, and `api.ts` exports them, but Chat does not consume them. `legacyOrchestrationEvent` is an adapter, not a mounted recovery workflow.
- No deep link encoding of a selected paper or graph node, no page jump from a source card, and no persisted graph filter/viewport.
- No evaluation-history browser or result-file import screen. Backend artifact persistence does not imply frontend restoration.

## Source and test map

These are existing test locations to consult, not tests executed for this guide. Some tests inspect source contracts or use fakes; their presence is not proof of mounted desktop behavior.

| Area | Existing tests / meaningful cases |
| --- | --- |
| Routes/history | [router.test.mjs](../app/tests/router.test.mjs), [chat-routing.test.mjs](../app/tests/chat-routing.test.mjs) |
| Search routing/collections | [search-routing.test.mjs](../app/tests/search-routing.test.mjs), [search-collections.test.mjs](../app/tests/search-collections.test.mjs) |
| Chat events/state | [chat-progress-state.test.mjs](../app/tests/chat-progress-state.test.mjs), [chat-progress-saga.test.mjs](../app/tests/chat-progress-saga.test.mjs), [chat-progress-interface.test.mjs](../app/tests/chat-progress-interface.test.mjs) |
| Paper status | [papers-state.test.mjs](../app/tests/papers-state.test.mjs) |
| Evaluation failures/interface | [evaluation-interface.test.mjs](../app/tests/evaluation-interface.test.mjs), [evaluation-saga.test.mjs](../app/tests/evaluation-saga.test.mjs), [evaluation-diagnostics.test.mjs](../app/tests/evaluation-diagnostics.test.mjs) |
| Graph and PDF detail | [Knowledge-graph test map](knowledge-graph.md#source-and-test-map), [PDF viewer](pdf-viewer.md) |

For a frontend change, update focused state/saga/component tests, then follow [operations and testing](operations-and-testing.md). Check both Python and desktop consumers when a shared transport contract changes.
