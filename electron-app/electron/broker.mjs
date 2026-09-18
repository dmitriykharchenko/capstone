import { MAX_FRAME_BYTES, MAX_PENDING, publicError } from './backend.mjs';
import { methods, requireValid, object, nonempty, parseResponse } from './validation.mjs';
import { PdfAssembler, parseEvent, terminalEvent } from './adapters.mjs';

const STREAMS = new Set(['chat_stream', 'index_papers_stream', 'evaluate_stream']);
const token = (v) => { nonempty(v); requireValid(/^[a-zA-Z0-9-]{1,64}$/.test(v)); return v; };
export class Broker {
  constructor(backend) { this.backend = backend; this.requests = new Map(); }
  async run(owner, payload, send) {
    let key;
    let pdf;
    let registered = false;
    try {
      requireValid(Buffer.byteLength(JSON.stringify(payload)) <= MAX_FRAME_BYTES - 256);
      object({ token, method: nonempty, args: (v) => v })(payload);
      requireValid(Object.hasOwn(methods, payload.method));
      key = `${owner}:${payload.token}`;
      requireValid(!this.requests.has(key));
      if (this.requests.size >= MAX_PENDING) throw publicError('busy');
      const [command, parse] = methods[payload.method];
      const params = parse(payload.args);
      // At most one 64MiB PDF assembly, not 32 simultaneously.
      if (command === 'read_pdf' && [...this.requests.values()].some((job) => job.pdf)) throw publicError('busy');
      pdf = command === 'read_pdf' ? new PdfAssembler(params.paper_id) : null;
      let sent = 0;
      const unacked = new Set();
      const publish = (event) => {
        if (unacked.size >= 32) throw publicError('busy');
        const seq = sent++;
        unacked.add(seq);
        send(payload.token, event, seq);
      };
      const job = this.backend.request(command, params, { onEvent: (name, data) => {
        if (pdf) pdf.event(name, data);
        else publish(parseEvent(command, name, data));
      } });
      this.requests.set(key, { ...job, owner, pdf, unacked });
      registered = true;
      const data = await job.done;
      if (pdf) return { ok: true, data: pdf.result(data) };
      if (STREAMS.has(command)) {
        publish(terminalEvent(command, data));
        return { ok: true, data: null };
      }
      return { ok: true, data: parseResponse(command, data) };
    } catch (error) {
      return { ok: false, error: publicError(error?.code ?? 'protocol').message };
    } finally {
      pdf?.clear();
      if (registered) this.requests.delete(key);
    }
  }
  cancel(owner, id) {
    token(id);
    const job = this.requests.get(`${owner}:${id}`);
    job?.pdf?.clear();
    job?.cancel();
  }
  cancelOwner(owner) {
    for (const job of this.requests.values()) if (job.owner === owner) { job.pdf?.clear(); job.cancel(); }
  }
  acknowledge(owner, id, seq) {
    token(id);
    requireValid(Number.isSafeInteger(seq) && seq >= 0);
    this.requests.get(`${owner}:${id}`)?.unacked.delete(seq);
  }
}