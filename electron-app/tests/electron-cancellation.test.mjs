import assert from 'node:assert/strict';
import test from 'node:test';
import { EventEmitter, getEventListeners } from 'node:events';
import { readFile } from 'node:fs/promises';
import { webcrypto } from 'node:crypto';
import vm from 'node:vm';
import { Backend } from '../electron/backend.mjs';
import { Broker } from '../electron/broker.mjs';

const flush = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
const reset = { ok: true, reset: { vector: true, bm25: true, statuses: true } };
const chat = { session_id: 's', reply: 'committed', chat_mode: 'rag', indexed: true, sources: [] };
const preload = await readFile(new URL('../electron/preload.cjs', import.meta.url), 'utf8');

// Real framing, backend, broker and preload; only the child and Electron IPC are
// controlled. A mock clock proves no premature settlement/kill without sleeps.
function host(t, options = {}) {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const children = [];
  const backend = new Backend({ cancelTimeout: 20, requestTimeout: 10_000, ...options, spawnChild() {
    const child = new EventEmitter();
    Object.assign(child, { stdout: new EventEmitter(), stderr: new EventEmitter(), stdin: new EventEmitter(),
      frames: [], eof: 0, kills: 0, closed: false });
    child.stdin.writableLength = 0;
    child.stdin.write = (line) => { child.frames.push(JSON.parse(line)); return true; };
    child.stdin.end = () => child.eof++;
    child.close = () => { if (!child.closed) { child.closed = true; child.stdout.emit('end'); child.emit('close'); } };
    child.kill = () => { child.kills++; child.close(); };
    child.frame = (frame) => child.stdout.emit('data', Buffer.from(JSON.stringify({ v: 1, ...frame }) + '\n'));
    children.push(child);
    return child;
  } });
  const broker = new Broker(backend);
  const ipc = new EventEmitter();
  const calls = [];
  ipc.invoke = async (channel, ...args) => {
    calls.push({ channel, args });
    if (channel === 'research:run') return broker.run(1, args[0], (token, event, seq) => ipc.emit('research:event', {}, token, event, seq));
    if (channel === 'research:cancel') broker.cancel(1, args[0]);
    if (channel === 'research:cancel-all') broker.cancelOwner(1);
    if (channel === 'research:ack') broker.acknowledge(1, ...args);
  };
  let api, unload;
  const context = vm.createContext({ crypto: webcrypto, require: () => ({ ipcRenderer: ipc,
    contextBridge: { exposeInMainWorld(_name, value) { api = value; } } }),
    window: { addEventListener(_name, fn) { unload = fn; } } });
  vm.runInContext(preload, context);
  t.after(async () => { children.forEach((child) => child.close()); await backend.stop(); });
  return { backend, broker, children, ipc, calls, api, unload: () => unload(),
    active: () => vm.runInContext('active.size', context),
    clean() { assert.equal(backend.pending.size, 0); assert.equal(backend.acks.size, 0);
      assert.equal(broker.requests.size, 0); assert.equal(this.active(), 0); },
    tick: async (ms) => { t.mock.timers.tick(ms); await flush(); } };
}

function observe(promise) {
  const state = { settled: false, value: undefined, error: undefined };
  state.done = promise.then((value) => { state.value = value; }, (error) => { state.error = error; })
    .then(() => { state.settled = true; });
  return state;
}

for (const outcome of ['result', 'error']) {
  test(`refused cancellation keeps cross-layer tracking for delayed sealed ${outcome}`, async (t) => {
    const h = host(t);
    const request = h.api.resetAllIndices();
    const state = observe(request.done);
    await flush();
    const child = h.children[0];
    const { id } = child.frames[0];
    const timer = h.backend.pending.get(id).timer;
    request.cancel(); request.cancel();
    assert.equal(h.backend.pending.size, 1);
    assert.equal(h.broker.requests.size, 1);
    assert.equal(h.active(), 1);
    assert.equal(child.frames.filter((frame) => frame.type === 'cancel').length, 1);
    child.frame({ id, type: 'cancel_ack', accepted: false });
    await h.tick(2000); // Beyond both cancelTimeout and the former kill deadline.
    assert.equal(state.settled, false);
    assert.equal(h.backend.pending.get(id).timer, timer);
    assert.equal(child.kills, 0);
    assert.equal(child.eof, 0);
    child.frame({ id, seq: 0, type: outcome, ...(outcome === 'result' ? { data: reset } :
      { error: { code: 'internal', message: 'PRIVATE-FIXTURE-DIAGNOSTIC' } }) });
    await state.done;
    if (outcome === 'result') assert.deepEqual(state.value, reset);
    else assert.equal(state.error.message, 'Backend operation failed.');
    h.clean();
    await h.tick(20_000);
    assert.equal(child.kills, 0);
    assert.equal(child.eof, 0);
    assert.equal(h.children.length, 1);
    assert.equal(child.frames.filter((frame) => frame.type === 'request').length, 1);
  });
}

test('accepted cancellation is not terminal, and repeated cancel stays bounded until cancelled', async (t) => {
  const h = host(t);
  const request = h.api.resetAllIndices();
  const state = observe(request.done);
  await flush();
  const child = h.children[0], { id } = child.frames[0];
  request.cancel(); request.cancel();
  child.frame({ id, type: 'cancel_ack', accepted: true });
  await h.tick(2000);
  assert.equal(state.settled, false);
  assert.equal(h.active(), 1);
  child.frame({ id, seq: 0, type: 'cancelled' });
  await state.done;
  assert.equal(state.error.message, 'Request cancelled.');
  h.clean();
  await h.tick(20_000);
  assert.equal(child.kills, 0);
  assert.equal(child.eof, 0);
});

for (const type of ['result', 'error', 'cancelled']) {
  test(`${type} before cancellation acknowledgment settles once and does not poison subsequent work`, async (t) => {
    const h = host(t);
    const request = h.api.resetAllIndices();
    const state = observe(request.done);
    await flush();
    const child = h.children[0], { id } = child.frames[0];
    request.cancel();
    child.frame({ id, seq: 0, type, ...(type === 'result' ? { data: reset } :
      type === 'error' ? { error: { code: 'internal' } } : {}) });
    await state.done;
    assert.equal(h.backend.pending.size, 0);
    assert.equal(h.backend.acks.size, 1);
    request.cancel();
    child.frame({ id, type: 'cancel_ack', accepted: type === 'cancelled' });
    h.clean();
    const next = h.api.resetAllIndices();
    await flush();
    const second = child.frames.findLast((frame) => frame.type === 'request');
    child.frame({ id: second.id, seq: 0, type: 'result', data: reset });
    assert.deepEqual(await next.done, reset);
    h.clean();
    await h.tick(20_000);
    assert.equal(child.eof, 0);
    assert.equal(child.kills, 0);
    assert.equal(h.children.length, 1);
  });
}

for (const dispose of ['cancel', 'unload', 'owner', 'callback']) {
  test(`${dispose} detaches obsolete progress without losing a refused-cancel terminal`, async (t) => {
    const h = host(t);
    let events = 0;
    const request = h.api.chatStream('s', '?', () => { events++; if (dispose === 'callback') throw new Error('view gone'); });
    const state = observe(request.done);
    await flush();
    const child = h.children[0], { id } = child.frames[0];
    child.frame({ id, seq: 0, type: 'event', event: 'delta', data: { delta: 'first' } });
    if (dispose === 'cancel') request.cancel();
    if (dispose === 'unload') { h.unload(); assert.equal(h.ipc.listenerCount('research:event'), 0); }
    if (dispose === 'owner') h.broker.cancelOwner(1);
    child.frame({ id, type: 'cancel_ack', accepted: false });
    for (let seq = 1; seq < 40; seq++) child.frame({ id, seq, type: 'event', event: 'delta', data: { delta: 'late' } });
    await h.tick(2000);
    assert.equal(state.settled, false);
    assert.equal(events, 1);
    assert.equal(h.active(), 1);
    assert.equal(h.broker.requests.size, 1);
    assert.equal([...h.broker.requests.values()][0].unacked.size, 0);
    child.frame({ id, seq: 40, type: 'result', data: chat });
    await state.done;
    assert.equal(state.error, undefined);
    assert.equal(state.value, null);
    h.clean();
    assert.equal(child.kills, 0);
    assert.equal(child.eof, 0);
    if (dispose === 'unload') assert.throws(() => h.api.resetAllIndices(), /view is closed/);
  });
}

test('cancelled preload requests still count toward admission until real terminal cleanup', async (t) => {
  const h = host(t);
  const requests = Array.from({ length: 32 }, () => h.api.resetAllIndices());
  const outcomes = requests.map((request) => observe(request.done));
  await flush();
  requests.forEach((request) => request.cancel());
  assert.equal(h.active(), 32);
  assert.throws(() => h.api.resetAllIndices(), /Too many active/);
  const child = h.children[0];
  for (const { id } of child.frames.filter((frame) => frame.type === 'request')) {
    child.frame({ id, seq: 0, type: 'cancelled' });
    child.frame({ id, type: 'cancel_ack', accepted: true });
  }
  await Promise.all(outcomes.map((state) => state.done));
  assert.ok(outcomes.every((state) => state.error.message === 'Request cancelled.'));
  h.clean();
});

test('progress backpressure or terminal delivery failure cannot replace a real committed result', async (t) => {
  const h = host(t);
  const events = [];
  const outcome = h.broker.run(1, { token: 'full', method: 'chatStream', args: { session_id: 's', message: '?' } },
    (_id, event) => { events.push(event); if (event.event === 'done') throw new Error('view gone'); });
  await flush();
  const child = h.children[0], { id } = child.frames[0];
  for (let seq = 0; seq < 32; seq++) child.frame({ id, seq, type: 'event', event: 'delta', data: { delta: 'progress' } });
  assert.equal([...h.broker.requests.values()][0].unacked.size, 32);
  child.frame({ id, seq: 32, type: 'result', data: chat });
  assert.equal((await outcome).ok, true);
  assert.equal(events.at(-1).event, 'done');
  h.clean();
});

test('abort signal remains tracked until authoritative result and listener is then removed', async (t) => {
  const h = host(t);
  const controller = new AbortController();
  const request = h.backend.request('reset_all_indices', {}, { signal: controller.signal });
  await flush();
  const child = h.children[0], { id } = child.frames[0];
  controller.abort();
  child.frame({ id, type: 'cancel_ack', accepted: false });
  child.frame({ id, seq: 0, type: 'result', data: reset });
  assert.deepEqual(await request.done, reset);
  assert.equal(getEventListeners(controller.signal, 'abort').length, 0);
  h.clean();
});

test('cancel after backend completion but before IPC delivery preserves success without sending cancel', async (t) => {
  const h = host(t);
  const request = h.api.resetAllIndices();
  await flush();
  const child = h.children[0], { id } = child.frames[0];
  child.frame({ id, seq: 0, type: 'result', data: reset });
  request.cancel();
  assert.deepEqual(await request.done, reset);
  assert.equal(child.frames.length, 1);
  h.clean();
});

test('stale request handles cannot cancel reused request IDs after child replacement', async (t) => {
  const h = host(t);
  const first = h.backend.request('reset_all_indices', {});
  await flush();
  const child = h.children[0], { id } = child.frames[0];
  child.frame({ id, seq: 0, type: 'result', data: reset });
  await first.done;
  child.close();
  await flush();
  const next = h.backend.request('reset_all_indices', {});
  await flush();
  const replacement = h.children[1];
  assert.equal(replacement.frames[0].id, id);
  first.cancel();
  assert.equal(replacement.frames.length, 1);
  replacement.frame({ id, seq: 0, type: 'result', data: reset });
  assert.deepEqual(await next.done, reset);
  h.clean();
});

test('graceful EOF drains sealed output past deadlines, including output after exit', async (t) => {
  const h = host(t);
  const request = h.api.resetAllIndices();
  const state = observe(request.done);
  await flush();
  const child = h.children[0], { id } = child.frames[0];
  request.cancel();
  child.frame({ id, type: 'cancel_ack', accepted: false });
  const stopped = observe(h.backend.stop());
  await h.tick(100_000);
  assert.equal(child.eof, 1);
  assert.equal(child.kills, 0);
  assert.equal(state.settled, false);
  assert.equal(stopped.settled, false);
  child.emit('exit', 0);
  child.frame({ id, seq: 0, type: 'result', data: reset });
  await state.done;
  assert.deepEqual(state.value, reset);
  child.close();
  await stopped.done;
  h.clean();
});

test('missing acknowledgment causes EOF, not kill or synthesized cancellation of a sealed write', async (t) => {
  const h = host(t);
  const request = h.api.resetAllIndices();
  const state = observe(request.done);
  await flush();
  const child = h.children[0], { id } = child.frames[0];
  request.cancel();
  await h.tick(2000);
  assert.equal(child.eof, 1);
  assert.equal(child.kills, 0);
  assert.equal(state.settled, false);
  child.frame({ id, type: 'cancel_ack', accepted: false });
  child.frame({ id, seq: 0, type: 'result', data: reset });
  await state.done;
  assert.deepEqual(state.value, reset);
  child.close();
  await flush();
  h.clean();
});

test('pre-submission cancellation while an old child drains settles locally without replay', async (t) => {
  const h = host(t);
  await h.backend.ready();
  const child = h.children[0];
  const stopping = h.backend.stop(false);
  const request = h.api.resetAllIndices();
  request.cancel();
  await assert.rejects(request.done, /cancelled/);
  assert.equal(child.frames.length, 0);
  child.close();
  await stopping;
  await flush();
  assert.equal(h.children.flatMap((entry) => entry.frames).length, 0);
  h.clean();
});

test('shutdown racing an unsubmitted request cannot fail or kill an existing sealed write', async (t) => {
  const h = host(t);
  const committed = h.api.resetAllIndices();
  const outcome = observe(committed.done);
  await flush();
  const child = h.children[0], { id } = child.frames[0];
  const unsubmitted = h.api.resetAllIndices();
  const rejected = observe(unsubmitted.done);
  const stopping = h.backend.stop();
  await h.tick(20_000);
  await rejected.done;
  assert.ok(rejected.error);
  assert.equal(child.frames.length, 1);
  assert.equal(child.kills, 0);
  assert.equal(outcome.settled, false);
  child.frame({ id, seq: 0, type: 'result', data: reset });
  await outcome.done;
  assert.deepEqual(outcome.value, reset);
  child.close();
  await stopping;
  h.clean();
});

for (const accepted of [true, false]) {
  test(`partial PDF cancellation accepted=${accepted} retains validation until its real terminal then clears bytes`, async (t) => {
    const h = host(t);
    const request = h.api.readPdf('p');
    const state = observe(request.done);
    await flush();
    const child = h.children[0], { id } = child.frames[0];
    const pdf = [...h.broker.requests.values()][0].pdf;
    child.frame({ id, seq: 0, type: 'event', event: 'pdf_chunk', data: { offset: 0, bytes: [37, 80] } });
    request.cancel();
    child.frame({ id, type: 'cancel_ack', accepted });
    await h.tick(2000);
    assert.equal(state.settled, false);
    if (accepted) child.frame({ id, seq: 1, type: 'cancelled' });
    else {
      child.frame({ id, seq: 1, type: 'event', event: 'pdf_chunk', data: { offset: 2, bytes: [68, 70] } });
      child.frame({ id, seq: 2, type: 'result', data: { id: 'p', filename: 'p.pdf', mime_type: 'application/pdf', total_bytes: 4 } });
    }
    await state.done;
    if (accepted) assert.match(state.error.message, /cancelled/);
    else assert.deepEqual(state.value.bytes, [37, 80, 68, 70]);
    assert.equal(pdf.chunks.length, 0);
    assert.equal(pdf.size, 0);
    h.clean();
  });
}

for (const failure of ['close', 'timeout', 'protocol', 'ipc']) {
  test(`${failure} after submitted cancellation is unknown and never replays the mutation`, async (t) => {
    const h = host(t);
    if (failure === 'ipc') h.ipc.invoke = async () => { throw new Error('PRIVATE-FIXTURE-DIAGNOSTIC'); };
    const request = h.api.resetAllIndices();
    const state = observe(request.done);
    await flush();
    if (failure !== 'ipc') {
      const child = h.children[0], { id } = child.frames[0];
      request.cancel();
      child.frame({ id, type: 'cancel_ack', accepted: false });
      if (failure === 'close') child.close();
      if (failure === 'protocol') child.frame({ id, seq: 7, type: 'result', data: reset });
      if (failure === 'timeout') await h.tick(10_001);
    }
    await state.done;
    assert.match(state.error.message, /outcome is unknown/);
    assert.doesNotMatch(state.error.message, /Please retry|cancelled|rollback|PRIVATE/);
    h.clean();
    await h.tick(2000);
    assert.ok(h.children.length <= 1);
    assert.ok(h.children.flatMap((child) => child.frames).filter((frame) => frame.type === 'request').length <= 1);
  });
}