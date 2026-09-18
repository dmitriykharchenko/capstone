import { createInterface } from 'node:readline';
const scenario = process.argv[2] ?? 'normal';
const active = new Set();
const send = (value) => process.stdout.write(JSON.stringify(value) + '\n');
createInterface({ input: process.stdin }).on('line', (line) => {
  const req = JSON.parse(line);
  const { id, command, params } = req;
  if (req.type === 'cancel') {
    const accepted = active.delete(id);
    if (scenario === 'ignore-cancel') return;
    if (accepted) send({ v: 1, id, seq: 0, type: 'cancelled' });
    send({ v: 1, id, type: 'cancel_ack', accepted });
    return;
  }
  if (command === 'hold') { active.add(id); return; }
  if (scenario === 'crash') { process.stderr.write('PRIVATE-FIXTURE-DIAGNOSTIC'); process.exit(2); }
  if (scenario === 'truncated') { process.stdout.write('{"v":1'); process.exit(1); }
  if (scenario === 'invalid-utf8') { process.stdout.write(Buffer.from([255, 10])); return; }
  if (scenario === 'oversized') { process.stdout.write('x'.repeat(1_048_577)); return; }
  if (scenario === 'out-of-order') { send({ v: 1, id, seq: 1, type: 'result', data: {} }); return; }
  if (scenario === 'missing-data') { send({ v: 1, id, seq: 0, type: 'result' }); return; }
  if (scenario === 'wrong-id') { send({ v: 1, id: 'unknown', seq: 0, type: 'result', data: {} }); return; }
  if (scenario === 'raw-error') {
    send({ v: 1, id, seq: 0, type: 'error', error: { code: 'internal', message: 'PRIVATE-FIXTURE-DIAGNOSTIC' } }); return;
  }
  let seq = 0;
  const event = (event, data) => send({ v: 1, id, seq: seq++, type: 'event', event, data });
  let data;
  if (command === 'read_pdf') {
    event('pdf_chunk', { offset: 0, bytes: [37, 80] });
    event('pdf_chunk', { offset: scenario === 'pdf-gap' ? 3 : 2, bytes: [68, 70] });
    data = { id: params.paper_id, filename: 'paper.pdf', mime_type: 'application/pdf', total_bytes: 4 };
  } else if (command === 'chat_stream') {
    const count = scenario === 'flood' ? 40 : 2;
    for (let i = 0; i < count; i++) event('delta', { delta: 'answer' });
    data = { session_id: params.session_id, reply: 'answer', chat_mode: 'rag', indexed: true, sources: [] };
  } else if (command === 'index_papers_stream') {
    event('started', { completed: 0, total: 1, paper_id: 'p', filename: 'paper.pdf' });
    event('progress', { completed: 1, total: 1, paper_id: 'p', filename: 'paper.pdf', status: 'indexed', chunk_count: 1, error: null });
    data = { completed: 1, total: 1, indexed: 1, empty: 0, failed: 0, skipped: 0 };
  } else if (command === 'health') {
    data = { ok: true, protocol_version: 1, backend: 'rust', stage: 'fixture', chat_mode: 'unavailable', search_enabled: false,
      commands: [{ command: 'health', owner: 'runtime', streaming: false, available: true }], pid: process.pid };
  } else { data = { command, params, pid: process.pid }; }
  const frame = JSON.stringify({ v: 1, id, seq, type: 'result', data }) + '\n';
  if (scenario === 'fragmented') {
    for (const byte of Buffer.from(frame)) process.stdout.write(Buffer.from([byte]));
  } else process.stdout.write(frame);
}).on('close', () => process.exit(0));