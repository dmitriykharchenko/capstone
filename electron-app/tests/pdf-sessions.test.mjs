import assert from 'node:assert/strict';
import test from 'node:test';
import { randomUUID } from 'node:crypto';
import { Broker } from '../electron/broker.mjs';
import { MAX_PDF_SESSIONS, MAX_PDF_PENDING, PDF_IDLE_MS } from '../electron/pdf-sessions.mjs';

const flush = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
function harness(t) {
  const jobs = [];
  const backend = { request(command, args, options = {}) {
    const done = Promise.withResolvers();
    const job = { command, args, options, ...done, done: done.promise, cancelled: 0,
      cancel() { job.cancelled++; done.reject(Object.assign(Error('private detail'), { code: 'cancelled' })); } };
    jobs.push(job); return job;
  } };
  const broker = new Broker(backend);
  t.after(() => broker.cancelOwner(1));
  let token = 0;
  const run = (method, args = {}, owner = 1) => broker.run(owner, { token: `t${++token}`, method, args }, () => {});
  async function open(owner = 1) {
    const opening = run('readPdf', { paper_id: 'p' }, owner);
    const job = jobs.at(-1), preview_id = randomUUID();
    job.options.onEvent('pdf_open', { id: 'p', filename: 'p.pdf', mime_type: 'application/pdf',
      preview_id, total_bytes: 100 * 1024 * 1024 });
    const result = await opening;
    assert.equal(result.ok, true); assert.equal(result.data.bytes, undefined);
    return { job, id: preview_id };
  }
  return { jobs, broker, run, open };
}

test('opaque ownership never lets foreign or unknown IDs read/close another preview', async t => {
  const h = harness(t), { id, job } = await h.open();
  for (const method of ['readPdfRange', 'closePdf']) {
    const args = method === 'readPdfRange' ? { preview_id: id, begin: 0, end: 1 } : { preview_id: id };
    assert.equal((await h.run(method, args, 2)).ok, false);
    assert.equal(job.cancelled, 0);
  }
  assert.equal((await h.run('readPdfRange', { preview_id: randomUUID(), begin: 0, end: 1 })).ok, false);
  assert.equal((await h.run('closePdf', { preview_id: id })).ok, true);
  assert.equal(job.cancelled, 1); assert.equal(h.broker.previews.sessions.size, 0);
  assert.equal((await h.run('readPdfRange', { preview_id: id, begin: 0, end: 1 })).ok, false);
});

test('malformed range parameters close the owned lease without sending native IO', async t => {
  for (const change of [{ begin: -1 }, { end: 0 }, { end: 65537 }, { end: 200 * 1024 * 1024 },
    { begin: 100 * 1024 * 1024, end: 100 * 1024 * 1024 + 1 },
    { begin: 0.5 }, { end: Number.MAX_SAFE_INTEGER + 1 }, { path: '/private' }, { begin: '0' }]) {
    const h = harness(t), { id, job } = await h.open();
    const result = await h.run('readPdfRange', { preview_id: id, begin: 0, end: 1, ...change });
    assert.equal(result.ok, false); assert.equal(result.error, 'PDF is invalid or unsupported.');
    assert.equal(h.jobs.length, 1); assert.equal(job.cancelled, 1);
  }
});

test('wrong identity, offsets, short/sparse/oversized bytes and extra fields fail closed', async t => {
  for (const change of [{ preview_id: randomUUID() }, { offset: 1 }, { bytes: [] }, { bytes: [256] },
    { bytes: new Array(1) }, { bytes: [1, 2] }, { bytes: Array(65537).fill(0) }, { path: '/private' }]) {
    const h = harness(t), { id, job } = await h.open();
    const reading = h.run('readPdfRange', { preview_id: id, begin: 0, end: 1 });
    h.jobs.at(-1).resolve({ preview_id: id, offset: 0, bytes: [1], ...change });
    const result = await reading;
    assert.equal(result.ok, false); assert.doesNotMatch(result.error, /private/);
    assert.equal(job.cancelled, 1); assert.equal(h.broker.previews.pending.size, 0);
  }
});

test('sessions and pending work are bounded and owner loss cancels every owned request', async t => {
  const h = harness(t), sessions = [];
  for (let i = 0; i < MAX_PDF_SESSIONS; i++) sessions.push(await h.open());
  assert.equal((await h.run('readPdf', { paper_id: 'p' })).ok, false);
  const reads = [];
  for (let i = 0; i < MAX_PDF_PENDING; i++) reads.push(h.run('readPdfRange', { preview_id: sessions[0].id, begin: i, end: i + 1 }));
  assert.equal(h.broker.previews.pending.size, MAX_PDF_PENDING);
  assert.equal((await h.run('readPdfRange', { preview_id: sessions[0].id, begin: 0, end: 1 })).ok, false);
  h.broker.cancelOwner(1);
  await Promise.all(reads);
  assert.equal(h.broker.previews.sessions.size, 0); assert.equal(h.broker.previews.pending.size, 0);
  assert.ok(h.jobs.every(job => job.cancelled === 1));
});

test('idle expiry and backend loss invalidate metadata and cancel in-flight reads', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const h = harness(t), first = await h.open();
  t.mock.timers.tick(PDF_IDLE_MS); await flush();
  assert.equal(first.job.cancelled, 1); assert.equal(h.broker.previews.sessions.size, 0);
  const second = await h.open();
  const reading = h.run('readPdfRange', { preview_id: second.id, begin: 0, end: 1 });
  second.job.reject(Object.assign(Error('private detail'), { code: 'pdf_changed' }));
  assert.equal((await reading).ok, false);
  assert.equal(h.broker.previews.sessions.size, 0); assert.equal(h.broker.previews.pending.size, 0);
});