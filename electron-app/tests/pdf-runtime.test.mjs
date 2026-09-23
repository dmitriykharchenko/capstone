import assert from 'node:assert/strict';
import test, { before, after } from 'node:test';
import { createServer } from 'vite';
import { fileURLToPath } from 'node:url';
import { randomBytes } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import ts from 'typescript';
import { syntheticPdf } from './pdf-fixtures.mjs';

let vite, lifecycle, runtime, library;
const deferred = () => Promise.withResolvers();
const settle = () => new Promise((done) => setImmediate(done));
const metadata = { preview_id: '00000000-0000-4000-8000-000000000001', total_bytes: 4 };
function source(bytes) {
  return { length: bytes.length, read: async (begin, end) => new Uint8Array(bytes.slice(begin, end)), close: async () => {} };
}
before(async () => {
  vite = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), appType: 'custom',
    logLevel: 'silent', server: { middlewareMode: true } });
  lifecycle = await vite.ssrLoadModule('/src/lib/pdf-lifecycle.ts');
  runtime = await vite.ssrLoadModule('/src/lib/pdf-runtime.ts');
});
after(async () => { await vite.close(); });

function fakeLoader(overrides = {}) {
  const ready = deferred(), calls = { destroyed: 0, disposed: 0, created: 0 };
  const task = { promise: ready.promise, destroy: async () => { calls.destroyed++; } };
  const api = { PasswordResponses: { INCORRECT_PASSWORD: 2 }, TextLayer: class {},
    PDFDataRangeTransport: class { constructor(length) { this.length = length; } onDataRange() {} },
    getDocument(options) { calls.options = options; return task; } };
  const load = lifecycle.createPdfLoader({ assetBase: 'app://research/assets/pdfjs-test/',
    loadLibrary: async () => api, createWorker(_lib, failed) {
      calls.created++; calls.failed = failed;
      return { worker: {}, dispose() { calls.disposed++; } };
    }, ...overrides });
  return { load: (bytes, options) => load(source(bytes), options), rawLoad: load, calls, ready, task, api };
}

test('runtime import is SSR-safe and load without a browser fails closed', async () => {
  assert.equal(typeof globalThis.window, 'undefined');
  const handle = runtime.loadPdf(metadata);
  await assert.rejects(handle.promise, { code: 'unavailable', message: 'Local PDF renderer unavailable.' });
  await handle.destroy();
});

async function runtimeHost(href, version = runtime.PDFJS_VERSION) {
  const source = await readFile(new URL('../src/lib/pdf-runtime.ts', import.meta.url), 'utf8');
  const compiled = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.CommonJS } }).outputText;
  const f = fakeLoader(), ports = [], imports = [];
  let destroyed = 0;
  class Port extends EventTarget {
    constructor(url, options) { super(); this.url = url; this.options = options; this.terminated = 0; ports.push(this); }
    terminate() { this.terminated++; }
  }
  const api = { ...f.api, version, PDFWorker: { create(options) {
    assert.equal(options.port, ports.at(-1));
    assert.equal(options.verbosity, 0);
    return { destroy() { destroyed++; } };
  } } };
  const exports = {};
  vm.runInNewContext(compiled, { exports, URL, Worker: Port, window: { location: { href } }, require(id) {
    if (id === 'pdfjs-dist/package.json') return { version: runtime.PDFJS_VERSION };
    if (id === './pdf-lifecycle') return lifecycle;
    if (id === './api') return { api: { closePdf: async () => {} } };
    imports.push(id); return api;
  } });
  return { ...f, exports, ports, imports, destroyed: () => destroyed };
}

test('production wrapper supplies an explicit local module port and owns teardown (Node host double)', async () => {
  for (const origin of ['app://research', 'http://127.0.0.1:5173']) {
    const host = await runtimeHost(`${origin}/papers`);
    const handle = host.exports.loadPdf(metadata); await settle();
    assert.deepEqual(host.imports, [`${origin}${runtime.PDFJS_ASSET_PATH}build/pdf.mjs`]);
    assert.equal(host.ports[0].url.href, `${origin}${runtime.PDFJS_ASSET_PATH}build/pdf.worker.mjs`);
    assert.equal(host.ports[0].options.type, 'module');
    host.ports[0].dispatchEvent(new Event('error'));
    await assert.rejects(handle.promise, { code: 'unavailable' }); await handle.destroy();
    assert.equal(host.ports[0].terminated, 1); assert.equal(host.destroyed(), 1);
    host.ports[0].dispatchEvent(new Event('messageerror'));
    assert.equal(host.ports[0].terminated, 1);
  }
});

test('production wrapper rejects wrong origins and version mismatch before creating a worker', async () => {
  for (const href of ['https://research/papers', 'app://research.evil/papers', 'app://user@research/papers',
    'app://research:99/papers', 'file:///papers', 'http://127.0.0.1:51730/papers']) {
    const host = await runtimeHost(href);
    await assert.rejects(host.exports.loadPdf(metadata).promise, { code: 'unavailable' });
    assert.equal(host.imports.length, 0); assert.equal(host.ports.length, 0);
  }
  const host = await runtimeHost('app://research/papers', 'mismatch');
  await assert.rejects(host.exports.loadPdf(metadata).promise, { code: 'unavailable' });
  assert.equal(host.ports.length, 0);
});

test('supplies lazy ranges and pins local, non-WASM, quiet options without a URL/password', async () => {
  const { load, calls, ready } = fakeLoader();
  const bytes = new Uint8Array([37, 80, 68, 70]);
  const handle = load(bytes);
  await settle();
  assert.equal(calls.options.range.length, 4);
  assert.equal(calls.options.disableAutoFetch, true);
  assert.equal(calls.options.disableStream, true);
  assert.equal(calls.options.rangeChunkSize, 65536);
  assert.equal(bytes.byteLength, 4);
  for (const key of ['data', 'url', 'password', 'docBaseUrl', 'iccUrl']) assert.equal(key in calls.options, false);
  assert.equal(calls.options.useWasm, false);
  assert.equal(calls.options.enableXfa, false);
  assert.equal(calls.options.verbosity, 0);
  assert.equal(calls.options.useWorkerFetch, true);
  assert.equal(calls.options.stopAtErrors, true);
  assert.equal(calls.options.useSystemFonts, false);
  assert.match(calls.options.wasmUrl, /^app:\/\/research\/assets\/pdfjs-test\/wasm\/$/);
  ready.resolve({ numPages: 1 });
  assert.equal((await handle.promise).document.numPages, 1);
  assert.equal(handle.destroy(), handle.destroy());
  await handle.destroy();
  assert.equal(calls.destroyed, 1); assert.equal(calls.disposed, 1);
});

test('invalid source lengths and pre-aborted ownership never create a worker', async () => {
  for (const length of [0, -1, 1.5, '1', NaN, Infinity, Number.MAX_SAFE_INTEGER + 1]) {
    const { rawLoad, calls } = fakeLoader();
    await assert.rejects(rawLoad({ ...source([1]), length }).promise, { code: 'invalid' });
    assert.equal(calls.created, 0);
  }
  const { load, calls } = fakeLoader();
  await assert.rejects(load([1], { signal: AbortSignal.abort() }).promise, { code: 'cancelled' });
  assert.equal(calls.created, 0);
});

test('destroy before lazy import completes cannot start a worker or publish stale results', async () => {
  const imported = deferred();
  const { load, calls, api } = fakeLoader({ loadLibrary: () => imported.promise });
  const handle = load([1]);
  await handle.destroy(); imported.resolve(api); await settle();
  await assert.rejects(handle.promise, { code: 'cancelled' });
  assert.equal(calls.created, 0);
});

test('switching loads, abort after resolution, late rejection and worker failures drain ownership', async () => {
  const first = fakeLoader(), second = fakeLoader(), controller = new AbortController();
  const a = first.load([1]), b = second.load([2], { signal: controller.signal });
  await settle();
  await a.destroy(); first.ready.resolve({ numPages: 99 });
  second.ready.resolve({ numPages: 2 });
  await assert.rejects(a.promise, { code: 'cancelled' });
  assert.equal((await b.promise).document.numPages, 2);
  controller.abort(); await b.destroy();
  assert.equal(first.calls.disposed, 1); assert.equal(second.calls.disposed, 1);
  const failed = fakeLoader(), c = failed.load([3]); await settle();
  failed.calls.failed(); failed.ready.reject(new Error('synthetic internal detail'));
  await assert.rejects(c.promise, { code: 'unavailable', message: 'Local PDF renderer unavailable.' });
  await c.destroy(); assert.equal(failed.calls.disposed, 1);
});

test('unresponsive worker teardown has a deadline then disposes its port', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const f = fakeLoader(), handle = f.load([1]); await settle();
  f.task.destroy = () => new Promise(() => {});
  const drained = handle.destroy();
  assert.equal(f.calls.disposed, 0);
  t.mock.timers.tick(lifecycle.PDF_DESTROY_GRACE_MS);
  await drained; assert.equal(f.calls.disposed, 1);
});

test('password prompts are transient single-use capabilities, invalidated on retry/close', async () => {
  const f = fakeLoader(), prompts = [];
  const handle = f.load([1], { onPassword: (prompt) => prompts.push(prompt) });
  await settle(); let updates = 0;
  f.task.onPassword(() => updates++, 1);
  assert.equal(prompts[0].reason, 'required');
  assert.equal(prompts[0].submit(randomBytes(8).toString('hex')), true);
  assert.equal(prompts[0].submit(randomBytes(8).toString('hex')), false);
  f.task.onPassword(() => updates++, 2);
  assert.equal(prompts[1].reason, 'incorrect');
  await prompts[0].cancel(); assert.equal(f.calls.disposed, 0);
  await prompts[1].cancel();
  assert.equal(prompts[1].submit(randomBytes(8).toString('hex')), false);
  assert.equal(updates, 1);
  await assert.rejects(handle.promise, { code: 'cancelled' });
});

test('absent/throwing password handler and library failures expose only fixed errors', async () => {
  for (const options of [{}, { onPassword() { throw new Error('synthetic internal detail'); } }]) {
    const f = fakeLoader(), handle = f.load([1], options); await settle();
    f.task.onPassword(() => {}, 1);
    await assert.rejects(handle.promise, { code: 'password', message: 'PDF password required.' });
    await handle.destroy(); assert.equal(f.calls.disposed, 1);
  }
  const f = fakeLoader(), handle = f.load([1]); await settle();
  f.ready.reject(new Error('synthetic internal detail'));
  await assert.rejects(handle.promise, { code: 'invalid', message: 'PDF is invalid or unsupported.' });
  await handle.destroy();
});

async function realLoader() {
  // Explicit Node/fake-worker evidence, NOT the Electron worker or page paint.
  // Upstream Node compatibility shims supply toHex/getOrInsertComputed missing
  // in Node24. We then exercise the SAME modern display/worker assets as the app.
  await import('pdfjs-dist/legacy/build/pdf.mjs');
  library ??= await import('pdfjs-dist/build/pdf.mjs');
  let disposed = 0, worker;
  const load = lifecycle.createPdfLoader({
    assetBase: fileURLToPath(new URL('../node_modules/pdfjs-dist/', import.meta.url)),
    loadLibrary: async () => ({ ...library, getDocument(options) {
      // Node fs fetches local fonts/CMaps; browser production uses worker fetch.
      return library.getDocument({ ...options, useWorkerFetch: false });
    } }),
    createWorker() {
      worker = library.PDFWorker.create({ verbosity: 0 });
      return { worker, dispose() { worker.destroy(); disposed++; } };
    },
  });
  return { load: (bytes, options) => load(source(bytes), options), rawLoad: load, disposed: () => disposed, worker: () => worker };
}

test('real PDF.js parses generated text, blank, RGB image and JS-decoded fax with Node fake worker', async () => {
  for (const kind of ['text', 'blank', 'image', 'fax']) {
    const f = await realLoader(), input = syntheticPdf(kind), handle = f.load(input.bytes);
    try {
      const { document, TextLayer } = await handle.promise;
      assert.equal(typeof TextLayer, 'function');
      assert.equal(document.numPages, 1);
      const page = await document.getPage(1);
      const text = await page.getTextContent();
      assert.equal(text.items.map((item) => item.str ?? '').join(''), kind === 'text' ? 'Synthetic PDF sample' : '');
      const operators = await page.getOperatorList();
      if (kind === 'image' || kind === 'fax') {
        assert.ok(operators.fnArray.some((op) => [library.OPS.paintImageXObject, library.OPS.paintInlineImageXObject].includes(op)));
      }
    } finally { await handle.destroy(); }
    assert.equal(f.disposed(), 1); assert.equal(f.worker().destroyed, true);
  }
});

test('real multipage PDF text and operators survive sequential page cleanup and a new view generation', async () => {
  const f = await realLoader(), handle = f.load(syntheticPdf('multipage').bytes);
  try {
    const { document } = await handle.promise;
    assert.equal(document.numPages, 3);
    for (let generation = 0; generation < 2; generation++) {
      await document.cleanup();
      for (let number = 1; number <= document.numPages; number++) {
        const page = await document.getPage(number);
        const operators = await page.getOperatorList();
        assert.ok(operators.fnArray.includes(library.OPS.showText));
        const source = page.streamTextContent().getReader();
        let text = '';
        while (true) {
          const chunk = await source.read();
          if (chunk.done) break;
          text += chunk.value.items.map(item => item.str ?? '').join('');
        }
        assert.ok(text.includes(`Synthetic PDF page ${number} of 3`));
        assert.ok(text.includes('End of generated page.'));
        assert.equal(page.cleanup(), true);
        await document.cleanup(true);
      }
    }
  } finally { await handle.destroy(); }
  assert.equal(f.disposed(), 1); assert.equal(f.worker().destroyed, true);
});

test('real PDF.js password required, incorrect, correct and cancel lifecycle with generated locked bytes', async () => {
  const fixture = syntheticPdf('locked'), f = await realLoader(), reasons = [], prompts = [];
  const handle = f.load(fixture.bytes, { onPassword(prompt) {
    reasons.push(prompt.reason); prompts.push(prompt);
    prompt.submit(reasons.length === 1 ? randomBytes(16).toString('hex') : fixture.password);
  } });
  try {
    const { document } = await handle.promise;
    assert.deepEqual(reasons, ['required', 'incorrect']);
    const content = await (await document.getPage(1)).getTextContent();
    assert.equal(content.items.map((item) => item.str ?? '').join(''), 'Synthetic PDF sample');
    assert.equal(prompts[1].submit(randomBytes(8).toString('hex')), false);
  } finally { await handle.destroy(); }
  const other = await realLoader();
  const cancelled = other.load(fixture.bytes, { onPassword(prompt) { void prompt.cancel(); } });
  await assert.rejects(cancelled.promise, { code: 'cancelled' }); await cancelled.destroy();
  assert.equal(other.disposed(), 1);
  fixture.password = undefined;
});

test('real PDF.js malformed input rejects and releases the Node fake worker', async () => {
  const f = await realLoader();
  const handle = f.load(new TextEncoder().encode('%PDF-1.4\nnot a document\n%%EOF'));
  await assert.rejects(handle.promise, { code: 'invalid' });
  await handle.destroy(); assert.equal(f.disposed(), 1);
});