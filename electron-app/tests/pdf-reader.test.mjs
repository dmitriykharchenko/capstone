import assert from 'node:assert/strict';
import test, { before, after } from 'node:test';
import { createServer } from 'vite';
import { fileURLToPath } from 'node:url';
import { randomBytes } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import ts from 'typescript';

let vite, readerModule, pageModule, lifecycle, actions, nativeApi;
const flush = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => Promise.withResolvers();
before(async () => {
  vite = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), appType: 'custom',
    logLevel: 'silent', server: { middlewareMode: true } });
  [readerModule, pageModule, lifecycle, actions, nativeApi] = await Promise.all([
    '/src/lib/pdf-reader.ts', '/src/lib/pdf-page.ts', '/src/lib/pdf-lifecycle.ts',
    '/src/slices/papers/papers-slice.ts', '/src/lib/api.ts',
  ].map(path => vite.ssrLoadModule(path)));
});
after(async () => vite.close());

// Deliberately a DOM/paint double, not a browser or evidence of native pixel rendering.
function surface() {
  const elements = [];
  const document = { defaultView: { devicePixelRatio: 2 }, createElement(tag) {
    const node = { tag, ownerDocument: document, children: [], attributes: {}, style: { setProperty(k, v) { this[k] = v; } },
      append(...nodes) { this.children.push(...nodes); for (const n of nodes) n.parent = this; },
      replaceChildren(...nodes) { this.children = []; this.append(...nodes); },
      remove() { if (this.parent) this.parent.children = this.parent.children.filter(n => n !== this); },
      setAttribute(k, v) { this.attributes[k] = v; }, clientWidth: 632,
      get offsetTop() { return 16 + (this.parent?.children.slice(0, this.parent.children.indexOf(this)) ?? [])
        .reduce((height, sibling) => height + parseFloat(sibling.style.height) + 16, 0); },
      scrollIntoView(options) { this.scrolled = options; this.scrolls = (this.scrolls ?? 0) + 1; } };
    elements.push(node); return node;
  } };
  return { host: document.createElement('div'), elements };
}

function documentDouble(options = {}) {
  const stats = { pages: [], paints: [], layers: [], cleanups: 0, keepFonts: [], pageCleanups: 0, streamsCancelled: 0 };
  const page = { userUnit: options.userUnit ?? 1,
    getViewport({ scale }) { const rotated = (options.rotation ?? 0) % 180 !== 0;
      return { scale, rotation: options.rotation ?? 0,
        width: (rotated ? options.height ?? 800 : options.width ?? 600) * scale * this.userUnit,
        height: (rotated ? options.width ?? 600 : options.height ?? 800) * scale * this.userUnit }; },
    render(parameters) {
      const done = deferred();
      const task = { ...done, promise: done.promise, parameters, cancelled: 0, cancel() { this.cancelled++; done.reject(Error('cancelled')); } };
      stats.paints.push(task);
      if (!options.holdPaint) done.resolve();
      return task;
    },
    streamTextContent() {
      let emitted = false;
      return new ReadableStream({ pull(controller) {
        if (options.holdText) return;
        if (emitted) { controller.close(); return; }
        emitted = true;
        controller.enqueue({ items: options.items ?? [{ str: 'Generated selectable text' }], styles: options.styles ?? {} });
      }, cancel() { stats.streamsCancelled++; return options.cancelText?.(); } });
    },
    cleanup() { stats.pageCleanups++; return true; },
  };
  class TextLayer {
    constructor(parameters) { this.parameters = parameters; this.cancelled = 0; stats.layers.push(this); }
    async render() {
      this.reader = this.parameters.textContentSource.getReader();
      while (true) {
        const chunk = await this.reader.read();
        if (chunk.done) break;
        for (const item of chunk.value.items) {
          const span = this.parameters.container.ownerDocument.createElement('span');
          span.textContent = item.str;
          this.parameters.container.append(span);
        }
      }
    }
    cancel() { this.cancelled++; void this.reader?.cancel().catch(() => {}); }
  }
  const loaded = { TextLayer, document: { numPages: options.pages ?? 3,
    getPage(n) { stats.pages.push(n); return options.getPage ? options.getPage(page, n) : Promise.resolve(page); },
    async cleanup(keepFonts = false) { stats.cleanups++; stats.keepFonts.push(keepFonts); await options.cleanup?.(stats.cleanups); },
  } };
  return { loaded, stats, page };
}

function harness(t, options = {}) {
  const dom = surface(), reads = [], loads = [], snapshots = [], prompts = [];
  const doc = documentDouble(options);
  const readPdf = options.readPdf ?? (id => {
    const done = deferred();
    const read = { id, ...done, cancelled: 0 };
    read.promise = Object.assign(done.promise, { cancel() { read.cancelled++; } });
    reads.push(read); return read.promise;
  });
  const loadPdf = options.loadPdf ?? ((bytes, callbacks) => {
    const done = deferred();
    const load = { ...done, bytes, callbacks, destroyed: 0,
      destroy() { if (!this.destroyed++) done.reject(new lifecycle.PdfLoadError('cancelled')); return options.drain ?? Promise.resolve(); } };
    loads.push(load);
    if (!options.holdLoad) done.resolve(doc.loaded);
    return load;
  });
  const reader = readerModule.createPdfReader(dom.host, state => snapshots.push(state), prompt => prompts.push(prompt),
    { readPdf, loadPdf, renderPage: pageModule.renderPdfPage });
  t.after(async () => { const stopped = reader.destroy(); for (const read of reads) read.resolve({ bytes: [] }); await stopped; });
  return { ...dom, ...doc, reader, reads, loads, snapshots, prompts, state: () => snapshots.at(-1),
    async open(id = 'generated') {
      const pending = reader.open(id); await flush();
      reads.at(-1)?.resolve({ bytes: [37, 80, 68, 70] }); await pending; await flush();
    } };
}

test('real open/close reducers drive the component controller without storing bytes or handles', async t => {
  const h = harness(t);
  let state = actions.papersReducer(undefined, { type: 'init' });
  state = actions.papersReducer(state, actions.openPaper('generated'));
  const opened = h.reader.open(state.selectedId); await flush();
  h.reads[0].resolve({ bytes: [37, 80, 68, 70] }); await opened; await flush();
  assert.equal(h.state().phase, 'ready');
  assert.deepEqual(h.stats.pages, [1, 2, 3]);
  assert.equal(h.state().rendered, 3); assert.equal(h.host.children.length, 3);
  for (const [index, root] of h.host.children.entries()) {
    assert.equal(root.attributes['data-page-number'], String(index + 1));
    assert.equal(root.style.visibility, 'visible');
    assert.equal(h.stats.layers[index].parameters.container.children[0].textContent, 'Generated selectable text');
    assert.equal(h.stats.paints[index].parameters.canvas.width * h.stats.paints[index].parameters.canvas.height <= pageModule.PDF_LIMITS.pixels, true);
  }
  assert.deepEqual(h.stats.keepFonts, [false, true, true, true]);
  for (const key of ['pdfUrl', 'pdfLoading', 'pdfError', 'bytes', 'document', 'password']) assert.equal(key in state, false);
  state = actions.papersReducer(state, actions.closePaper()); await h.reader.open(state.selectedId);
  assert.equal(h.state().phase, 'idle'); assert.equal(h.host.children.length, 0);
  assert.ok(h.stats.layers.every(layer => layer.parameters.container.children.length === 0));
  assert.ok(h.stats.paints.every(paint => paint.parameters.canvas.width === 0));
  assert.equal(h.loads[0].destroyed, 1); assert.equal(h.stats.pageCleanups, 3);
});

test('actual native API read cancellation drains before replacement and drops stale bytes', async t => {
  const native = [], original = globalThis.window;
  globalThis.window = { research: { readPdf(id) {
    const result = deferred(); const handle = { id, ...result, done: result.promise, cancelled: 0, cancel() { handle.cancelled++; } };
    native.push(handle); return handle;
  } } };
  t.after(() => { globalThis.window = original; });
  const h = harness(t, { readPdf: nativeApi.api.readPdf });
  const a = h.reader.open('a'); await flush();
  const b = h.reader.open('b'); const c = h.reader.open('c'); await flush();
  assert.equal(native[0].cancelled, 1); assert.equal(native.length, 1);
  native[0].resolve({ bytes: [99] }); await a; await b; await flush();
  assert.deepEqual(native.map(r => r.id), ['a', 'c']);
  native[1].resolve({ bytes: [37, 80, 68, 70] }); await c; await flush();
  assert.equal(h.loads.length, 1); assert.deepEqual(h.loads[0].bytes, [37, 80, 68, 70]);
  await h.reader.destroy();
});

test('switch waits for the old loader destroy and stale load never mounts a page', async t => {
  const drain = deferred(); const h = harness(t, { holdLoad: true, drain: drain.promise });
  const a = h.reader.open('a'); await flush(); h.reads[0].resolve({ bytes: [1] }); await flush();
  const b = h.reader.open('b'); await flush();
  assert.equal(h.loads[0].destroyed, 1); assert.equal(h.reads.length, 1);
  h.loads[0].resolve(h.loaded); await a;
  assert.equal(h.stats.pages.length, 0);
  drain.resolve(); await flush();
  h.reads[1].resolve({ bytes: [2] }); await flush(); h.loads[1].resolve(h.loaded); await b; await flush();
  assert.deepEqual(h.stats.pages, [1, 2, 3]);
});

test('one active render and latest-only zoom/resize work; navigation does not cancel continuous rendering', async t => {
  const h = harness(t, { holdPaint: true }); await h.open();
  assert.equal(h.state().phase, 'rendering');
  h.reader.page(2); h.reader.page(999); h.reader.zoom(99); h.reader.zoom(1.5);
  for (let i = 0; i < 200; i++) h.reader.resize(800 + i);
  await flush();
  assert.deepEqual(h.stats.pages, [1, 1]);
  assert.equal(h.state().page, 3); assert.equal(h.state().zoom, 1.5);
  assert.equal(h.stats.paints[0].parameters.canvas.width, 0);
  assert.equal(h.stats.paints[0].cancelled, 1);
  assert.equal(h.stats.layers.length, 0);
  h.stats.paints[0].resolve(); await flush(); assert.equal(h.state().phase, 'rendering');
  for (const index of [1, 2, 3]) { h.stats.paints[index].resolve(); await flush(); }
  assert.equal(h.state().phase, 'ready'); assert.equal(h.state().rendered, 3);
  assert.deepEqual(h.stats.pages, [1, 1, 2, 3]);
  assert.equal(h.host.children.length, 3); assert.equal(h.stats.layers.length, 3);
  assert.deepEqual(h.host.children[2].scrolled, { block: 'start', inline: 'nearest' });
  h.reader.page(-99); h.reader.zoom(-10); await flush();
  assert.equal(h.state().page, 1); assert.equal(h.state().zoom, 0.25);
  const count = h.stats.pages.length; h.reader.page(NaN); h.reader.zoom(Infinity); await flush();
  assert.equal(h.stats.pages.length, count);
});

test('all 500 permitted pages remain scrollable within a shared pixel budget through zoom and fit', async t => {
  const h = harness(t, { pages: 500 }); await h.open();
  const assertBounded = () => {
    const canvases = h.elements.filter(n => n.tag === 'canvas' && n.width > 0);
    assert.equal(h.host.children.length, 500); assert.equal(canvases.length, 500);
    assert.ok(canvases.reduce((sum, c) => sum + c.width * c.height, 0) <= pageModule.PDF_LIMITS.documentPixels);
    assert.ok(canvases.every(c => c.width * c.height <= pageModule.PDF_LIMITS.pixels));
    assert.equal(h.state().phase, 'ready'); assert.equal(h.state().rendered, 500);
  };
  assertBounded(); assert.match(h.state().notice, /pixel limit/);
  assert.equal(h.state().zoom, 1); h.reader.resize(332); await flush();
  assert.equal(h.state().zoom, 0.5); assertBounded();
  h.reader.zoom(2); await flush(); h.reader.resize(632); await flush();
  assert.equal(h.state().zoom, 2); assertBounded(); h.reader.fit(); await flush(); assert.equal(h.state().zoom, 1);
  assertBounded(); assert.equal(h.stats.pages.length, 2000);
});

test('page controls and scrolling keep every page and text layer without new PDF work', async t => {
  const h = harness(t); await h.open();
  const roots = [...h.host.children], paints = h.stats.paints.length, cleanups = h.stats.cleanups;
  h.reader.page(3); assert.equal(h.state().page, 3); assert.equal(roots[2].scrolls, 1);
  h.reader.scrolled(roots[1].offsetTop); assert.equal(h.state().page, 2);
  h.reader.scrolled(0); assert.equal(h.state().page, 1);
  h.reader.scrolled(Infinity); assert.equal(h.state().page, 1);
  h.reader.page(999); assert.equal(h.state().page, 3);
  h.reader.page(-99); assert.equal(h.state().page, 1);
  h.reader.page(NaN); await flush();
  assert.deepEqual(h.host.children, roots); assert.equal(h.stats.paints.length, paints);
  assert.equal(h.stats.cleanups, cleanups);
  assert.ok(h.stats.layers.every(layer => layer.cancelled === 0));
});

test('mixed page sizes fit individually and report the visible page zoom without repainting', async t => {
  const sizes = [documentDouble({ width: 600 }), documentDouble({ width: 300 }), documentDouble({ width: 800, rotation: 90 })];
  const h = harness(t, { getPage: (_page, n) => Promise.resolve(sizes[n - 1].page) }); await h.open();
  assert.ok(h.host.children.every(root => root.style.width === '600px'));
  assert.equal(h.state().zoom, 1);
  h.reader.scrolled(h.host.children[1].offsetTop); assert.equal(h.state().zoom, 2);
  h.reader.page(3); assert.equal(h.state().zoom, 0.75);
  h.reader.zoom(1.5); await flush();
  assert.deepEqual(h.host.children.map(root => root.style.width), ['900px', '450px', '1200px']);
  h.reader.page(2); assert.equal(h.state().zoom, 1.5);
  h.reader.resize(332); h.reader.fit(); await flush();
  assert.ok(h.host.children.every(root => root.style.width === '300px'));
  assert.equal(h.state().page, 2); assert.equal(h.state().zoom, 1);
  assert.equal(h.host.children[1].scrolls, 1);
});

test('progressive pages paint sequentially and page jumps wait without cancelling current work', async t => {
  const h = harness(t, { holdPaint: true }); await h.open();
  h.reader.page(3); h.reader.scrolled(0);
  assert.equal(h.state().page, 3); assert.equal(h.stats.paints.length, 1);
  h.stats.paints[0].resolve(); await flush();
  assert.equal(h.state().rendered, 1); assert.equal(h.state().phase, 'rendering');
  assert.equal(h.host.children[0].style.visibility, 'visible');
  assert.equal(h.host.children[1].style.visibility, 'hidden');
  h.stats.paints[1].resolve(); await flush();
  assert.equal(h.state().rendered, 2); assert.equal(h.stats.layers.length, 2);
  h.stats.paints[2].resolve(); await flush();
  assert.equal(h.state().phase, 'ready'); assert.equal(h.host.children[2].scrolls, 1);
  assert.ok(h.stats.paints.every(paint => paint.cancelled === 0));
});

test('a resize storm keeps only the latest fitted generation including after a completed page', async t => {
  const h = harness(t, { holdPaint: true }); await h.open();
  h.stats.paints[0].resolve(); await flush();
  const oldRoot = h.host.children[0];
  for (let i = 0; i < 200; i++) h.reader.resize(433 + i);
  assert.equal(h.host.children.length, 0); assert.equal(oldRoot.children.length, 0);
  await flush(); assert.deepEqual(h.stats.pages, [1, 2, 1]);
  assert.equal(h.stats.paints[1].cancelled, 1);
  for (const i of [2, 3, 4]) { h.stats.paints[i].resolve(); await flush(); }
  assert.equal(h.state().phase, 'ready'); assert.equal(h.state().zoom, 1);
  assert.equal(h.state().rendered, 3); assert.equal(h.host.children.length, 3);
  assert.ok(h.stats.paints.slice(0, 2).every(p => p.parameters.canvas.width === 0));
});

test('resize during initial cleanup does not prevent later generation cancellation', async t => {
  const cleanup = deferred();
  const h = harness(t, { holdPaint: true, cleanup: n => n === 1 ? cleanup.promise : undefined }); await h.open();
  h.reader.resize(332); cleanup.resolve(); await flush();
  assert.equal(h.stats.paints[0].parameters.viewport.width, 300);
  h.reader.zoom(2); await flush();
  assert.equal(h.stats.paints[0].cancelled, 1);
  assert.equal(h.stats.paints[0].parameters.canvas.width, 0);
  for (const i of [1, 2, 3]) { h.stats.paints[i].resolve(); await flush(); }
  assert.equal(h.host.children.length, 3); assert.equal(h.state().phase, 'ready');
});

test('a page jump during a zoom rebuild cannot reuse the previous generation zoom', async t => {
  const h = harness(t); await h.open();
  h.reader.zoom(2); h.reader.page(2); await flush();
  assert.equal(h.state().zoom, 2); assert.equal(h.state().page, 2);
  assert.ok(h.host.children.every(root => root.style.width === '1200px'));
  assert.equal(h.host.children[1].scrolls, 1);
});

for (const [label, options, admitted] of [
  ['document text items', { pages: 11, items: Array.from({ length: 10000 }, () => ({ str: 'a' })) }, 10],
  ['document text characters', { pages: 6, items: [{ str: 'a'.repeat(1000000) }] }, 5],
  ['document text styles', { pages: 17, styles: Object.fromEntries(Array.from({ length: 256 }, (_, n) => [n, {}])) }, 16],
]) {
  test(`${label} aggregate limit fails closed after the permitted pages and releases every surface`, async t => {
    const h = harness(t, options); await h.open(); await flush();
    assert.equal(h.state().phase, 'error'); assert.match(h.state().message, /limit/);
    assert.equal(h.state().rendered, admitted); assert.equal(h.stats.pages.length, admitted + 1);
    assert.equal(h.loads[0].destroyed, 1); assert.equal(h.host.children.length, 0);
    assert.ok(h.stats.paints.every(paint => paint.parameters.canvas.width === 0));
    assert.ok(h.stats.layers.every(layer => layer.parameters.container.children.length === 0));
  });
}

test('a later page failure clears earlier selectable pages and publishes only a fixed error', async t => {
  const h = harness(t, { getPage: (page, n) => n === 2 ? Promise.reject(Error('PRIVATE-DETAIL')) : Promise.resolve(page) });
  await h.open(); await flush();
  assert.equal(h.state().phase, 'error'); assert.doesNotMatch(h.state().message, /PRIVATE/);
  assert.equal(h.state().rendered, 1); assert.deepEqual(h.stats.pages, [1, 2]);
  assert.equal(h.loads[0].destroyed, 1); assert.equal(h.host.children.length, 0);
  assert.equal(h.stats.layers[0].parameters.container.children.length, 0);
  assert.equal(h.stats.paints[0].parameters.canvas.width, 0);
});

test('reopen while a later page paints drains all old pages and suppresses late completion', async t => {
  const h = harness(t, { holdPaint: true }); await h.open();
  h.stats.paints[0].resolve(); await flush();
  const reopened = h.reader.open('replacement');
  assert.equal(h.host.children.length, 0);
  await flush(); h.stats.paints[1].resolve(); await flush();
  assert.equal(h.state().phase, 'loading'); assert.equal(h.state().rendered, 0);
  h.reads[1].resolve({ bytes: [1] }); await reopened; await flush();
  for (const i of [2, 3, 4]) { h.stats.paints[i].resolve(); await flush(); }
  assert.equal(h.state().phase, 'ready'); assert.equal(h.host.children.length, 3);
  assert.equal(h.loads[0].destroyed, 1); assert.equal(h.loads[1].destroyed, 0);
  assert.ok(h.stats.paints.slice(0, 2).every(paint => paint.parameters.canvas.width === 0));
});

test('close during inter-page cleanup never starts the next page', async t => {
  const cleanup = deferred(); const h = harness(t, { cleanup: n => n === 2 ? cleanup.promise : undefined }); await h.open();
  assert.deepEqual(h.stats.pages, [1]);
  const closing = h.reader.open(null); cleanup.resolve(); await closing; await flush();
  assert.equal(h.state().phase, 'idle'); assert.deepEqual(h.stats.pages, [1]);
  assert.equal(h.host.children.length, 0); assert.equal(h.loads[0].destroyed, 1);
});

test('rejected inter-page cleanup fails closed even when a newer zoom is pending', async t => {
  const cleanup = deferred(); const h = harness(t, { cleanup: n => n === 2 ? cleanup.promise : undefined }); await h.open();
  h.reader.zoom(2); cleanup.reject(Error('PRIVATE-DETAIL')); await flush();
  assert.equal(h.state().phase, 'error'); assert.match(h.state().message, /release/);
  assert.doesNotMatch(h.state().message, /PRIVATE/); assert.deepEqual(h.stats.pages, [1]);
  assert.equal(h.host.children.length, 0); assert.equal(h.loads[0].destroyed, 1);
});

test('page geometry limits account for device scale, extreme dimensions and user units', () => {
  const view = { zoom: 4, fit: false, width: 1000, pixelRatio: 3 };
  const { page } = documentDouble();
  const result = pageModule.pageGeometry(page, view);
  assert.ok(result.width * result.height <= pageModule.PDF_LIMITS.pixels);
  assert.equal(result.reduced, true);
  for (const width of [NaN, Infinity, 0, -1, 40_000]) {
    const { page } = documentDouble({ width });
    assert.throws(() => pageModule.pageGeometry(page, view), { code: 'limit' });
  }
  for (const pixelLimit of [NaN, 0, -1]) {
    assert.throws(() => pageModule.pageGeometry(page, { ...view, pixelLimit }), { code: 'limit' });
  }
  const clamped = pageModule.pageGeometry(page, { ...view, pixelLimit: Infinity });
  assert.ok(clamped.width * clamped.height <= pageModule.PDF_LIMITS.pixels);
});

for (const [label, options] of [['blank', { items: [] }], ['image-only', { items: [], pages: 1 }]]) {
  test(`${label} pages with no selectable text still complete and clean up`, async t => {
    const h = harness(t, options); await h.open();
    assert.equal(h.state().phase, 'ready'); assert.equal(h.stats.layers[0].parameters.container.children.length, 0);
    assert.equal(h.host.children[0].style.visibility, 'visible'); await h.reader.destroy();
    assert.equal(h.host.children.length, 0);
  });
}

for (const [label, options] of [
  ['pages', { pages: 501 }], ['dimensions', { width: 1e8, height: 1e8 }],
  ['text items', { items: Array.from({ length: 10001 }, () => ({ str: 'a' })) }],
  ['text characters', { items: [{ str: 'a'.repeat(1000001) }] }],
  ['text styles', { styles: Object.fromEntries(Array.from({ length: 257 }, (_, n) => [n, {}])) }],
]) {
  test(`${label} limit is visible and frees the document instead of leaving a blank page`, async t => {
    const h = harness(t, options); await h.open(); await flush();
    assert.equal(h.state().phase, 'error'); assert.match(h.state().message, /limit/);
    assert.equal(h.loads[0].destroyed, 1); assert.equal(h.host.children.length, 0);
    assert.equal(h.elements.filter(n => n.tag === 'canvas' && n.width > 0).length, 0);
  });
}

test('close during text streaming cancels text, clears its DOM/canvas and destroys once', async t => {
  const h = harness(t, { holdText: true }); await h.open();
  const root = h.host.children[0];
  assert.equal(h.stats.layers.length, 1); await h.reader.open(null);
  assert.equal(h.stats.layers[0].cancelled, 1); assert.equal(h.stats.streamsCancelled, 1);
  assert.equal(root.children.length, 0);
  assert.equal(h.host.children.length, 0); assert.equal(h.stats.paints[0].parameters.canvas.height, 0);
  assert.equal(h.loads[0].destroyed, 1); await h.reader.destroy();
  assert.equal(h.loads[0].destroyed, 1);
});

test('zoom waits for text-source cancellation before document cleanup or another render', async t => {
  const cancelled = deferred();
  const h = harness(t, { holdText: true, cancelText: () => cancelled.promise }); await h.open();
  const cleanups = h.stats.cleanups;
  try {
    h.reader.zoom(2); await flush();
    assert.equal(h.stats.streamsCancelled, 1);
    assert.deepEqual(h.stats.pages, [1]); assert.equal(h.stats.cleanups, cleanups);
    assert.equal(h.host.children.length, 0);
  } finally { cancelled.resolve(); }
  await flush();
  assert.deepEqual(h.stats.pages, [1, 1]);
});

test('rejected text cancellation fails closed and cannot start a replacement page', async t => {
  const h = harness(t, { holdText: true, cancelText: () => Promise.reject(Error('PRIVATE-DETAIL')) }); await h.open();
  h.reader.zoom(2); await flush();
  assert.equal(h.state().phase, 'error'); assert.match(h.state().message, /release/);
  assert.doesNotMatch(h.state().message, /PRIVATE/);
  assert.deepEqual(h.stats.pages, [1]); assert.equal(h.loads[0].destroyed, 1);
  assert.equal(h.host.children.length, 0);
});

test('closing during document cleanup prevents late cleanup from starting a page', async t => {
  const cleanup = deferred(); const h = harness(t, { cleanup: () => cleanup.promise }); await h.open();
  const closing = h.reader.open(null);
  cleanup.resolve(); await closing; await flush();
  assert.equal(h.state().phase, 'idle'); assert.deepEqual(h.stats.pages, []);
  assert.equal(h.loads[0].destroyed, 1);
});

test('late getPage after close never mounts', async t => {
  const pending = deferred(); const h = harness(t, { getPage: () => pending.promise }); await h.open();
  const closing = h.reader.open(null); await flush();
  pending.resolve(h.page); await closing; await flush();
  assert.equal(h.host.children.length, 0); assert.equal(h.stats.pageCleanups, 1);
  assert.equal(h.stats.paints.length, 0); assert.equal(h.state().phase, 'idle');
});

test('stalled page teardown fails closed without overlapping another page', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const pending = deferred(); const h = harness(t, { getPage: () => pending.promise }); await h.open();
  h.reader.zoom(2); await flush(); t.mock.timers.tick(1001); await flush();
  assert.equal(h.state().phase, 'error'); assert.match(h.state().message, /release/);
  assert.equal(h.loads[0].destroyed, 1); assert.deepEqual(h.stats.pages, [1]);
  pending.resolve(h.page); await flush(); assert.equal(h.stats.pageCleanups, 1);
  assert.equal(h.host.children.length, 0);
});

test('native/read and page errors expose fixed messages, not raw details', async t => {
  const h = harness(t);
  const opening = h.reader.open('bad'); await flush(); h.reads[0].reject(Error('PRIVATE-DETAIL')); await opening;
  assert.match(h.state().message, /64 MiB/); assert.doesNotMatch(h.state().message, /PRIVATE/);
  const broken = harness(t, { getPage: () => Promise.reject(Error('PRIVATE-DETAIL')) }); await broken.open();
  assert.equal(broken.state().phase, 'error'); assert.doesNotMatch(broken.state().message, /PRIVATE/);
});

test('verified loader integrates local password retry, stale prompts, cancel and malformed bytes', async t => {
  const attempts = [], tasks = []; let disposed = 0;
  const doc = documentDouble();
  const load = lifecycle.createPdfLoader({ assetBase: 'app://research/assets/generated/',
    async loadLibrary() { return { PasswordResponses: { INCORRECT_PASSWORD: 2 }, TextLayer: doc.loaded.TextLayer,
      getDocument() { const done = deferred(); const task = { ...done, destroy: async () => done.reject(Error('cancelled')) }; tasks.push(task); return task; },
    }; }, createWorker() { return { worker: {}, dispose() { disposed++; } }; },
  });
  const h = harness(t, { loadPdf: load });
  const opening = h.reader.open('locked'); await flush(); h.reads[0].resolve({ bytes: [1] }); await flush();
  const send = value => { attempts.push(typeof value); };
  tasks[0].onPassword(send, 1);
  const first = h.prompts.at(-1); assert.equal(first.reason, 'required');
  assert.equal(first.submit(randomBytes(16).toString('hex')), true);
  assert.equal(h.prompts.at(-1), undefined);
  assert.equal(first.submit(randomBytes(16).toString('hex')), false);
  tasks[0].onPassword(send, 2);
  const retry = h.prompts.at(-1); assert.equal(retry.reason, 'incorrect'); await first.cancel();
  assert.equal(h.state().phase, 'password');
  retry.submit(randomBytes(16).toString('hex')); tasks[0].resolve(doc.loaded.document);
  await opening; await flush(); assert.equal(h.state().phase, 'ready');
  assert.deepEqual(attempts, ['string', 'string']);
  assert.equal(h.snapshots.some(s => 'password' in s), false);
  const cancelled = h.reader.open('cancel'); await flush(); h.reads[1].resolve({ bytes: [1] }); await flush();
  tasks[1].onPassword(send, 1); const prompt = h.prompts.at(-1); await prompt.cancel(); await cancelled;
  assert.equal(h.state().phase, 'cancelled'); assert.equal(h.prompts.at(-1), undefined); assert.equal(disposed, 2);
  const bad = h.reader.open('invalid'); await flush(); h.reads[2].resolve({ bytes: [256] }); await bad;
  assert.equal(h.state().phase, 'error'); assert.equal(h.state().message, 'PDF is invalid or unsupported.');
  assert.equal(tasks.length, 2);
});

test('unmount/shutdown destroys active render and ignores every later result', async t => {
  const h = harness(t, { holdPaint: true }); await h.open();
  const count = h.snapshots.length; await h.reader.destroy();
  h.stats.paints[0].resolve(); await h.reader.open('late'); h.reader.zoom(2); await flush();
  assert.equal(h.snapshots.length, count); assert.equal(h.loads[0].destroyed, 1);
  assert.equal(h.host.children.length, 0); assert.equal(h.reads.length, 1);
});

test('a fresh component waits for the unmounted reader native read to drain', async t => {
  const first = harness(t);
  const opening = first.reader.open('old'); await flush();
  const stopped = first.reader.destroy();
  const second = harness(t);
  const replacement = second.reader.open('new'); await flush();
  assert.equal(first.reads[0].cancelled, 1); assert.equal(second.reads.length, 0);
  first.reads[0].resolve({ bytes: [1] }); await stopped; await opening; await flush();
  assert.equal(first.loads.length, 0); assert.equal(second.reads.length, 1);
  second.reads[0].resolve({ bytes: [2] }); await replacement; await flush();
  assert.equal(second.state().phase, 'ready');
});

test('resolved-after-destroy loader cannot publish a borrowed stale document', async t => {
  const ready = deferred(); let destroyed = 0;
  const h = harness(t, { loadPdf: () => ({ promise: ready.promise, destroy: async () => { destroyed++; } }) });
  const opening = h.reader.open('stale'); await flush(); h.reads[0].resolve({ bytes: [1] }); await flush();
  await h.reader.open(null); ready.resolve(h.loaded); await opening; await flush();
  assert.equal(destroyed, 1); assert.equal(h.state().phase, 'idle'); assert.equal(h.stats.pages.length, 0);
});

test('rotated user-unit page shares the exact viewport and text CSS scale with its canvas', async t => {
  const h = harness(t, { rotation: 90, userUnit: 2 }); await h.open();
  const canvas = h.stats.paints[0].parameters, text = h.stats.layers[0].parameters;
  assert.equal(canvas.viewport, text.viewport); assert.equal(canvas.viewport.rotation, 90);
  assert.equal(h.state().zoom, 600 / 1600);
  assert.equal(h.host.children[0].style['--total-scale-factor'], String(h.state().zoom * 2));
  assert.equal(canvas.canvas.style.width, '600px');
});

test('actual component script owns selection, password retry/cancel/input clearing and shutdown', async t => {
  const source = await readFile(new URL('../src/components/PdfViewer.svelte', import.meta.url), 'utf8');
  const script = source.match(/<script lang="ts">([\s\S]*?)<\/script>/)[1];
  const expose = '\nexport const harness = { setNodes(a,b) { surface=a; scroll=b; }, setInput(a) { passwordInput=a; }, state() { return viewState; }, unlock, cancelPassword };';
  const compiled = ts.transpileModule(script + expose, { compilerOptions: { target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.CommonJS } }).outputText;
  const dom = surface(), doc = documentDouble(), mount = [], unmount = [], listeners = new Set(), reads = [], loads = [];
  const events = new Map();
  let state = actions.papersReducer(undefined, { type: 'init' }); let destroyed = 0, submitted = 0, nativeArgs = [];
  const store = { dispatch(action) {
    const old = state.selectedId; state = actions.papersReducer(state, action);
    if (old !== state.selectedId) for (const fn of listeners) fn(state.selectedId);
  } };
  const selector = { subscribe(fn) { listeners.add(fn); fn(state.selectedId); return () => listeners.delete(fn); } };
  const bindings = { exports: {}, $state: Object.assign(value => value, { raw: value => value }), $derived: value => value,
    window: { addEventListener(name, fn) { events.set(name, fn); }, removeEventListener(name) { events.delete(name); } },
    ResizeObserver: class { observe() {} disconnect() {} },
    require(id) {
      if (id === 'svelte') return { onMount: fn => mount.push(fn), onDestroy: fn => unmount.push(fn) };
      if (id.endsWith('/store')) return { store };
      if (id.endsWith('papers-slice')) return actions;
      if (id.endsWith('papers-selectors')) return { selectSelectedPaperId: () => selector };
      if (id.endsWith('pdf-page')) return pageModule;
      if (id.endsWith('pdf-reader')) return { ...readerModule, createPdfReader(host, changed, password) {
        return readerModule.createPdfReader(host, changed, password, {
          readPdf(id) { nativeArgs.push(id); const read = deferred(); reads.push(read); return read.promise; },
          loadPdf(_bytes, options) { const load = deferred(); loads.push({ ...load, options });
            return { promise: load.promise, async destroy() { destroyed++; load.reject(Error('cancelled')); } }; },
          renderPage: pageModule.renderPdfPage,
        });
      } };
      if (id.endsWith('.css')) return {};
      throw Error('Unexpected component import');
    },
  };
  vm.runInNewContext(compiled, bindings);
  const component = bindings.exports.harness;
  component.setNodes(dom.host, { clientWidth: 632, scrollTo() {} });
  const cleanup = mount[0]();
  t.after(() => { cleanup(); unmount.forEach(fn => fn()); });
  store.dispatch(actions.openPaper('component-generated')); await flush();
  reads[0].resolve({ bytes: [1] }); await flush();
  loads[0].options.onPassword({ reason: 'required', submit() { submitted++; return true; }, cancel: async () => {} });
  const input = { value: randomBytes(16).toString('hex') }; component.setInput(input);
  component.unlock({ preventDefault() {} }); assert.equal(input.value, ''); assert.equal(submitted, 1);
  loads[0].options.onPassword({ reason: 'incorrect', submit() { submitted++; return true; }, cancel: async () => {} });
  assert.equal(component.state().phase, 'password');
  input.value = randomBytes(16).toString('hex'); component.unlock({ preventDefault() {} });
  assert.equal(input.value, ''); assert.equal(submitted, 2);
  loads[0].resolve(doc.loaded); await flush(); assert.equal(dom.host.children.length, 3);
  store.dispatch(actions.openPaper('component-cancel')); await flush();
  reads[1].resolve({ bytes: [2] }); await flush();
  loads[1].options.onPassword({ reason: 'required', submit() { submitted++; return true; }, cancel: async () => {} });
  input.value = randomBytes(16).toString('hex'); component.cancelPassword(); await flush();
  assert.equal(input.value, ''); assert.equal(component.state().phase, 'cancelled'); assert.equal(submitted, 2);
  store.dispatch(actions.openPaper('component-shutdown')); await flush();
  reads[2].resolve({ bytes: [3] }); await flush();
  loads[2].options.onPassword({ reason: 'required', submit() { submitted++; return true; }, cancel: async () => {} });
  input.value = randomBytes(16).toString('hex'); events.get('beforeunload')(); await flush();
  assert.equal(input.value, ''); assert.equal(destroyed, 3); assert.equal(submitted, 2);
  cleanup(); unmount.forEach(fn => fn()); await flush();
  assert.equal(destroyed, 3); assert.equal(listeners.size, 0); assert.equal(dom.host.children.length, 0);
  assert.equal(events.size, 0);
  assert.deepEqual(nativeArgs, ['component-generated', 'component-cancel', 'component-shutdown']); assert.equal(state.selectedId, null);
});