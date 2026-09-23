import assert from 'node:assert/strict';
import test, { after, before } from 'node:test';
import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';
import { createServer } from 'vite';
import { runSaga, stdChannel } from 'redux-saga';
import { methods, parseResponse } from '../electron/validation.mjs';
import { parseEvent, terminalEvent } from '../electron/adapters.mjs';
import { Broker } from '../electron/broker.mjs';

const warning = 'Reply saved in history. History search indexing is incomplete; no reply was rolled back. Reindex requires an explicit action.';
const saved = { session_id: 's', turn_id: 'turn', exchange_id: 42, reply: 'saved answer', sources: [], chat_mode: 'native',
  indexed: false, history_index_status: 'failed', history_index_status_observed_at: { reset_epoch: 2, retrieval_revision: 4 }, warning };
const source = { rank: 1, paper_id: 'paper', chunk_index: 0, filename: 'paper.pdf', title: 'Paper', page: 1, score: 0.9, snippet: 'Evidence' };
const hop = (index) => ({ id: `hop-${index}`, index, query: `Question ${index}` });
const settle = () => new Promise((resolve) => setImmediate(resolve));
let vite, actions, streamChatTurn;

before(async () => {
  assert.ok(readFileSync(new URL('../backend/src/rag/pipeline.rs', import.meta.url), 'utf8').includes('format!("hop-{index}")'));
  vite = await createServer({ root: fileURLToPath(new URL('..', import.meta.url)), appType: 'custom', logLevel: 'silent', server: { middlewareMode: true } });
  actions = await vite.ssrLoadModule('/src/slices/chat/chat-slice.ts');
  ({ streamChatTurn } = await vite.ssrLoadModule('/src/slices/chat/sagas/chat-saga.ts'));
});
after(async () => vite?.close());

function runTurn(service) {
  let state = actions.chatReducer(undefined, actions.setSessionId('s'));
  const dispatched = [];
  const task = runSaga({ channel: stdChannel(), dispatch(action) {
    dispatched.push(action);
    state = actions.chatReducer(state, action);
  }, getState: () => ({ chat: state }) }, streamChatTurn, actions.sendMessage('Question'), service);
  return { task, dispatched, state: () => state };
}

function applyProgress(state, name, data) {
  const e = parseEvent('chat_stream', name, data);
  const action = name === 'plan' ? actions.chatPlanReceived(e.subqueries)
    : name === 'coverage' ? actions.chatCoverageReceived(e.status, e.total, e.subqueries)
    : name === 'subquery' ? actions.chatSubqueryStarted(e, e.total)
    : name === 'subanswer' ? actions.chatSubanswerReceived(e.id, e.index, e.query, e.answer, e.sources, e.total)
    : actions.chatChunk(e.delta);
  return actions.chatReducer(state, action);
}

test('native chat request requires stable turn identity and denies unknown options', () => {
  const parse = methods.chat[1];
  assert.deepEqual(parse({ session_id: 's', turn_id: 't', message: '?' }), { session_id: 's', turn_id: 't', message: '?' });
  for (const value of [{ session_id: 's', message: '?' }, { session_id: 's', turn_id: '', message: '?' },
    { session_id: 's', turn_id: 't', message: '?', options: {} }]) assert.throws(() => parse(value));
});
test('saved warning terminal preserves authoritative routing and observation metadata', () => {
  assert.deepEqual(parseResponse('chat', saved), saved);
  const event = terminalEvent('chat_stream', saved);
  assert.equal(event.event, 'done'); assert.equal(event.turnId, 'turn'); assert.equal(event.exchangeId, 42);
  assert.equal(event.warning, warning); assert.equal(event.historyIndexStatus, 'failed');
  assert.deepEqual(event.historyIndexStatusObservedAt, saved.history_index_status_observed_at);
});
test('chat validator rejects false index claims, unsafe IDs and unsanitized warning details', () => {
  for (const change of [{ indexed: true }, { warning: null }, { exchange_id: Number.MAX_SAFE_INTEGER + 1 },
    { warning: 'raw private provider detail' }, { history_index_status: 'unknown' },
    { history_index_status_observed_at: null }, { credentials: {} }]) assert.throws(() => parseResponse('chat', { ...saved, ...change }));
  assert.doesNotThrow(() => parseResponse('chat', { ...saved, history_index_status: 'blocked', history_index_status_observed_at: null }));
});
test('canonical hydration keeps source and identity metadata even while derived index is blocked', () => {
  const result = parseResponse('get_messages', { session_id: 's', messages: [
    { role: 'assistant', content: 'saved answer', exchange_id: 42, sources: [], turn_id: 'turn',
      history_index_status: 'blocked', history_index_status_observed_at: null }] });
  assert.equal(result.messages[0].content, 'saved answer'); assert.equal(result.messages[0].turn_id, 'turn');
});
test('broker refuses wrong session or turn instead of misrouting saved success', async () => {
  for (const change of [{ session_id: 'other' }, { turn_id: 'other' }, {}]) {
    let calls = 0;
    const broker = new Broker({ request() { calls++; return { done: Promise.resolve({ ...saved, ...change }), cancel() {} }; } });
    const result = await broker.run(1, { token: 'request', method: 'chat', args: { session_id: 's', turn_id: 'turn', message: '?' } }, () => {});
    assert.equal(result.ok, Object.keys(change).length === 0); assert.equal(calls, 1);
  }
});

for (const initial of [2, 3, 4, 5]) for (const extra of [0, 1, 2]) {
  test(`native adapters → saga → reducer: ${initial} initial hops and ${extra} followups`, async (t) => {
    let emit, finish, turnId, calls = 0;
    const run = runTurn((_session, _message, callback, id) => {
      calls++; emit = callback; turnId = id;
      return new Promise((resolve) => { finish = resolve; });
    });
    t.after(() => run.task.cancel());
    const send = async (name, data) => { emit(parseEvent('chat_stream', name, data)); await settle(); };
    await settle();
    const planned = Array.from({ length: initial }, (_, i) => hop(i + 1));
    await send('plan', { subqueries: planned });
    assert.deepEqual(run.state().activeTurn.subqueries.map(({ id }) => id), planned.map(({ id }) => id));
    assert.equal(run.state().activeTurn.initialCount, initial);
    const answer = async (item, total, kind) => {
      await send('subquery', { ...item, total });
      assert.equal(run.state().activeTurn.subqueries[item.index - 1].status, 'active');
      await send('subanswer', { ...item, total, answer: `Answer ${item.index}`, sources: [source] });
      const entry = run.state().activeTurn.subqueries[item.index - 1];
      assert.deepEqual([entry.id, entry.index, entry.kind, entry.status, entry.answer],
        [item.id, item.index, kind, 'complete', `Answer ${item.index}`]);
      assert.deepEqual(entry.sources, [source]);
      assert.equal(run.state().messages.length, 1, 'intermediate answers stay out of the transcript');
    };
    for (const item of planned) await answer(item, initial, 'initial');
    await send('coverage', { status: 'reviewing', total: initial, subqueries: [] });
    assert.equal(run.state().activeTurn.coverageStatus, 'reviewing');
    await send('delta', { delta: 'too early' });
    assert.equal(run.state().activeTurn.finalAnswer, '');
    const followups = Array.from({ length: extra }, (_, i) => hop(initial + i + 1));
    await send('coverage', { status: extra ? 'followup' : 'complete', total: initial + extra, subqueries: followups });
    assert.equal(run.state().activeTurn.total, initial + extra);
    assert.equal(run.state().activeTurn.coverageStatus, extra ? 'followup' : 'complete');
    for (const item of followups) await answer(item, initial + extra, 'followup');
    await send('delta', { delta: 'provisional' });
    assert.equal(run.state().activeTurn.finalAnswer, 'provisional');
    const done = terminalEvent('chat_stream', { ...saved, turn_id: turnId, sources: [source] });
    assert.equal(done.warning, warning);
    emit(done); finish(null);
    await run.task.toPromise();
    assert.equal(calls, 1);
    assert.equal(run.state().error, null);
    assert.equal(run.state().pending, false);
    assert.equal(run.state().activeTurn, null);
    assert.deepEqual(run.state().messages.map(({ turn_id, exchange_id }) => [turn_id, exchange_id]), [[turnId, 42], [turnId, 42]]);
    assert.equal(run.state().messages[1].content, 'saved answer');
    assert.equal(run.state().messages[1].history_index_status, 'failed');
    assert.deepEqual(run.state().messages[1].sources, [source]);
    assert.deepEqual(run.state().messages[1].history_index_status_observed_at, saved.history_index_status_observed_at);
  });
}

test('native ID validation retains bounds, identity, ordering, duplicate and reset guards', () => {
  const empty = actions.chatReducer(undefined, actions.chatStarted());
  for (const id of ['arbitrary', 'hop-0', 'hop-01', 'hop-8', 'hop-2', 'HOP-1']) {
    assert.equal(applyProgress(empty, 'plan', { subqueries: [{ ...hop(1), id }, hop(2)] }), empty);
    assert.equal(applyProgress(empty, 'subquery', { ...hop(1), id, total: 2 }), empty);
  }
  assert.throws(() => parseEvent('chat_stream', 'subquery', { ...hop(8), total: 8 }));
  assert.throws(() => parseEvent('chat_stream', 'subquery', { ...hop(1.5), total: 2 }));
  for (const subqueries of [[hop(1), hop(1)], [hop(1), hop(3)]]) {
    assert.equal(applyProgress(empty, 'plan', { subqueries }), empty);
  }
  const withoutPlan = applyProgress(empty, 'subquery', { ...hop(1), total: 2 });
  assert.equal(withoutPlan.activeTurn.subqueries.length, 1);
  assert.equal(withoutPlan.activeTurn.subqueries[0].id, 'hop-1');
  let state = applyProgress(empty, 'plan', { subqueries: [hop(2), hop(1)] });
  assert.deepEqual(state.activeTurn.subqueries.map(({ id }) => id), ['hop-1', 'hop-2']);
  assert.equal(applyProgress(state, 'plan', { subqueries: [hop(1), hop(2)] }), state);
  assert.equal(applyProgress(state, 'subquery', { ...hop(4), total: 4 }), state);
  assert.equal(applyProgress(state, 'coverage', { status: 'followup', total: 3, subqueries: [hop(3)] }), state);
  for (const data of [{ ...hop(1), id: 'arbitrary' }, { ...hop(1), index: 2 }, { ...hop(1), query: 'Changed' }]) {
    assert.equal(applyProgress(state, 'subanswer', { ...data, total: 2, answer: 'answer', sources: [] }), state);
  }
  for (const item of [hop(1), hop(2)]) {
    state = applyProgress(state, 'subquery', { ...item, total: 2 });
    state = applyProgress(state, 'subanswer', { ...item, total: 2, answer: 'answer', sources: [source] });
  }
  assert.equal(applyProgress(state, 'subquery', { ...hop(1), total: 2 }), state);
  assert.equal(applyProgress(state, 'subanswer', { ...hop(1), total: 2, answer: 'overwrite', sources: [] }), state);
  state = applyProgress(state, 'coverage', { status: 'reviewing', total: 2, subqueries: [] });
  for (const subqueries of [[hop(4), hop(3)], [hop(3), hop(3)], [{ ...hop(3), id: 'other' }], [{ ...hop(3), query: hop(1).query }]]) {
    assert.equal(applyProgress(state, 'coverage', { status: 'followup', total: 2 + subqueries.length, subqueries }), state);
  }
  state = applyProgress(state, 'coverage', { status: 'followup', total: 3, subqueries: [hop(3)] });
  assert.equal(applyProgress(state, 'coverage', { status: 'followup', total: 3, subqueries: [hop(3)] }), state);
  const reset = actions.chatReducer(state, actions.setSessionId('other'));
  const complete = actions.chatReducer(state, actions.chatSucceeded('canonical', 'native', []));
  for (const closed of [reset, complete]) for (const [name, data] of [
    ['plan', { subqueries: [hop(1), hop(2)] }], ['subquery', { ...hop(3), total: 3 }],
    ['subanswer', { ...hop(3), total: 3, answer: 'late', sources: [] }],
    ['coverage', { status: 'followup', total: 3, subqueries: [hop(3)] }], ['delta', { delta: 'late' }],
  ]) assert.equal(applyProgress(closed, name, data), closed);
});

for (const loseTerminal of [false, true]) {
  test(`real Broker terminal delivery ${loseTerminal ? 'loss is UNKNOWN without replay' : 'preserves saved reply and warning'}`, async () => {
    let calls = 0;
    const broker = new Broker({ request(_command, params) {
      calls++;
      return { done: Promise.resolve({ ...saved, turn_id: params.turn_id }), cancel() {} };
    } });
    const run = runTurn((session_id, message, callback, turn_id) => broker.run(1,
      { token: 'terminal', method: 'chatStream', args: { session_id, message, turn_id } },
      (_token, event) => {
        if (loseTerminal) throw new Error('synthetic private delivery detail');
        callback(event);
      }).then((result) => { assert.equal(result.ok, true); return result.data; }));
    await run.task.toPromise();
    assert.equal(calls, 1);
    assert.equal(broker.requests.size, 0);
    assert.equal(run.state().pending, false);
    assert.equal(run.state().activeTurn, null);
    if (loseTerminal) {
      assert.equal(run.dispatched.filter((a) => a.type === 'chat/failed').length, 1);
      assert.equal(run.dispatched.some((a) => a.type === 'chat/succeeded'), false);
      assert.equal(run.state().messages.length, 1);
      assert.match(run.state().error, /outcome is unknown/i);
      assert.match(run.state().error, /check.*saved history.*before.*new turn/i);
      assert.doesNotMatch(run.state().error, /please try again|cancelled|rolled back|synthetic private/i);
    } else {
      assert.equal(run.dispatched.filter((a) => a.type === 'chat/succeeded').length, 1);
      assert.equal(run.dispatched.some((a) => a.type === 'chat/failed'), false);
      assert.equal(run.state().messages[1].content, saved.reply);
      assert.equal(run.state().messages[1].history_index_status, 'failed');
      assert.equal(run.state().error, null);
    }
  });
}