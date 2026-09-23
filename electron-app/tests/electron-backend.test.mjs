import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { mkdtemp, cp, rm, mkdir, writeFile, open, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { Backend, MAX_FRAME_BYTES } from '../electron/backend.mjs';
import { Broker } from '../electron/broker.mjs';
import { pdf } from './fixtures/pdf.mjs';

const fixture = fileURLToPath(new URL('./fixtures/backend-fixture.mjs', import.meta.url));
const settle = () => new Promise((resolve) => setImmediate(resolve));
function backend(t, scenario = 'normal', options = {}) {
  const value = new Backend({ executable: process.execPath, args: [fixture, scenario], env: {}, ...options });
  t.after(() => value.stop());
  return value;
}

test('multiplexes requests through one child and preserves snake_case/Unicode arguments', async (t) => {
  const value = backend(t, 'fragmented');
  const responses = await Promise.all(Array.from({ length: 10 }, (_, i) => value.request('echo', { session_id: `s${i}`, message: 'λ研究' }).done));
  assert.equal(new Set(responses.map((v) => v.pid)).size, 1);
  assert.equal(responses[2].params.session_id, 's2');
  assert.equal(responses[2].params.message, 'λ研究');
  assert.equal(value.pending.size, 0);
});

test('rotates only when idle, waits for close, never overlaps children', async (t) => {
  let live = 0, max = 0, starts = 0;
  const value = backend(t, 'normal', { rotateAfter: 3, spawnChild(...args) {
    starts++; live++; max = Math.max(max, live);
    const child = spawn(...args); child.on('close', () => live--); return child;
  } });
  const pids = [];
  for (let i = 0; i < 8; i++) pids.push((await value.request('echo', {}).done).pid);
  assert.equal(starts, 3);
  assert.equal(max, 1);
  assert.equal(new Set(pids).size, 3);
});

test('rejects excess work without unbounded queue, supports cooperative cancellation and cleanup', async (t) => {
  const value = backend(t);
  const requests = Array.from({ length: 32 }, () => value.request('hold', {}));
  const outcomes = requests.map((request) => assert.rejects(request.done, /cancelled/));
  await settle();
  await assert.rejects(value.request('health', {}).done, /busy/);
  for (const request of requests) request.cancel();
  await Promise.all(outcomes);
  await value.stop(false);
  assert.equal(value.pending.size, 0);
  assert.equal(value.acks.size, 0);
  assert.equal((await value.request('health', {}).done).ok, true);
});

test('cancellation before startup does not send or replay a mutation', async (t) => {
  const value = backend(t);
  const request = value.request('reset_all_indices', {});
  request.cancel();
  await assert.rejects(request.done, /cancelled/);
  assert.equal(value.count, 0);
});

test('preview-only ranges exceed the general request ledger without rotation and never restart stale leases', async t => {
  let starts = 0;
  const value = backend(t, 'normal', { rotateAfter: 1, spawnChild(...args) { starts++; return spawn(...args); } });
  const broker = new Broker(value);
  const opened = await broker.run(1, { token: 'open', method: 'readPdf', args: { paper_id: 'p' } }, () => {});
  assert.equal(opened.ok, true);
  const pid = value.child.pid;
  for (let i = 0; i < 4097; i++) {
    const read = await broker.run(1, { token: `range${i}`, method: 'readPdfRange',
      args: { preview_id: opened.data.preview_id, begin: 0, end: 1 } }, () => {});
    assert.equal(read.ok, true); assert.deepEqual(read.data.bytes, [37]);
  }
  assert.equal(value.count, 1); assert.equal(value.previewSequence, 4097n);
  assert.equal(value.child.pid, pid); assert.equal(starts, 1); assert.equal(value.pending.size, 1);
  broker.cancelOwner(1); await value.stop(false);
  await assert.rejects(value.request('read_pdf_range', { preview_id: opened.data.preview_id, begin: 0, end: 1 }).done, /closed/);
  assert.equal(starts, 1); assert.equal(broker.previews.sessions.size, 0);
});

for (const scenario of ['out-of-order', 'wrong-id', 'missing-data', 'oversized', 'invalid-utf8', 'truncated', 'crash']) {
  test(`rejects and cleans up ${scenario} child output without leaking diagnostics`, { timeout: 5000 }, async (t) => {
    const value = backend(t, scenario, { requestTimeout: 1500 });
    const result = await value.request('health', {}).done.catch((error) => error);
    assert.ok(result instanceof Error);
    assert.doesNotMatch(result.message, /PRIVATE|fixture|Users|Bearer/);
    assert.equal(value.pending.size, 0);
  });
}

test('safe errors ignore backend message strings and do not replay failed mutations', async (t) => {
  const value = backend(t, 'raw-error');
  await assert.rejects(value.request('reset_all_indices', {}).done, (error) => error.message === 'Backend operation failed.');
  assert.equal(value.count, 1);
});

test('missing cancel acknowledgment retires via EOF and lost output is unknown, not cancelled', { timeout: 5000 }, async (t) => {
  const value = backend(t, 'ignore-cancel', { cancelTimeout: 20 });
  const request = value.request('hold', {});
  const rejected = assert.rejects(request.done, /outcome is unknown/);
  await settle();
  const child = value.child;
  const closed = new Promise((resolve) => child.once('close', resolve));
  request.cancel();
  await rejected;
  await closed;
  assert.equal(value.child, null);
  assert.equal(value.pending.size, 0);
  assert.equal(value.acks.size, 0);
  assert.equal(child.killed, false);
  assert.equal((await value.request('health', {}).done).ok, true);
});

test('oversized request is rejected and all pending work is settled', async (t) => {
  const value = backend(t);
  await assert.rejects(value.request('echo', { message: 'x'.repeat(MAX_FRAME_BYTES) }).done);
  assert.equal(value.pending.size, 0);
});

test('broker opens pathless PDF ranges and adapts ordered chat/index events only at terminal result', async (t) => {
  const broker = new Broker(backend(t));
  const events = [];
  const send = (id, event, seq) => { events.push(event); broker.acknowledge(1, id, seq); };
  const pdf = await broker.run(1, { token: 'p', method: 'readPdf', args: { paper_id: 'paper' } }, send);
  assert.equal(pdf.data.bytes, undefined);
  const range = await broker.run(1, { token: 'r', method: 'readPdfRange', args: { preview_id: pdf.data.preview_id, begin: 0, end: 4 } }, send);
  assert.deepEqual(range.data.bytes, [37, 80, 68, 70]);
  await broker.run(1, { token: 'close', method: 'closePdf', args: { preview_id: pdf.data.preview_id } }, send);
  assert.equal(events.length, 0);
  assert.equal((await broker.run(1, { token: 'c', method: 'chatStream', args: { session_id: 's', turn_id: 'turn', message: '?' } }, send)).ok, true);
  assert.deepEqual(events.map((event) => event.event), ['delta', 'delta', 'done']);
  assert.equal(events.at(-1).chatMode, 'native');
  assert.equal(events.at(-1).turnId, 'turn');
  assert.equal(events.at(-1).exchangeId, 1);
  events.length = 0;
  await broker.run(1, { token: 'i', method: 'indexPapersStream', args: {} }, send);
  assert.equal(events[0].paperId, 'p');
  assert.equal(events[1].chunkCount, 1);
  assert.equal(events[2].event, 'done');
  assert.equal(broker.requests.size, 0);
});

test('broker discards partial PDFs and bounds an unacknowledged renderer stream', async (t) => {
  for (const [scenario, method, args] of [['pdf-gap', 'readPdf', { paper_id: 'p' }], ['flood', 'chatStream', { session_id: 's', turn_id: 'turn', message: '?' }]]) {
    const broker = new Broker(backend(t, scenario));
    let count = 0;
    const result = await broker.run(1, { token: 'x', method, args }, () => count++);
    assert.equal(result.ok, false);
    assert.ok(count <= 32);
    if (scenario === 'flood') assert.ok(count > 0, 'valid Chat request reaches stream backpressure');
    assert.equal(result.data, undefined);
    assert.equal(broker.requests.size, 0);
  }
});

test('unconfigured actual Rust binary stays health-only outside repository without cwd discovery', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'research-shell-health-'));
  const executable = join(dir, 'research-backend');
  await cp(fileURLToPath(new URL('../backend/target/debug/research-backend', import.meta.url)), executable);
  const value = new Backend({ executable, cwd: dir, env: { PATH: dir } });
  t.after(async () => { await value.stop(); await rm(dir, { recursive: true, force: true }); });
  const broker = new Broker(value);
  const health = await broker.run(1, { token: 'h', method: 'health', args: {} }, () => {});
  assert.equal(health.ok, true);
  assert.equal(health.data.backend, 'rust');
  assert.equal(health.data.commands.filter((item) => item.available).length, 1);
  assert.equal(health.data.commands.length, 20);
  const pid = value.child.pid;
  const papers = await broker.run(1, { token: 'p', method: 'listPapers', args: {} }, () => {});
  assert.equal(papers.ok, false);
  assert.match(papers.error, /not integrated/);
  assert.equal(value.child.pid, pid);
});

test('configured actual binary and broker list/get/read ranges with only browsing capabilities', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'research-shell-papers-'));
  const executable = join(dir, 'research-backend');
  const resources = join(dir, 'resources'), papers = join(resources, 'ResearchPapers');
  const data = join(dir, 'data');
  await mkdir(papers, { recursive: true }); await mkdir(data);
  const bytes = pdf();
  await writeFile(join(papers, 'Preview.pdf'), bytes);
  await writeFile(join(papers, 'Corrupt.pdf'), 'not a PDF');
  const large = await open(join(papers, 'Oversized.pdf'), 'w');
  await large.truncate(64 * 1024 * 1024 + 1); await large.close();
  await cp(fileURLToPath(new URL('../backend/target/debug/research-backend', import.meta.url)), executable);
  const value = new Backend({ executable, cwd: data, env: { PATH: dir,
    CAPSTONE_APP_DATA_ROOT: data, CAPSTONE_RESOURCES_DIR: resources, CAPSTONE_PAPERS_DIR: papers } });
  t.after(async () => { await value.stop(); await rm(dir, { recursive: true, force: true }); });
  const broker = new Broker(value);
  let sequence = 0;
  const run = (method, args = {}) => broker.run(1, { token: `p${++sequence}`, method, args }, () => {});
  const health = await run('health');
  assert.deepEqual(health.data.commands.filter((c) => c.available).map((c) => c.command),
    ['get_paper', 'health', 'list_papers', 'read_pdf', 'read_pdf_range']);
  assert.equal(health.data.search_enabled, false);
  const listed = await run('listPapers');
  assert.deepEqual(listed.data.map((p) => p.id), ['Corrupt', 'Oversized', 'Preview']);
  const info = await run('getPaper', { paper_id: 'Preview' });
  assert.deepEqual(info.data, { id: 'Preview', filename: 'Preview.pdf', title: 'Preview', size_bytes: bytes.length });
  const read = await run('readPdf', { paper_id: 'Preview' });
  assert.equal(read.ok, true);
  assert.equal(read.data.mime_type, 'application/pdf');
  assert.equal(read.data.bytes, undefined);
  const range = await run('readPdfRange', { preview_id: read.data.preview_id, begin: 0, end: bytes.length });
  assert.deepEqual(Buffer.from(range.data.bytes), bytes);
  await run('closePdf', { preview_id: read.data.preview_id });
  for (const id of ['missing', 'Corrupt', 'Oversized', '../Preview', '/Preview']) {
    const result = await run('readPdf', { paper_id: id });
    assert.equal(result.ok, false);
    assert.equal(result.data, undefined);
    assert.doesNotMatch(result.error, /resources|ResearchPapers|research-shell/);
  }
  for (const method of ['paperStatuses', 'bm25Status', 'indexPapers', 'resetAllIndices']) {
    const result = await run(method);
    assert.equal(result.ok, false);
    assert.match(result.error, /not integrated/);
  }
  assert.deepEqual(await readdir(data), []);
  assert.equal(broker.requests.size, 0);
});