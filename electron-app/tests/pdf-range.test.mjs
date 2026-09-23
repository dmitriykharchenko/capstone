import assert from 'node:assert/strict';
import test, { before, after } from 'node:test';
import { createServer } from 'vite';
import { fileURLToPath } from 'node:url';
import { mkdtemp, mkdir, open, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Backend } from '../electron/backend.mjs';
import { Broker } from '../electron/broker.mjs';
import { sparsePdf } from './fixtures/sparse-pdf.mjs';

let vite, rangeModule, lifecycle;
const flush = () => new Promise(resolve => setImmediate(resolve));
before(async () => {
  vite = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), appType: 'custom', logLevel: 'silent', server: { middlewareMode: true } });
  [rangeModule, lifecycle] = await Promise.all(['/src/lib/pdf-range.ts', '/src/lib/pdf-lifecycle.ts'].map(path => vite.ssrLoadModule(path)));
});
after(async () => vite.close());

test('public coalesced range over 64MiB uses bounded sequential bridge reads, no eager fetch', async () => {
  let reads = 0, closed = 0, failed = 0;
  const delivered = Promise.withResolvers(), length = 65 * 1024 * 1024 + 1;
  class PDFDataRangeTransport { onDataRange(begin, bytes) { delivered.resolve({ begin, length: bytes.length, last: bytes.at(-1) }); } }
  const session = rangeModule.pdfRangeTransport({ PDFDataRangeTransport }, { length,
    async read(begin, end) { reads++; assert.ok(end - begin <= 65536); return new Uint8Array(end - begin).fill(9); },
    async close() { closed++; } }, () => failed++);
  assert.equal(reads, 0);
  session.transport.requestDataRange(0, length);
  assert.deepEqual(await delivered.promise, { begin: 0, length, last: 9 });
  assert.equal(reads, Math.ceil(length / 65536)); assert.equal(failed, 0);
  await session.release(); await session.release(); assert.equal(closed, 1);
});

test('queued ranges are bounded and closing cancels active IO and suppresses late callbacks', async () => {
  const pending = Promise.withResolvers(); let cancelled = 0, closed = 0, failed = 0, delivered = 0;
  class PDFDataRangeTransport { onDataRange() { delivered++; } }
  let session;
  session = rangeModule.pdfRangeTransport({ PDFDataRangeTransport }, { length: 1000,
    read() { return Object.assign(pending.promise, { cancel() { cancelled++; } }); },
    async close() { closed++; } }, () => { failed++; void session.release(); });
  for (let i = 0; i < rangeModule.PDF_PENDING_RANGES + 1; i++) session.transport.requestDataRange(i, i + 1);
  assert.equal(failed, 1); assert.equal(cancelled, 1); assert.equal(closed, 1);
  pending.resolve(new Uint8Array(1)); await flush();
  assert.equal(delivered, 0); session.transport.requestDataRange(0, 1); assert.equal(delivered, 0);
});

test('real sparse >64MiB PDF begins loading through actual Rust, broker and public PDF.js ranges', async () => {
  await import('pdfjs-dist/legacy/build/pdf.mjs');
  const library = await import('pdfjs-dist/build/pdf.mjs');
  const temp = await mkdtemp(join(tmpdir(), 'pdf-range-fixture-'));
  const papers = join(temp, 'papers'), data = join(temp, 'data');
  await mkdir(papers); await mkdir(data);
  const fixture = sparsePdf(), file = await open(join(papers, 'large.pdf'), 'wx');
  const backend = new Backend({ executable: fileURLToPath(new URL('../backend/target/debug/research-backend', import.meta.url)),
    env: { CAPSTONE_PAPERS_DIR: papers, CAPSTONE_APP_DATA_ROOT: data, CAPSTONE_RESOURCES_DIR: papers } });
  const broker = new Broker(backend); let sequence = 0, readBytes = 0, ranges = 0, closed = 0, disposed = 0;
  const run = async (method, args) => {
    const result = await broker.run(1, { token: `s${++sequence}`, method, args }, () => {});
    assert.equal(result.ok, true, result.error); return result.data;
  };
  let handle;
  try {
    await file.truncate(fixture.length);
    for (const [offset, bytes] of fixture.parts) await file.write(bytes, 0, bytes.length, offset);
    await file.close();
    const info = await run('readPdf', { paper_id: 'large' });
    assert.equal(info.total_bytes, fixture.length); assert.equal(info.bytes, undefined);
    const load = lifecycle.createPdfLoader({ assetBase: fileURLToPath(new URL('../node_modules/pdfjs-dist/', import.meta.url)),
      loadLibrary: async () => ({ ...library, getDocument: options => library.getDocument({ ...options, useWorkerFetch: false }) }),
      createWorker() { const worker = library.PDFWorker.create({ verbosity: 0 }); return { worker, dispose() { worker.destroy(); disposed++; } }; } });
    handle = load({ length: info.total_bytes,
      async read(begin, end) { assert.ok(end - begin <= 65536); ranges++; const result = await run('readPdfRange', { preview_id: info.preview_id, begin, end }); readBytes += result.bytes.length; return new Uint8Array(result.bytes); },
      async close() { closed++; await run('closePdf', { preview_id: info.preview_id }); } });
    const loaded = await handle.promise;
    assert.equal(loaded.document.numPages, 1);
    assert.ok(readBytes < 1024 * 1024); assert.ok(ranges >= 2);
    await handle.destroy(); assert.equal(closed, 1); assert.equal(disposed, 1);
    assert.equal(broker.previews.sessions.size, 0); assert.equal(broker.previews.pending.size, 0);
    await backend.stop(); assert.equal(backend.pending.size, 0); assert.equal(backend.child, null);
  } finally { await file.close().catch(() => {}); await handle?.destroy(); broker.cancelOwner(1); await backend.stop(); await rm(temp, { recursive: true, force: true }); }
});