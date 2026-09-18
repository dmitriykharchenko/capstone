import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { mkdtemp, cp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { Backend, MAX_FRAME_BYTES } from '../electron/backend.mjs';
import { Broker } from '../electron/broker.mjs';

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

test('hung cancellation kills child and allows a new request only after close', { timeout: 5000 }, async (t) => {
  const value = backend(t, 'ignore-cancel', { cancelTimeout: 20 });
  const request = value.request('hold', {});
  const rejected = assert.rejects(request.done, /cancelled/);
  await settle();
  const child = value.child;
  const closed = new Promise((resolve) => child.once('close', resolve));
  request.cancel();
  await rejected;
  await closed;
  assert.equal(value.child, null);
  assert.equal(value.pending.size, 0);
});

test('oversized request is rejected and all pending work is settled', async (t) => {
  const value = backend(t);
  await assert.rejects(value.request('echo', { message: 'x'.repeat(MAX_FRAME_BYTES) }).done);
  assert.equal(value.pending.size, 0);
});

test('broker assembles PDF and adapts ordered chat/index events only at terminal result', async (t) => {
  const broker = new Broker(backend(t));
  const events = [];
  const send = (id, event, seq) => { events.push(event); broker.acknowledge(1, id, seq); };
  const pdf = await broker.run(1, { token: 'p', method: 'readPdf', args: { paper_id: 'paper' } }, send);
  assert.deepEqual(pdf.data.bytes, [37, 80, 68, 70]);
  assert.equal(events.length, 0);
  assert.equal((await broker.run(1, { token: 'c', method: 'chatStream', args: { session_id: 's', message: '?' } }, send)).ok, true);
  assert.deepEqual(events.map((event) => event.event), ['delta', 'delta', 'done']);
  assert.equal(events.at(-1).chatMode, 'rag');
  events.length = 0;
  await broker.run(1, { token: 'i', method: 'indexPapersStream', args: {} }, send);
  assert.equal(events[0].paperId, 'p');
  assert.equal(events[1].chunkCount, 1);
  assert.equal(events[2].event, 'done');
  assert.equal(broker.requests.size, 0);
});

test('broker discards partial PDFs and bounds an unacknowledged renderer stream', async (t) => {
  for (const [scenario, method, args] of [['pdf-gap', 'readPdf', { paper_id: 'p' }], ['flood', 'chatStream', { session_id: 's', message: '?' }]]) {
    const broker = new Broker(backend(t, scenario));
    let count = 0;
    const result = await broker.run(1, { token: 'x', method, args }, () => count++);
    assert.equal(result.ok, false);
    assert.ok(count <= 32);
    assert.equal(result.data, undefined);
    assert.equal(broker.requests.size, 0);
  }
});

test('actual Rust health-only binary runs outside repository with no Python and reports unavailable honestly', async (t) => {
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
  assert.equal(health.data.commands.length, 19);
  const pid = value.child.pid;
  const papers = await broker.run(1, { token: 'p', method: 'listPapers', args: {} }, () => {});
  assert.equal(papers.ok, false);
  assert.match(papers.error, /not integrated/);
  assert.equal(value.child.pid, pid);
});