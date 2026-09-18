import assert from 'node:assert/strict';
import test from 'node:test';
import { EventEmitter } from 'node:events';
import { readFile, mkdtemp, mkdir, writeFile, symlink, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';
import { webcrypto } from 'node:crypto';
import { assertSender, trustedURL, webPreferences, csp, secureContents, appResponse } from '../electron/security.mjs';
import { Broker } from '../electron/broker.mjs';
import { methods, parseResponse } from '../electron/validation.mjs';
import { PdfAssembler, MAX_PDF_BYTES, MAX_PDF_CHUNK, parseEvent, terminalEvent } from '../electron/adapters.mjs';

test('only exact app/dev origins and the owning top-level frame can invoke', () => {
  const frame = { url: 'app://research/chat?sessionId=s' };
  const contents = { mainFrame: frame };
  const window = { isDestroyed: () => false, webContents: contents };
  assert.doesNotThrow(() => assertSender({ sender: contents, senderFrame: frame }, window));
  for (const url of ['https://research/chat', 'app://research.evil/chat', 'app://user@research/', 'app://research:99/',
    'file:///tmp/index.html', 'javascript:alert(1)', 'http://127.0.0.1:51730', 'http://localhost:5173']) {
    assert.equal(trustedURL(url), false);
    assert.equal(trustedURL(url, true), false);
  }
  assert.equal(trustedURL('http://127.0.0.1:5173/chat', true), true);
  assert.throws(() => assertSender({ sender: {}, senderFrame: frame }, window));
  assert.throws(() => assertSender({ sender: contents, senderFrame: { ...frame } }, window));
  assert.throws(() => assertSender({ sender: contents }, window));
  assert.equal(webPreferences.sandbox, true);
  assert.equal(webPreferences.contextIsolation, true);
  assert.equal(webPreferences.nodeIntegration, false);
  assert.equal(webPreferences.webviewTag, false);
  assert.doesNotMatch(csp(), /unsafe-eval|https:|unsafe-inline.*script-src/);
  assert.match(csp(), /script-src 'self'/);
  assert.match(csp(), /frame-src blob:/);
});

test('navigation, redirects, popups, permissions and webviews are restricted', () => {
  const contents = new EventEmitter();
  contents.session = new EventEmitter();
  contents.session.setPermissionRequestHandler = (fn) => { contents.permission = fn; };
  contents.session.setPermissionCheckHandler = (fn) => { contents.check = fn; };
  contents.setWindowOpenHandler = (fn) => { contents.open = fn; };
  secureContents(contents);
  let prevented = 0;
  const event = { preventDefault: () => prevented++ };
  contents.emit('will-navigate', event, 'app://research/papers');
  assert.equal(prevented, 0);
  contents.emit('will-navigate', event, 'https://example.com');
  contents.emit('will-redirect', event, 'file:///etc/passwd');
  contents.emit('will-attach-webview', event);
  contents.session.emit('will-download', event);
  contents.emit('will-frame-navigate', { ...event, url: 'https://external.invalid/', isMainFrame: false });
  contents.emit('will-frame-navigate', { ...event, url: 'blob:app://research/pdf-id', isMainFrame: false });
  assert.equal(prevented, 5);
  assert.equal(webPreferences.plugins, true);
  assert.deepEqual(contents.open({ url: 'https://example.com' }), { action: 'deny' });
  contents.permission(contents, 'media', (granted) => assert.equal(granted, false));
  assert.equal(contents.check(), false);
});

test('custom app scheme supports route reloads and serves only contained build assets', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'shell-origin-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const dist = join(dir, 'dist');
  await mkdir(join(dist, 'assets'), { recursive: true });
  await writeFile(join(dist, 'index.html'), '<main>shell</main>');
  await writeFile(join(dist, 'assets', 'app.js'), 'export {}');
  await writeFile(join(dir, 'outside'), 'NOT PUBLIC');
  await symlink(join(dir, 'outside'), join(dist, 'assets', 'escape'));
  for (const route of ['/', '/chat?sessionId=s', '/papers?q=query', '/history', '/graph', '/evals']) {
    const response = await appResponse(new Request(`app://research${route}`), dist);
    assert.equal(response.status, 200);
    assert.equal(await response.text(), '<main>shell</main>');
    assert.match(response.headers.get('Content-Security-Policy'), /default-src 'self'/);
  }
  assert.equal((await appResponse(new Request('app://research/assets/app.js'), dist)).status, 200);
  for (const path of ['/assets/escape', '/assets/%2e%2e%2f%2e%2e%2foutside', '/electron/main.mjs', '/.env', '/backend/target/debug/research-backend']) {
    assert.notEqual((await appResponse(new Request(`app://research${path}`), dist)).status, 200);
  }
  assert.equal((await appResponse(new Request('app://research/chat', { method: 'POST' }), dist)).status, 403);
});

test('request allowlist rejects raw commands, paths, credentials, unknown fields and malformed cases', async () => {
  let called = false;
  const broker = new Broker({ request() { called = true; throw new Error(); } });
  for (const [method, args] of [['exec', {}], ['invoke', {}], ['__proto__', {}], ['readPdf', { paper_id: '../secret' }],
    ['health', { env: {} }], ['chat', { session_id: 's', message: '?', apiKey: 'not-a-real-key' }],
    ['runEvaluation', { retrieval_type: 'vector', test_cases: [] }], ['search', { query: 'q', limit: -1 }]]) {
    assert.equal((await broker.run(1, { token: 'x', method, args }, () => {})).ok, false);
  }
  assert.equal(called, false);
});

test('duplicate tokens cannot remove existing work and cancellation is owner-scoped', async () => {
  let reject;
  let cancelled = 0;
  const broker = new Broker({ request() { return { done: new Promise((_resolve, fail) => { reject = fail; }),
    cancel() { cancelled++; reject(new Error('private')); } }; } });
  const payload = { token: 'x', method: 'chatStream', args: { session_id: 's', message: 'q' } };
  const first = broker.run(1, payload, () => {});
  assert.equal((await broker.run(1, payload, () => {})).ok, false);
  assert.equal(broker.requests.size, 1);
  broker.cancel(2, 'x');
  assert.equal(cancelled, 0);
  broker.cancelOwner(1);
  assert.equal(cancelled, 1);
  assert.equal((await first).ok, false);
  assert.equal(broker.requests.size, 0);
});

test('PDF validation enforces integer bytes, chunk/frame bounds, contiguous offsets and terminal byte equality', () => {
  for (const bytes of [[-1], [256], [1.5], ['1'], [], Array(MAX_PDF_CHUNK + 1).fill(0)]) {
    assert.throws(() => new PdfAssembler('p').event('pdf_chunk', { offset: 0, bytes }));
  }
  const pdf = new PdfAssembler('p');
  assert.throws(() => pdf.event('pdf_chunk', { offset: 1, bytes: [1] }));
  pdf.event('pdf_chunk', { offset: 0, bytes: [1, 2] });
  assert.throws(() => pdf.result({ id: 'p', filename: 'p.pdf', mime_type: 'application/pdf', total_bytes: 1 }));
  assert.throws(() => pdf.result({ id: 'other', filename: 'p.pdf', mime_type: 'application/pdf', total_bytes: 2 }));
  pdf.size = MAX_PDF_BYTES;
  assert.throws(() => pdf.event('pdf_chunk', { offset: MAX_PDF_BYTES, bytes: [1] }));
  pdf.clear();
  assert.equal(pdf.chunks.length, 0);
});

test('domain output validation preserves evaluation casing/nested provenance and denies premature terminal events', () => {
  const event = parseEvent('evaluate_stream', 'case_start', { case_index: 1, total: 1, retrieval_type: 'hybrid',
    input: { question: '?', grading_notes: 'notes', sources: ['p'] } });
  assert.equal(event.event, 'caseStart');
  assert.equal(event.caseIndex, 1);
  assert.equal(event.input.grading_notes, 'notes');
  assert.throws(() => parseEvent('chat_stream', 'done', {}));
  assert.throws(() => parseEvent('chat_stream', 'delta', { delta: 'ok', api_key: 'not-a-real-key' }));
  assert.throws(() => terminalEvent('chat_stream', { reply: 'incomplete' }));
  assert.throws(() => parseResponse('list_papers', [{ id: '../p' }]));
  assert.throws(() => parseResponse('get_messages', { session_id: 's', messages: [{ role: 'assistant', content: 'x', sources: [{}] }] }));
});

test('preload exposes fixed typed methods, hides IPC objects, and cleans callbacks on result/cancel/reload', async () => {
  const source = await readFile(new URL('../electron/preload.cjs', import.meta.url), 'utf8');
  const ipc = new EventEmitter();
  const calls = [];
  const pending = new Map();
  ipc.invoke = (channel, ...args) => {
    calls.push({ channel, args });
    if (channel !== 'research:run') return Promise.resolve();
    return new Promise((resolve) => pending.set(args[0].token, resolve));
  };
  let api, unload;
  vm.runInNewContext(source, { require(name) {
    assert.equal(name, 'electron');
    return { ipcRenderer: ipc, contextBridge: { exposeInMainWorld(name, value) { assert.equal(name, 'research'); api = value; } } };
  }, crypto: webcrypto, window: { addEventListener(event, callback) { assert.equal(event, 'beforeunload'); unload = callback; } } });
  assert.deepEqual(Object.keys(api).sort(), [...Object.keys(methods), 'toggleMaximize'].sort());
  let events = 0;
  const first = api.chatStream('s', '?', () => events++);
  const payload = calls[0].args[0];
  assert.equal(payload.args.session_id, 's');
  ipc.emit('research:event', { sender: 'must-not-leak' }, payload.token, { event: 'delta', delta: 'x' }, 0);
  assert.equal(events, 1);
  pending.get(payload.token)({ ok: true, data: null });
  await first.done;
  ipc.emit('research:event', {}, payload.token, {}, 1);
  assert.equal(events, 1);
  const second = api.chatStream('s', '?', () => events++);
  const secondPayload = calls.findLast((v) => v.channel === 'research:run').args[0];
  second.cancel(); second.cancel();
  ipc.emit('research:event', {}, secondPayload.token, {}, 0);
  assert.equal(events, 1);
  assert.equal(calls.filter((v) => v.channel === 'research:cancel').length, 1);
  pending.get(secondPayload.token)({ ok: false, error: 'Request cancelled.' });
  await assert.rejects(second.done, /cancelled/);
  unload();
  assert.equal(ipc.listenerCount('research:event'), 0);
  assert.equal(calls.at(-1).channel, 'research:cancel-all');
});

test('dev server never loads environment files and denies desktop/backend or legacy paths', async (t) => {
  const { createServer } = await import('vite');
  const root = fileURLToPath(new URL('..', import.meta.url));
  const server = await createServer({ root, logLevel: 'silent', server: { port: 0, strictPort: false } });
  t.after(async () => {
    // Let Vite finish speculative request hooks before closing its plugin container.
    await server.waitForRequestsIdle();
    await server.close();
  });
  assert.equal(server.config.envDir, false);
  assert.deepEqual(server.config.envPrefix, []);
  await server.listen();
  const address = server.httpServer.address();
  const origin = `http://127.0.0.1:${address.port}`;
  assert.equal((await fetch(`${origin}/src/main.ts`)).status, 200);
  for (const path of ['/electron/main.mjs', '/backend/src/main.rs', '/scripts/dev.mjs',
    `/@fs/${fileURLToPath(new URL('../../app/src/main.ts', import.meta.url))}`]) {
    assert.equal((await fetch(origin + path)).status, 403);
  }
});