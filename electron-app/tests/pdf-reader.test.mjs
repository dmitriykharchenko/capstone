import assert from 'node:assert/strict';
import test, { before, after } from 'node:test';
import { createServer } from 'vite';
import { fileURLToPath } from 'node:url';
import { randomBytes } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import ts from 'typescript';
import { syntheticPdf } from './pdf-fixtures.mjs';

let vite, readerModule, pageModule, lifecycle, actions, nativeApi, layoutModule;
const flush = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => Promise.withResolvers();
before(async () => {
  vite = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), appType: 'custom',
    logLevel: 'silent', server: { middlewareMode: true } });
  [readerModule, pageModule, lifecycle, actions, nativeApi, layoutModule] = await Promise.all([
    '/src/lib/pdf-reader.ts', '/src/lib/pdf-page.ts', '/src/lib/pdf-lifecycle.ts',
    '/src/slices/papers/papers-slice.ts', '/src/lib/api.ts', '/src/lib/pdf-layout.ts',
  ].map(path => vite.ssrLoadModule(path)));
});
after(async () => vite.close());

// Deliberately a DOM/paint double, not a browser or evidence of native pixel rendering.
function surface(options = {}) {
  const elements = [];
  const document = { defaultView: { devicePixelRatio: 2 }, createElement(tag) {
    const node = { tag, ownerDocument: document, children: [], attributes: {}, style: { setProperty(k, v) { this[k] = v; } },
      append(...nodes) { for (const n of nodes) { n.remove(); this.children.push(n); n.parent = this; } },
      replaceChildren(...nodes) { this.children = []; this.append(...nodes); },
      remove() { if (this.parent) this.parent.children = this.parent.children.filter(n => n !== this); this.parent = undefined; },
      get parentElement() { return this.parent; },
      setAttribute(k, v) { this.attributes[k] = v; }, clientWidth: 632, clientHeight: 800, scrollTop: 0, scrollLeft: 0,
      listeners: new Map(), addEventListener(name, listener) { this.listeners.set(name, listener); },
      removeEventListener(name) { this.listeners.delete(name); },
      getContext() { if (options.noContext) return null; return { fillRect() {}, clearRect() {},
        getImageData() { if (options.allocationError) throw Error('PRIVATE-ALLOCATION'); return { data: [0, 0, 0, options.blankCanvas ? 0 : 255] }; } }; },
      get offsetTop() { return 16 + (this.parent?.children.slice(0, this.parent.children.indexOf(this)) ?? [])
        .reduce((height, sibling) => height + parseFloat(sibling.style.height) + 16, 0); },
      scrollIntoView(options) { this.scrolled = options; this.scrolls = (this.scrolls ?? 0) + 1; } };
    elements.push(node); return node;
  } };
  const scroll = document.createElement('div'), host = document.createElement('div'); scroll.append(host);
  return { host, scroll, elements };
}
const textItem = (str = 'Generated selectable text', extra = {}) => ({ str, dir: 'ltr', fontName: 'f1',
  transform: [12, 0, 0, 12, 20, 20], width: 120, height: 12, hasEOL: false, ...extra });
const textStyle = { fontFamily: 'sans-serif', vertical: false, ascent: 0.8, descent: -0.2 };
const slots = h => h.host.children[0]?.children ?? [];
const roots = h => slots(h).flatMap(slot => slot.children);
const liveCanvases = h => h.elements.filter(n => n.tag === 'canvas' && n.width > 0);

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
        controller.enqueue(options.chunk ?? { items: options.items ?? [textItem()], styles: options.styles ?? { f1: textStyle } });
      }, cancel() { stats.streamsCancelled++; return options.cancelText?.(); } });
    },
    cleanup() { stats.pageCleanups++; return true; },
  };
  class TextLayer {
    constructor(parameters) { this.parameters = parameters; this.cancelled = 0; stats.layers.push(this); }
    async render() {
        if (options.textError) throw Error('PRIVATE-TEXT');
        for (const item of this.parameters.textContentSource.items) {
          const span = this.parameters.container.ownerDocument.createElement('span');
          span.textContent = item.str;
          this.parameters.container.append(span);
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
  const dom = surface(options), reads = [], loads = [], snapshots = [], prompts = [];
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
  assert.equal(h.state().rendered, 3); assert.equal(slots(h).length, 3);
  for (const [index, root] of roots(h).entries()) {
    assert.equal(root.attributes['data-page-number'], String(index + 1));
    assert.equal(root.style.visibility, 'visible');
    assert.equal(h.stats.layers[index].parameters.container.children[0].textContent, 'Generated selectable text');
    assert.equal(h.stats.paints[index].parameters.canvas.width, 1200);
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
  assert.equal(h.loads.length, 1); assert.deepEqual(h.loads[0].bytes, { bytes: [37, 80, 68, 70] });
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

test('one active render and latest-only zoom/resize work prioritize the destination page', async t => {
  const h = harness(t, { holdPaint: true }); await h.open();
  assert.equal(h.state().phase, 'rendering');
  h.reader.page(2); h.reader.page(999); h.reader.zoom(99); h.reader.zoom(1.5);
  for (let i = 0; i < 200; i++) h.reader.resize(800 + i);
  await flush();
  assert.deepEqual(h.stats.pages, [1, 3]);
  assert.equal(h.state().page, 3); assert.equal(h.state().zoom, 1.5);
  assert.equal(h.stats.paints[0].parameters.canvas.width, 0);
  assert.equal(h.stats.paints[0].cancelled, 1);
  assert.equal(h.stats.layers.length, 0);
  h.stats.paints[0].resolve(); await flush(); assert.equal(h.state().phase, 'rendering');
  for (const index of [1, 2, 3]) { h.stats.paints[index].resolve(); await flush(); }
  assert.equal(h.state().phase, 'ready'); assert.equal(h.state().rendered, 3);
  assert.deepEqual(h.stats.pages, [1, 3, 2, 1]);
  assert.equal(slots(h).length, 3); assert.equal(h.stats.layers.length, 3);
  assert.equal(h.scroll.scrollTop, 2 * (1200 + 16) + 16);
  h.reader.page(-99); h.reader.zoom(-10); await flush();
  assert.equal(h.state().page, 1); assert.equal(h.state().zoom, 0.25);
  const count = h.stats.pages.length; h.reader.page(NaN); h.reader.zoom(Infinity); await flush();
  assert.equal(h.stats.pages.length, count);
});

test('a billion pages have a bounded surface/window and can jump to the end and revisit', async t => {
  const h = harness(t, { pages: 1_000_000_000 }); await h.open();
  const assertBounded = () => {
    assert.ok(slots(h).length <= layoutModule.PDF_ACTIVE_PAGES);
    assert.ok(liveCanvases(h).length <= layoutModule.PDF_ACTIVE_PAGES);
    assert.ok(parseFloat(h.host.children[0].style.height) <= layoutModule.PDF_SCROLL_SPAN);
    assert.equal(h.state().phase, 'ready'); assert.equal(h.state().pages, 1_000_000_000);
  };
  assertBounded(); assert.equal(h.stats.pages.length, 3); assert.equal(h.state().notice, '');
  const old = [...h.stats.paints];
  h.reader.page(1_000_000_000); await flush(); assertBounded();
  assert.equal(h.state().page, 1_000_000_000);
  assert.ok(old.every(p => p.parameters.canvas.width === 0));
  h.reader.page(1); await flush(); assertBounded();
  assert.equal(h.stats.pages.filter(n => n === 1).length, 2);
  assert.equal(h.state().zoom, 1); h.reader.resize(332); await flush();
  assert.equal(h.state().zoom, 0.5); assertBounded();
  h.reader.zoom(2); await flush(); h.reader.resize(632); await flush();
  assert.equal(h.state().zoom, 2); assertBounded(); h.reader.fit(); await flush(); assert.equal(h.state().zoom, 1);
  assertBounded(); assert.ok(h.stats.pages.length < 40);
});

test('scroll entry/exit releases canvases, selectable DOM and shared fonts before deterministic revisit', async t => {
  const h = harness(t, { pages: 1000 }); await h.open();
  const old = [...h.stats.paints], layers = [...h.stats.layers];
  h.reader.scrolled(816 * 50 + 16);
  assert.equal(liveCanvases(h).length, 0);
  assert.ok(layers.every(layer => layer.parameters.container.children.length === 0));
  await flush(); assert.equal(h.state().page, 51);
  assert.ok(h.stats.pages.includes(51)); assert.ok(!h.stats.pages.includes(25));
  assert.ok(old.every(p => p.parameters.canvas.width === 0 && p.parameters.canvas.height === 0));
  assert.ok(h.stats.keepFonts.filter(keep => !keep).length >= 2);
  h.reader.scrolled(0); await flush();
  assert.equal(h.state().page, 1);
  assert.equal(h.stats.pages.filter(page => page === 1).length, 2);
  assert.equal(roots(h)[0].children[1].children[0].children[0].textContent, 'Generated selectable text');
  assert.ok(liveCanvases(h).length <= layoutModule.PDF_ACTIVE_PAGES);
});

test('compressed scrollbar reaches every logical region while local scrolling keeps page-size steps', async t => {
  const h = harness(t, { pages: 1_000_000 }); await h.open();
  const end = layoutModule.PDF_SCROLL_SPAN - h.scroll.clientHeight;
  h.reader.scrolled(end / 2); await flush();
  const page = h.state().page;
  assert.ok(page > 490000 && page < 510000);
  const physical = h.scroll.scrollTop;
  assert.equal(h.reader.scrollBy(400), true); await flush();
  assert.ok(h.state().page >= page && h.state().page <= page + 1);
  const paints = h.stats.paints.length;
  h.reader.scrolled(h.scroll.scrollTop); h.reader.resize(632, 800); await flush();
  assert.equal(h.stats.paints.length, paints);
  h.reader.scrolled(physical + 20); await flush(); assert.ok(h.state().page > page + 1);
  assert.equal(h.reader.scrollKey('PageDown'), true); await flush();
  assert.equal(h.reader.scrollKey('x'), false);
  assert.equal(h.reader.scrollKey('toString'), false);
  h.reader.scrolled(end); await flush(); assert.equal(h.state().page, 1_000_000);
  h.reader.scrolled(0); await flush(); assert.equal(h.state().page, 1);
  h.reader.scrollKey('End'); await flush(); assert.equal(h.state().page, 1_000_000);
  h.reader.scrollKey('Home'); await flush(); assert.equal(h.state().page, 1);
});

test('sparse layout covers all pages and updates mixed geometry without allocating per-page arrays', () => {
  const layout = new layoutModule.PdfLayout(1_000_000_000);
  layout.configure(600, 1, true);
  assert.equal(layout.pageAt(layout.top(987654321)), 987654321);
  layout.measure(5, 300, 1000);
  assert.equal(layout.top(6), 5 * 816 + 1200);
  assert.equal(layout.pageAt(layout.top(6) - 1), 5);
  layout.configure(600, 2, false);
  assert.equal(layout.size(5).height, 2000);
  assert.equal(layout.pageAt(layout.top(999999999)), 999999999);
  assert.ok(layoutModule.pdfWindow(layout, layout.top(20), 100000).length <= layoutModule.PDF_ACTIVE_PAGES);
});

test('late geometry from an offscreen getPage cannot move or publish over the destination window', async t => {
  const pending = deferred();
  const h = harness(t, { pages: 100, getPage: (page, n) => n === 1 ? pending.promise : Promise.resolve(page) }); await h.open();
  h.reader.page(80); h.reader.resize(332); await flush();
  assert.deepEqual(h.stats.pages, [1]);
  pending.resolve(documentDouble({ width: 200, height: 9000 }).page); await flush();
  assert.equal(h.state().page, 80); assert.equal(h.state().zoom, 0.5);
  assert.equal(h.state().phase, 'ready'); assert.equal(h.scroll.scrollTop, 79 * 416 + 16);
  assert.equal(h.stats.paints.some(p => p.parameters.viewport.height === 9000), false);
});

test('tiny pages are not rejected and retain only the bounded active page window', async t => {
  const h = harness(t, { pages: 10000, width: 600, height: 1 }); await h.open();
  assert.equal(h.state().phase, 'ready'); assert.ok(liveCanvases(h).length <= layoutModule.PDF_ACTIVE_PAGES);
  h.reader.page(9000); await flush(); assert.equal(h.state().page, 9000);
  assert.equal(h.state().phase, 'ready'); assert.ok(liveCanvases(h).length <= layoutModule.PDF_ACTIVE_PAGES);
});

test('more than the upstream single-layer text threshold renders every item in uncapped batches', async t => {
  const items = Array.from({ length: 100005 }, (_, n) => textItem(`item-${n}`));
  const h = harness(t, { pages: 1, items }); await h.open();
  assert.equal(h.state().phase, 'ready');
  assert.equal(h.stats.layers.reduce((sum, layer) => sum + layer.parameters.container.children.length, 0), items.length);
  assert.equal(h.stats.layers.at(-1).parameters.container.children.at(-1).textContent, 'item-100004');
  assert.ok(h.stats.layers.every(layer => layer.parameters.textContentSource.items.length <= 4096));
  await h.reader.destroy(); assert.ok(h.stats.layers.every(layer => layer.parameters.container.children.length === 0));
});

test('real PDF.js range-loaded page geometry/text feed the renderer across cleanup and revisit (paint double)', async t => {
  await import('pdfjs-dist/legacy/build/pdf.mjs');
  const library = await import('pdfjs-dist/build/pdf.mjs'), fixture = syntheticPdf('multipage');
  let closed = 0, disposed = 0;
  const load = lifecycle.createPdfLoader({ assetBase: fileURLToPath(new URL('../node_modules/pdfjs-dist/', import.meta.url)),
    loadLibrary: async () => ({ ...library, getDocument: options => library.getDocument({ ...options, useWorkerFetch: false }) }),
    createWorker() { const worker = library.PDFWorker.create({ verbosity: 0 }); return { worker, dispose() { worker.destroy(); disposed++; } }; } });
  const handle = load({ length: fixture.bytes.length, async read(begin, end) { return fixture.bytes.slice(begin, end); }, async close() { closed++; } });
  t.after(() => handle.destroy());
  const loaded = await handle.promise, dom = surface(), doc = documentDouble(), states = [];
  const reader = readerModule.createPdfReader(dom.host, state => states.push(state), () => {}, {
    readPdf: async () => ({}), loadPdf: () => ({ destroy: handle.destroy, promise: Promise.resolve({ TextLayer: doc.loaded.TextLayer,
      document: { numPages: loaded.document.numPages, cleanup: keep => loaded.document.cleanup(keep), async getPage(n) {
        const page = await loaded.document.getPage(n);
        return { userUnit: page.userUnit, getViewport: options => page.getViewport(options),
          streamTextContent: options => page.streamTextContent(options), cleanup: () => page.cleanup(),
          render() { return { promise: page.getOperatorList().then(operators => assert.ok(operators.fnArray.includes(library.OPS.showText))), cancel() {} }; } };
      } },
    }) }), renderPage: pageModule.renderPdfPage,
  });
  t.after(() => reader.destroy());
  const ready = async () => {
    for (let i = 0; i < 200 && states.at(-1)?.phase !== 'ready' && states.at(-1)?.phase !== 'error'; i++) await flush();
    assert.equal(states.at(-1)?.phase, 'ready');
  };
  await reader.open('generated'); await ready();
  assert.equal(roots(dom).length, 3);
  assert.ok(doc.stats.layers.flatMap(layer => layer.parameters.container.children).some(span => span.textContent.includes('Synthetic PDF page 3')));
  const canvases = liveCanvases(dom);
  reader.zoom(2); await flush(); await ready();
  assert.ok(canvases.every(canvas => canvas.width === 0));
  assert.ok(roots(dom).every(root => root.style.width === '1224px'));
  reader.page(3); reader.fit(); await flush(); await ready();
  assert.equal(states.at(-1).page, 3);
  await reader.destroy(); assert.equal(liveCanvases(dom).length, 0);
  assert.equal(closed, 1); assert.equal(disposed, 1);
});

test('page controls and scrolling synchronize without repainting the same small window', async t => {
  const h = harness(t); await h.open();
  const original = roots(h), paints = h.stats.paints.length;
  h.reader.page(3); assert.equal(h.state().page, 3); assert.equal(h.scroll.scrollTop, 1648);
  h.reader.scrolled(832); assert.equal(h.state().page, 2);
  h.reader.scrolled(0); assert.equal(h.state().page, 1);
  h.reader.scrolled(Infinity); assert.equal(h.state().page, 1);
  h.reader.page(999); assert.equal(h.state().page, 3);
  h.reader.page(-99); assert.equal(h.state().page, 1);
  h.reader.page(NaN); await flush();
  assert.deepEqual(roots(h), original); assert.equal(h.stats.paints.length, paints);
  assert.ok(h.stats.layers.every(layer => layer.cancelled === 0));
});

test('mixed page sizes fit individually and report the visible page zoom without repainting', async t => {
  const sizes = [documentDouble({ width: 600 }), documentDouble({ width: 300 }), documentDouble({ width: 800, rotation: 90 })];
  const h = harness(t, { getPage: (_page, n) => Promise.resolve(sizes[n - 1].page) }); await h.open();
  assert.ok(roots(h).every(root => root.style.width === '600px'));
  assert.equal(h.state().zoom, 1);
  h.reader.scrolled(832); assert.equal(h.state().zoom, 2);
  h.reader.page(3); assert.equal(h.state().zoom, 0.75);
  h.reader.zoom(1.5); await flush();
  assert.deepEqual(roots(h).map(root => root.style.width), ['900px', '450px', '1200px']);
  h.reader.page(2); assert.equal(h.state().zoom, 1.5);
  h.reader.resize(332); h.reader.fit(); await flush();
  assert.ok(roots(h).every(root => root.style.width === '300px'));
  assert.equal(h.state().page, 2); assert.equal(h.state().zoom, 1);
  assert.equal(h.scroll.scrollTop, 432);
});

test('page jumps cancel offscreen paint immediately without visiting intermediate pages', async t => {
  const h = harness(t, { pages: 10000, holdPaint: true }); await h.open();
  h.reader.page(9999); h.reader.page(8000); await flush();
  assert.equal(h.state().page, 8000); assert.equal(h.stats.paints.length, 2);
  assert.equal(h.stats.paints[0].cancelled, 1); assert.equal(h.stats.paints[0].parameters.canvas.width, 0);
  assert.deepEqual(h.stats.pages, [1, 8000]);
  h.stats.paints[0].resolve(); await flush(); assert.equal(h.state().rendered, 0);
  h.stats.paints[1].resolve(); await flush(); assert.equal(h.state().rendered, 1);
});

test('a resize storm keeps only the latest fitted generation including after a completed page', async t => {
  const h = harness(t, { holdPaint: true }); await h.open();
  h.stats.paints[0].resolve(); await flush();
  const oldRoot = roots(h)[0];
  for (let i = 0; i < 200; i++) h.reader.resize(433 + i);
  assert.equal(roots(h).length, 0); assert.equal(oldRoot.children.length, 0);
  await flush(); assert.deepEqual(h.stats.pages, [1, 2, 1]);
  assert.equal(h.stats.paints[1].cancelled, 1);
  for (const i of [2, 3, 4]) { h.stats.paints[i].resolve(); await flush(); }
  assert.equal(h.state().phase, 'ready'); assert.equal(h.state().zoom, 1);
  assert.equal(h.state().rendered, 3); assert.equal(slots(h).length, 3);
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
  assert.equal(slots(h).length, 3); assert.equal(h.state().phase, 'ready');
});

test('a page jump during a zoom rebuild cannot reuse the previous generation zoom', async t => {
  const h = harness(t); await h.open();
  h.reader.zoom(2); h.reader.page(2); await flush();
  assert.equal(h.state().zoom, 2); assert.equal(h.state().page, 2);
  assert.ok(roots(h).every(root => root.style.width === '1200px'));
  assert.equal(h.scroll.scrollTop, 1632);
});

for (const [label, options] of [
  ['document text items', { pages: 11, items: Array.from({ length: 10000 }, () => textItem('a')) }],
  ['document text characters', { pages: 6, items: [textItem('a'.repeat(1000001))] }],
  ['document text styles', { pages: 17, styles: { f1: textStyle, ...Object.fromEntries(Array.from({ length: 257 }, (_, n) => [n, textStyle])) } }],
]) {
  test(`${label} has no aggregate cap across window changes`, async t => {
    const h = harness(t, options); await h.open();
    for (let page = 1; page <= options.pages; page++) {
      h.reader.page(page); await flush(); assert.equal(h.state().phase, 'ready');
      assert.ok(liveCanvases(h).length <= layoutModule.PDF_ACTIVE_PAGES);
    }
    await h.reader.destroy();
    assert.equal(liveCanvases(h).length, 0);
    assert.ok(h.stats.layers.every(layer => layer.parameters.container.children.length === 0));
  });
}

test('a later page failure clears earlier selectable pages and publishes only a fixed error', async t => {
  const h = harness(t, { getPage: (page, n) => n === 2 ? Promise.reject(Error('PRIVATE-DETAIL')) : Promise.resolve(page) });
  await h.open(); await flush();
  assert.equal(h.state().phase, 'error'); assert.doesNotMatch(h.state().message, /PRIVATE/);
  assert.equal(h.state().rendered, 0); assert.deepEqual(h.stats.pages, [1, 2]);
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
  assert.equal(h.state().phase, 'ready'); assert.equal(slots(h).length, 3);
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

test('page geometry uses requested device pixels without dimension/pixel caps and validates numbers', () => {
  const view = { zoom: 4, fit: false, width: 1000, pixelRatio: 3 };
  const { page } = documentDouble();
  const result = pageModule.pageGeometry(page, view);
  assert.equal(result.width, 7200); assert.equal(result.height, 9600);
  const wide = pageModule.pageGeometry(documentDouble({ width: 40_000 }).page, view);
  assert.equal(wide.width, 480_000);
  for (const width of [NaN, Infinity, 0, -1]) {
    const { page } = documentDouble({ width });
    assert.throws(() => pageModule.pageGeometry(page, view), { code: 'render' });
  }
  for (const pixelRatio of [NaN, 0, -1, Infinity]) {
    assert.throws(() => pageModule.pageGeometry(page, { ...view, pixelRatio }), { code: 'render' });
  }
  const fit = pageModule.pageGeometry(documentDouble({ width: 40_000 }).page, { ...view, fit: true });
  assert.equal(fit.viewport.width, 1000); assert.equal(fit.zoom, 0.025);
});

for (const [label, options] of [['blank', { items: [] }], ['image-only', { items: [], pages: 1 }]]) {
  test(`${label} pages with no selectable text still complete and clean up`, async t => {
    const h = harness(t, options); await h.open();
    assert.equal(h.state().phase, 'ready'); assert.equal(h.stats.layers.length, 0);
    assert.equal(roots(h)[0].style.visibility, 'visible'); await h.reader.destroy();
    assert.equal(h.host.children.length, 0);
  });
}

for (const [label, options] of [
  ['pages', { pages: 0 }], ['fractional pages', { pages: 1.5 }], ['dimensions', { width: Infinity }],
  ['malformed text item', { items: [{ str: 'a' }] }], ['sparse text items', { items: new Array(2) }],
  ['malformed transform', { items: [textItem('a', { transform: new Array(6) })] }],
  ['missing styles', { chunk: { items: [] } }], ['malformed styles', { styles: { f1: {} } }],
  ['text renderer rejection', { textError: true }], ['missing canvas context', { noContext: true }],
  ['canvas allocation exception', { allocationError: true }], ['unallocated canvas', { blankCanvas: true }],
]) {
  test(`${label} fails with a fixed error and drains the document`, async t => {
    const h = harness(t, options); await h.open(); await flush();
    assert.equal(h.state().phase, 'error'); assert.doesNotMatch(h.state().message, /PRIVATE/);
    assert.equal(h.loads[0].destroyed, 1); assert.equal(h.host.children.length, 0);
    assert.equal(h.elements.filter(n => n.tag === 'canvas' && n.width > 0).length, 0);
  });
}

test('close during text streaming cancels text, clears its DOM/canvas and destroys once', async t => {
  const h = harness(t, { holdText: true }); await h.open();
  const root = roots(h)[0];
  await h.reader.open(null);
  assert.equal(h.stats.streamsCancelled, 1);
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
    assert.equal(roots(h).length, 0);
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
  assert.match(h.state().message, /Reopen the preview/); assert.doesNotMatch(h.state().message, /PRIVATE|64 MiB/);
  const broken = harness(t, { getPage: () => Promise.reject(Error('PRIVATE-DETAIL')) }); await broken.open();
  assert.equal(broken.state().phase, 'error'); assert.doesNotMatch(broken.state().message, /PRIVATE/);
});

test('verified loader integrates local password retry, stale prompts, cancel and malformed bytes', async t => {
  const attempts = [], tasks = []; let disposed = 0;
  const doc = documentDouble();
  const load = lifecycle.createPdfLoader({ assetBase: 'app://research/assets/generated/',
    async loadLibrary() { return { PasswordResponses: { INCORRECT_PASSWORD: 2 }, TextLayer: doc.loaded.TextLayer,
      PDFDataRangeTransport: class {},
      getDocument() { const done = deferred(); const task = { ...done, destroy: async () => done.reject(Error('cancelled')) }; tasks.push(task); return task; },
    }; }, createWorker() { return { worker: {}, dispose() { disposed++; } }; },
  });
  const h = harness(t, { loadPdf: (pdf, options) => load({ length: pdf.bytes[0] === 256 ? 0 : pdf.bytes.length,
    read: async () => new Uint8Array(pdf.bytes), close: async () => {} }, options) });
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
  assert.equal(roots(h)[0].style['--total-scale-factor'], String(h.state().zoom * 2));
  assert.equal(canvas.canvas.style.width, '600px');
});

test('actual component script owns selection, password retry/cancel/input clearing and shutdown', async t => {
  const source = await readFile(new URL('../src/components/PdfViewer.svelte', import.meta.url), 'utf8');
  const script = source.match(/<script lang="ts">([\s\S]*?)<\/script>/)[1];
  const expose = '\nexport const harness = { setNodes(a,b) { surface=a; scroll=b; }, setInput(a) { passwordInput=a; }, state() { return viewState; }, unlock, cancelPassword };';
  const compiled = ts.transpileModule(script + expose, { compilerOptions: { target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.CommonJS } }).outputText;
  const dom = surface(), doc = documentDouble(), mount = [], unmount = [], listeners = new Set(), reads = [], loads = [];
  const events = new Map(), observers = [];
  let state = actions.papersReducer(undefined, { type: 'init' }); let destroyed = 0, submitted = 0, nativeArgs = [];
  const store = { dispatch(action) {
    const old = state.selectedId; state = actions.papersReducer(state, action);
    if (old !== state.selectedId) for (const fn of listeners) fn(state.selectedId);
  } };
  const selector = { subscribe(fn) { listeners.add(fn); fn(state.selectedId); return () => listeners.delete(fn); } };
  const bindings = { exports: {}, $state: Object.assign(value => value, { raw: value => value }), $derived: value => value,
    window: { addEventListener(name, fn) { events.set(name, fn); }, removeEventListener(name) { events.delete(name); } },
    ResizeObserver: class { constructor(callback) { observers.push(callback); } observe() {} disconnect() {} },
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
  dom.scroll.scrollTo = () => {}; component.setNodes(dom.host, dom.scroll);
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
  loads[0].resolve(doc.loaded); await flush(); assert.equal(slots(dom).length, 3);
  const paints = doc.stats.paints.length;
  observers[0](); observers[0](); await flush(); assert.equal(doc.stats.paints.length, paints);
  dom.scroll.listeners.get('wheel')({ deltaMode: 0, deltaY: 10, deltaX: 0, preventDefault() { assert.fail('Small document keeps native scrolling'); } });
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
  assert.equal(dom.scroll.listeners.size, 0);
  observers[0](); await flush(); assert.equal(dom.host.children.length, 0);
  assert.deepEqual(nativeArgs, ['component-generated', 'component-cancel', 'component-shutdown']); assert.equal(state.selectedId, null);
});