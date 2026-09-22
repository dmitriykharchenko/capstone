import test from 'node:test';
import assert from 'node:assert/strict';
import { methods, parseResponse } from '../electron/validation.mjs';
import { parseEvent, terminalEvent } from '../electron/adapters.mjs';

const publication = { paper_id:'p', filename:'p.pdf', status:'indexed', chunk_count:1,
  version_id:'version', attempt_scope:'session', attempt:null };
test('native status distinguishes durable publication from session-only attempts', () => {
  const value = { readiness:'ready', statuses:[{ ...publication, attempt:{ status:'failed',
    error:'Embedding or metadata preparation failed.', observed_at:'2026-09-22T00:00:00Z' } }] };
  assert.deepEqual(parseResponse('paper_statuses', value), value);
  assert.equal(parseResponse('paper_statuses', { readiness:'ready', statuses:[publication] }).statuses[0].attempt, null);
  for (const readiness of ['unavailable','rebuild_required','recovery_required','recovering']) {
    assert.deepEqual(parseResponse('paper_statuses', { readiness,statuses:[] }), { readiness,statuses:[] });
    assert.throws(() => parseResponse('paper_statuses', { readiness,statuses:[publication] }));
    assert.throws(() => parseResponse('bm25_status', { readiness,built:true,chunk_count:1,paper_ids:['p'] }));
  }
  for (const patch of [{ status:'failed' }, { status:'empty' }, { status:'not_indexed' },
    { version_id:null }, { chunk_count:0 }, { attempt_scope:'durable' }, { private:'x' },
    { attempt:{ status:'indexed',error:'Embedding or metadata preparation failed.',observed_at:'now' } },
    { attempt:{ status:'failed',error:'private upstream output',observed_at:'now' } }]) {
    assert.throws(() => parseResponse('paper_statuses', { readiness:'ready',statuses:[{ ...publication,...patch }] }));
  }
});
test('index requests, progress, summary and reset remain strict', () => {
  assert.deepEqual(methods.indexPapers[1]({ reindex:true }), { reindex:true });
  for (const value of [{ reindex:'true' },{ force:true },{ path:'/tmp/private' }]) assert.throws(() => methods.indexPapers[1](value));
  const progress = { completed:1,total:1,paper_id:'p',filename:'p.pdf',status:'indexed',chunk_count:1,error:null };
  assert.equal(parseEvent('index_papers_stream','progress',progress).chunkCount,1);
  for (const patch of [{ status:'ready' },{ chunk_count:0 },{ completed:2 },{ status:'failed' },{ error:'private' }]) {
    assert.throws(() => parseEvent('index_papers_stream','progress',{ ...progress,...patch }));
  }
  const summary = { completed:1,total:1,indexed:1,empty:0,failed:0,skipped:0 };
  assert.equal(terminalEvent('index_papers_stream',summary).event,'done');
  assert.throws(() => terminalEvent('index_papers_stream',{ ...summary,failed:1 }));
  assert.equal(parseResponse('index_papers',{ enabled:true,indexed:1,summary }).summary.total,1);
  assert.throws(() => parseResponse('index_papers',{ enabled:true,indexed:0,summary }));
  const reset={ ok:true,reset:{ vector:true,bm25:true,statuses:true,graph:true },preserved:{ pdfs:true,history:true } };
  assert.deepEqual(parseResponse('reset_all_indices',reset),reset);
  assert.throws(() => parseResponse('reset_all_indices',{ ...reset,preserved:{ pdfs:true,history:false } }));
  assert.throws(() => parseResponse('reset_all_indices',{ ok:true,reset:{ vector:true,bm25:true,statuses:true } }));
});