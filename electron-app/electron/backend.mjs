import { spawn } from 'node:child_process';

export const MAX_FRAME_BYTES = 1_048_576;
export const MAX_PENDING = 32;
const MESSAGES = {
  unavailable: 'This feature is not integrated in this build yet.',
  busy: 'The backend is busy. Please retry after current work finishes.',
  cancelled: 'Request cancelled.',
  invalid_params: 'Invalid command parameters.',
  transport: 'The backend connection closed. Please retry; no operation was replayed.',
  protocol: 'The backend returned an invalid response.',
};
export function publicError(code = 'transport') {
  const error = new Error(MESSAGES[code] ?? 'Backend operation failed.');
  error.code = code;
  return error;
}

// One owner, one child, no automatic request replay (especially mutations).
export class Backend {
  constructor({ executable, args = [], cwd, env = {}, spawnChild = spawn,
    rotateAfter = 4000, requestTimeout = 30 * 60_000, cancelTimeout = 5000 } = {}) {
    Object.assign(this, { executable, args, cwd, env, spawnChild, requestTimeout, cancelTimeout });
    this.rotateAfter = Math.min(4000, Math.max(1, rotateAfter));
    this.pending = new Map();
    this.acks = new Map();
    this.count = 0;
    this.child = null;
    this.stopping = null;
    this.closed = false;
    this.buffer = Buffer.alloc(0);
  }

  async ready() {
    if (this.closed) throw publicError();
    if (this.stopping) await this.stopping;
    if (this.count >= this.rotateAfter) {
      if (this.pending.size || this.acks.size) throw publicError('busy');
      await this.stop(false);
    }
    if (this.closed) throw publicError();
    if (this.child) return;
    const child = this.spawnChild(this.executable, this.args, {
      cwd: this.cwd, env: this.env, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true,
    });
    this.child = child;
    this.count = 0;
    this.buffer = Buffer.alloc(0);
    child.stdout.on('data', (bytes) => {
      if (this.child !== child || this.stopping) return;
      try { this.consume(bytes); } catch { void this.fail('protocol'); }
    });
    // Drain, never accumulate, log, or forward backend diagnostics/credentials.
    child.stderr.on('data', () => {});
    child.stdout.on('end', () => { if (!this.stopping) void this.fail(); });
    child.stdin.on('error', () => { void this.fail(); });
    child.on('error', () => { void this.fail(); });
    child.on('exit', () => this.rejectAll());
    child.on('close', () => {
      if (this.child !== child) return;
      this.rejectAll();
      this.child = null;
      this.count = 0;
      this.buffer = Buffer.alloc(0);
    });
  }

  request(command, params, { onEvent = () => {}, signal } = {}) {
    let id;
    let cancelled = false;
    const cancel = () => {
      cancelled = true;
      if (!id) return;
      const job = this.pending.get(id);
      if (!job || job.cancelled) return;
      job.cancelled = true;
      job.reject(publicError('cancelled'));
      clearTimeout(job.timer);
      job.timer = setTimeout(() => { void this.fail(); }, this.cancelTimeout);
      this.acks.set(id, setTimeout(() => { void this.fail(); }, this.cancelTimeout));
      try { this.write({ v: 1, type: 'cancel', id }); } catch { void this.fail(); }
    };
    const done = (async () => {
      if (signal?.aborted) throw publicError('cancelled');
      await this.ready();
      if (cancelled || signal?.aborted) throw publicError('cancelled');
      if (this.pending.size >= MAX_PENDING || this.acks.size >= MAX_PENDING ||
          this.count >= this.rotateAfter) throw publicError('busy');
      id = `r${++this.count}`;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { void this.fail(); }, this.requestTimeout);
        this.pending.set(id, { resolve, reject, seq: 0, onEvent, timer, cancelled: false });
        try { this.write({ v: 1, type: 'request', id, command, params }); }
        catch { void this.fail(); }
      });
    })().finally(() => signal?.removeEventListener('abort', cancel));
    signal?.addEventListener('abort', cancel, { once: true });
    return { done, cancel };
  }

  write(frame) {
    const line = JSON.stringify(frame);
    const size = Buffer.byteLength(line);
    if (!this.child || this.stopping || size > MAX_FRAME_BYTES ||
        this.child.stdin.writableLength + size > MAX_FRAME_BYTES * 2) throw publicError();
    this.child.stdin.write(line + '\n');
  }

  consume(bytes) {
    let start = 0;
    while (start < bytes.length) {
      const newline = bytes.indexOf(10, start);
      const end = newline < 0 ? bytes.length : newline;
      if (this.buffer.length + end - start > MAX_FRAME_BYTES) throw publicError('protocol');
      this.buffer = Buffer.concat([this.buffer, bytes.subarray(start, end)]);
      if (newline < 0) return;
      const text = new TextDecoder('utf-8', { fatal: true }).decode(this.buffer);
      this.buffer = Buffer.alloc(0);
      this.frame(JSON.parse(text));
      start = end + 1;
    }
  }

  frame(frame) {
    if (!frame || frame.v !== 1 || typeof frame.id !== 'string') throw publicError('protocol');
    const fields = { event: ['seq', 'event', 'data'], result: ['seq', 'data'], error: ['seq', 'error'],
      cancelled: ['seq'], cancel_ack: ['accepted'] };
    if (!Object.hasOwn(fields, frame.type) || Object.keys(frame).some((key) =>
      !['v', 'id', 'type', ...fields[frame.type]].includes(key))) throw publicError('protocol');
    if (frame.type === 'cancel_ack') {
      if (typeof frame.accepted !== 'boolean' || !this.acks.has(frame.id)) throw publicError('protocol');
      clearTimeout(this.acks.get(frame.id));
      this.acks.delete(frame.id);
      return;
    }
    const job = this.pending.get(frame.id);
    if (!job || frame.seq !== job.seq++) throw publicError('protocol');
    if (frame.type === 'event') {
      if (typeof frame.event !== 'string' || !Object.hasOwn(frame, 'data')) throw publicError('protocol');
      if (!job.cancelled) job.onEvent(frame.event, frame.data);
      return;
    }
    if (!['result', 'error', 'cancelled'].includes(frame.type)) throw publicError('protocol');
    if (frame.type === 'result' && !Object.hasOwn(frame, 'data')) throw publicError('protocol');
    if (frame.type === 'error' && typeof frame.error?.code !== 'string') throw publicError('protocol');
    this.pending.delete(frame.id);
    clearTimeout(job.timer);
    if (frame.type === 'result') job.resolve(frame.data);
    else if (frame.type === 'cancelled') job.reject(publicError('cancelled'));
    else job.reject(publicError(frame.error.code));
  }

  rejectAll(code = 'transport') {
    for (const job of this.pending.values()) {
      clearTimeout(job.timer);
      job.reject(publicError(code));
    }
    this.pending.clear();
    for (const timer of this.acks.values()) clearTimeout(timer);
    this.acks.clear();
  }

  fail(code = 'transport') {
    this.rejectAll(code);
    return this.stop(false);
  }

  stop(permanent = true) {
    if (permanent) this.closed = true;
    if (this.stopping) return this.stopping;
    this.rejectAll();
    const child = this.child;
    if (!child) return Promise.resolve();
    this.stopping = new Promise((resolve) => {
      const timer = setTimeout(() => child.kill('SIGKILL'), 1000);
      child.once('close', () => { clearTimeout(timer); resolve(); });
      child.stdin.end();
    }).finally(() => { this.stopping = null; });
    return this.stopping;
  }
}