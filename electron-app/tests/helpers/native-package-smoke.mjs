import assert from 'node:assert/strict';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { syntheticPdf } from '../pdf-fixtures.mjs';
import { fakeProvider } from './packaging-provider.mjs';

// Caller supplies a newly-created disposable root, never a user data path.
export async function nativePackageSmoke({ root, executable, resources, Backend, Broker }) {
  const data = join(root, 'data'), papers = join(root, 'papers'), emptyPath = join(root, 'empty-path');
  for (const path of [data, papers, emptyPath]) await mkdir(path);
  const pdf = syntheticPdf('text').bytes;
  await writeFile(join(papers, 'Fixture.pdf'), pdf);
  const provider = await fakeProvider();
  const env = { PATH: emptyPath, CAPSTONE_APP_DATA_ROOT: data, CAPSTONE_RESOURCES_DIR: resources,
    CAPSTONE_PAPERS_DIR: papers, CAPSTONE_CHAT_MODEL: 'fixture-chat', CAPSTONE_EMBED_MODEL: 'fixture-embed',
    CAPSTONE_EMBED_DIMENSIONS: '2', CAPSTONE_CHAT_BASE_URL: provider.url, CAPSTONE_EMBED_BASE_URL: provider.url,
    OPENAI_API_KEY: randomUUID() };
  let backend, broker, sequence = 0;
  const children = [];
  const start = (extra = {}) => {
    backend = new Backend({ executable, cwd: root, env: { ...env, ...extra }, requestTimeout: 15_000 });
    broker = new Broker(backend);
  };
  const run = async (method, args = {}) => {
    const token = `smoke-${++sequence}`, events = [];
    const result = await broker.run(1, { token, method, args }, (_, value, seq) => {
      events.push(value); broker.acknowledge(1, token, seq);
    });
    assert.equal(result.ok, true, `${method}: ${result.error ?? 'failed'}`);
    if (backend.child && !children.includes(backend.child)) children.push(backend.child);
    return { data: result.data, events };
  };
  const raw = (command, params = {}) => backend.request(command, params).done;
  const stop = async () => {
    const child = backend.child;
    await backend.stop();
    assert.equal(backend.child, null);
    assert.equal(backend.pending.size, 0);
    assert.equal(broker.requests.size, 0);
    if (child) assert.equal(child.exitCode, 0);
  };
  try {
    start();
    let health = (await run('health')).data;
    assert.deepEqual(health.commands.filter((c) => c.available).map((c) => c.command), ['get_paper', 'health', 'list_papers', 'read_pdf', 'read_pdf_range']);
    assert.equal(health.chat_mode, 'unavailable');
    assert.equal((await readdir(data)).length, 0, 'no opt-in means no database creation');
    assert.deepEqual(provider.counts, { embedding: 0, model: 0, stream: 0 });
    await stop();

    start({ CAPSTONE_NATIVE_CHAT: '1', CAPSTONE_NATIVE_INDEXING: '1' });
    health = (await run('health')).data;
    assert.equal(health.commands.length, 20);
    assert.ok(health.commands.every((c) => c.available), 'all frozen commands must be installed');
    assert.deepEqual(provider.counts, { embedding: 0, model: 0, stream: 0 }, 'startup is provider-free');
    const owner = backend.child.pid;
    assert.equal((await run('listPapers')).data.length, 1);
    assert.equal((await run('getPaper', { paper_id: 'Fixture' })).data.id, 'Fixture');
    const preview = (await run('readPdf', { paper_id: 'Fixture' })).data;
    assert.equal(preview.total_bytes, pdf.length);
    assert.equal(preview.bytes, undefined);
    assert.deepEqual(Buffer.from((await run('readPdfRange', { preview_id: preview.preview_id, begin: 0, end: pdf.length })).data.bytes), Buffer.from(pdf));
    await run('closePdf', { preview_id: preview.preview_id });
    const indexed = await run('indexPapersStream', { reindex: false });
    assert.equal(indexed.events.at(-1).indexed, 1);
    assert.equal(indexed.events.at(-1).failed, 0);
    assert.equal((await run('indexPapers', { reindex: false })).data.summary.skipped, 1);
    assert.equal((await run('paperStatuses')).data.statuses[0].status, 'indexed');
    assert.equal((await run('bm25Status')).data.built, true);
    assert.equal((await run('searchPapers', { query: 'synthetic', limit: 8 })).data.matches[0].paper_id, 'Fixture');
    const chunks = await raw('retrieve_chunks', { query: 'synthetic', strategy: 'hybrid', scope: 'chat_candidates', limit: 5 });
    assert.equal(chunks.chunks[0].source.paper_id, 'Fixture');
    assert.ok((await run('graphNeighborhood', { node_id: 'Fixture', max_depth: 2 })).data.neighborhood.nodes.length >= 5);
    assert.equal((await raw('graph_query', { query: 'paper_details', paper_id: 'Fixture' })).paper.paper_id, 'Fixture');
    for (const [command, params] of [
      ['search_papers', { query: '', limit: 8 }], ['search_papers', { query: 'x', limit: 101 }],
      ['retrieve_chunks', { query: 'x', strategy: 'bm25', scope: 'chat_candidates', limit: 5 }],
      ['graph_neighborhood', { node_id: 'Fixture', max_depth: 6 }],
      ['graph_query', { query: 'paper_details', paper_id: 'Fixture', extra: true }],
      ['evaluate_stream', { retrieval_type: 'vector', test_cases: [] }],
    ]) await assert.rejects(raw(command, params), { code: 'invalid_params' });
    const cases = [{ question: 'What does the synthetic sample say?', grading_notes: 'Fixture evidence.', sources: ['Fixture.pdf'] }];
    const [chat, evaluation] = await Promise.all([
      run('chatStream', { session_id: 'fixture-session', turn_id: 'fixture-turn', message: cases[0].question }),
      run('runEvaluation', { retrieval_type: 'hybrid', test_cases: cases }),
    ]);
    assert.equal(chat.events.at(-1).historyIndexStatus, 'indexed');
    assert.equal(chat.events.at(-1).sources[0].paper_id, 'Fixture');
    assert.equal(evaluation.events.at(-1).succeeded, 1);
    assert.deepEqual(evaluation.events.map((e) => e.event), ['runStart', 'caseStart', 'caseSuccess', 'complete']);
    assert.equal((await run('listSessions')).data.sessions.length, 1, 'evaluation cannot create Chat sessions');
    assert.equal((await run('getMessages', { session_id: 'fixture-session' })).data.messages.length, 2);
    assert.equal((await run('search', { query: 'synthetic', limit: 5 })).data.matches.length, 1);
    const second = await run('chat', { session_id: 'fixture-session-2', turn_id: 'fixture-turn-2', message: cases[0].question });
    assert.equal(second.data.history_index_status, 'indexed');
    assert.equal(backend.child.pid, owner, 'concurrent commands use exactly one backend');
    const artifact = await readFile(join(data, 'evaluations/eval_results.json'));
    assert.equal(JSON.parse(artifact).results.length, 1);
    await stop();

    const before = { ...provider.counts };
    start({ CAPSTONE_NATIVE_CHAT: '1', CAPSTONE_NATIVE_INDEXING: '1' });
    await run('health');
    assert.equal((await run('getMessages', { session_id: 'fixture-session' })).data.messages.length, 2);
    assert.equal((await run('graphNeighborhood', { node_id: 'Fixture', max_depth: 1 })).data.neighborhood.nodes.length, 5);
    assert.equal((await run('paperStatuses')).data.statuses[0].status, 'indexed');
    await run('chat', { session_id: 'fixture-session-2', turn_id: 'fixture-turn-2', message: cases[0].question });
    assert.deepEqual(provider.counts, before, 'restart and duplicate turn do not replay provider calls');
    assert.deepEqual(await readFile(join(data, 'evaluations/eval_results.json')), artifact);
    const child = backend.child;
    const closed = once(child, 'close');
    child.kill('SIGKILL');
    await closed;
    assert.equal(backend.child, null);
    assert.equal(backend.pending.size, 0);
    assert.deepEqual(provider.counts, before, 'crash never triggers automatic replay');
    await run('health');
    assert.equal((await run('getMessages', { session_id: 'fixture-session' })).data.messages.length, 2);
    assert.deepEqual(provider.counts, before, 'explicit restart remains provider-free');
    const reset = (await run('resetAllIndices')).data;
    assert.equal(reset.preserved.history, true);
    assert.equal((await run('getMessages', { session_id: 'fixture-session' })).data.messages.length, 2);
    assert.equal((await run('bm25Status')).data.chunk_count, 0);
    assert.deepEqual(await readFile(join(papers, 'Fixture.pdf')), Buffer.from(pdf));
    // Damage only this disposable journal: every newly wired derived command
    // must refuse reads/providers rather than bypass the shared readiness gate.
    const beforeDamage = { ...provider.counts };
    await writeFile(join(data, 'index-state.sqlite3'), 'synthetic damaged journal');
    for (const [command, params] of [
      ['graph_neighborhood', { node_id: 'Fixture', max_depth: 1 }],
      ['graph_query', { query: 'paper_details', paper_id: 'Fixture' }],
      ['search_papers', { query: 'synthetic', limit: 8 }],
      ['retrieve_chunks', { query: 'synthetic', strategy: 'hybrid', scope: 'chat_candidates', limit: 5 }],
      ['evaluate_stream', { retrieval_type: 'hybrid', test_cases: cases }],
    ]) await assert.rejects(raw(command, params), { code: 'unavailable' });
    assert.deepEqual(provider.counts, beforeDamage, 'failed readiness never calls a provider');
    assert.equal((await run('getMessages', { session_id: 'fixture-session' })).data.messages.length, 2);
    await stop();
    return { commands: 19, pythonFreePath: true, concurrentChatEvaluation: true,
      restartPersistence: true, fixtureResetOnly: true, providerCalls: provider.counts };
  } finally { await backend?.stop(); await provider.close(); }
}