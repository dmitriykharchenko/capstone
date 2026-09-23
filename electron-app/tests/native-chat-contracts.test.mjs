import assert from 'node:assert/strict';
import test from 'node:test';
import { methods, parseResponse } from '../electron/validation.mjs';
import { terminalEvent } from '../electron/adapters.mjs';
import { Broker } from '../electron/broker.mjs';

const warning = 'Reply saved in history. History search indexing is incomplete; no reply was rolled back. Reindex requires an explicit action.';
const saved = { session_id: 's', turn_id: 'turn', exchange_id: 42, reply: 'saved answer', sources: [], chat_mode: 'native',
  indexed: false, history_index_status: 'failed', history_index_status_observed_at: { reset_epoch: 2, retrieval_revision: 4 }, warning };

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