import assert from 'node:assert/strict';
import test, { before, after } from 'node:test';
import { runSaga, stdChannel } from 'redux-saga';
import { createServer } from 'vite';
import { fileURLToPath } from 'node:url';
import { readFile } from 'node:fs/promises';

let vite, saga, actions;
const settle = () => new Promise((resolve) => setImmediate(resolve));
before(async () => {
  vite = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), appType: 'custom',
    logLevel: 'silent', server: { middlewareMode: true } });
  ({ papersSaga: saga } = await vite.ssrLoadModule('/src/slices/papers/sagas/papers-saga.ts'));
  actions = await vite.ssrLoadModule('/src/slices/papers/papers-slice.ts');
});
after(async () => vite.close());
function start(t, methods) {
  const old = globalThis.window;
  globalThis.window = { research: methods };
  let state = { papers: actions.papersReducer(undefined, { type: 'init' }) };
  const input = stdChannel();
  const dispatch = (action) => { state = { papers: actions.papersReducer(state.papers, action) }; input.put(action); };
  const task = runSaga({ channel: input, dispatch, getState: () => state }, saga);
  t.after(async () => { task.cancel(); await task.toPromise(); globalThis.window = old; });
  return { dispatch, task, state: () => state.papers };
}
const handle = (value) => ({ done: Promise.resolve(value), cancel() {} });
const health = (names) => handle({ commands: names.map((command) => ({ command, available: true })) });

test('browsing-only health lists PDFs but never calls unavailable status, search, indexing or reset', async (t) => {
  const run = start(t, {
    health: () => health(['health', 'list_papers', 'get_paper', 'read_pdf']),
    listPapers: () => handle([{ id: 'a', title: 'A', filename: 'a.pdf', size_bytes: 1 }]),
  });
  run.dispatch(actions.loadPapers()); await settle();
  assert.equal(run.state().items[0].id, 'a');
  assert.equal(run.state().statusesKnown, false);
  assert.equal(run.state().bm25Known, false);
  assert.equal(run.state().error, null);
  run.dispatch(actions.runPaperIndex()); await settle();
  assert.match(run.state().indexError, /unavailable/);
  assert.equal(run.state().indexing, false);
  run.dispatch(actions.resetIndices()); await settle();
  assert.match(run.state().indexError, /unavailable/);
  assert.equal(run.state().resetting, false);
});

test('missing roots give actionable unavailable, not empty-library success', async (t) => {
  const run = start(t, { health: () => health(['health']) });
  run.dispatch(actions.loadPapers()); await settle();
  assert.match(run.state().error, /CAPSTONE_PAPERS_DIR.*restart/);
  assert.equal(run.state().loading, false);
});

test('failed status refresh stays unknown and does not prevent browsing', async (t) => {
  const run = start(t, { health: () => health(['list_papers', 'paper_statuses', 'bm25_status']),
    listPapers: () => handle([]), paperStatuses() { throw Error('status unavailable'); },
    bm25Status() { throw Error('status unavailable'); } });
  run.dispatch(actions.setPaperStatuses([{ paper_id: 'a', status: 'indexed' }]));
  run.dispatch(actions.setBm25Statuses(['a']));
  run.dispatch(actions.loadPapers()); await settle();
  assert.equal(run.state().error, null);
  assert.equal(run.state().statusesKnown, false);
  assert.equal(run.state().bm25Known, false);
});

test('open/close update selection only; the library saga never creates a second PDF read', async (t) => {
  const reads = [];
  const run = start(t, { readPdf(id) { reads.push(id); return handle({ bytes: [37, 80, 68, 70] }); } });
  run.dispatch(actions.openPaper('a'));
  run.dispatch(actions.openPaper('b'));
  await settle();
  assert.equal(run.state().selectedId, 'b');
  assert.deepEqual(reads, []);
  run.dispatch(actions.closePaper());
  assert.equal(run.state().selectedId, null);
  for (const key of ['bytes', 'pdfUrl', 'password', 'document']) assert.equal(key in run.state(), false);
});

test('Papers source contract disables unsupported controls and shows unknown status plus inline errors', async () => {
  const list = await readFile(new URL('../src/components/PapersList.svelte', import.meta.url), 'utf8');
  for (const control of ['canIndex', 'canReset', 'canSearch']) assert.ok(list.includes(`disabled={!${control}`));
  assert.match(list, /existing index contents have not been checked/);
  assert.match(list, /BM25 status unavailable/);
  // Structural guard only, not evidence of native layout/hit testing: the library
  // bar must reserve flow space above the independently scrollable PDF controls.
  assert.doesNotMatch(list, /absolute inset-x-0 top-0/);
  assert.match(list, /class="relative min-h-0 flex-1"/);
  const viewer = await readFile(new URL('../src/components/PdfViewer.svelte', import.meta.url), 'utf8');
  assert.match(viewer, /role="alert"/);
  assert.match(viewer, /onDestroy\(\(\) => store.dispatch\(closePaper\(\)\)\)/);
  assert.match(viewer, /selectedId.subscribe/);
  assert.match(viewer, /createPdfReader/);
  assert.match(viewer, /role="region" aria-label="PDF page, scroll to read" tabindex="0"/);
  assert.doesNotMatch(viewer, /<iframe/);
});