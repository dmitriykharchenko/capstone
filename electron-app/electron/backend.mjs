import { spawn } from 'node:child_process';

export const MAX_FRAME_BYTES = 1_048_576;
export const MAX_PENDING = 32;
const MESSAGES = {
  unavailable: 'This feature is not integrated in this build yet.',
  busy: 'The backend is busy. Please retry after current work finishes.',
  cancelled: 'Request cancelled.',
  invalid_params: 'Invalid command parameters.',
  transport: 'The backend connection closed. The operation outcome is unknown; check its state before retrying. No operation was replayed.',
  protocol: 'The backend returned an invalid response. The operation outcome is unknown; check its state before retrying.',
};
export function publicError(code = 'transport') {
  const error = new Error(MESSAGES[code] ?? 'Backend operation failed.');
  error.code = code;
  return error;
}

// One owner, one child, no automatic request replay (especially mutations).
export class Backend {
  constructor({ executable, args = [], cwd, env = {}, spawnChild = spawn,
    rotateAfter = 4000, requestTimeout = 30 * 60_000, cancelTimeout = 5000,
    shutdownTimeout = 30_000, killTimeout = 1000 } = {}) {
    for (const timeout of [shutdownTimeout, killTimeout]) {
      if (!Number.isSafeInteger(timeout) || timeout < 1 || timeout > 2_147_483_647) throw new RangeError('Invalid shutdown deadline.');
    }
    Object.assign(this, { executable, args, cwd, env, spawnChild, requestTimeout, cancelTimeout, shutdownTimeout, killTimeout });
    this.rotateAfter = Math.min(4000, Math.max(1, rotateAfter));
    this.pending = new Map();
    this.acks = new Map();
    this.count = 0;
    this.child = null;
    this.stopping = null;
    this.teardown = null;
    this.closed = false;
    this.failed = false;
    this.buffer = Buffer.alloc(0);
  }

  async ready() {
    if (this.closed) throw publicError();
    if (this.stopping) await this.stopping;
    if (this.count >= this.rotateAfter) {
      if (this.pending.size || this.acks.size) throw publicError('busy');
      await this.stop(false);
    }
    if (this.closed || this.stopping) throw publicError();
    if (this.child) return;
    const child = this.spawnChild(this.executable, this.args, {
      cwd: this.cwd, env: this.env, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true,
    });
    this.child = child;
    this.failed = false;
    this.count = 0;
    this.buffer = Buffer.alloc(0);
    child.stdout.on('data', (bytes) => {
      if (this.child !== child || this.failed) return;
      try { this.consume(bytes); } catch { void this.fail('protocol'); }
    });
    // Drain, never accumulate, log, or forward backend diagnostics/credentials.
    child.stderr.on('data', () => {});
    child.stdout.on('end', () => {
      if (this.child !== child) return;
      this.rejectAll(); // All stdout has drained; remaining outcomes are unknown.
      void this.stop(false);
    });
    child.stdin.on('error', () => { if (this.child === child) void this.fail(); });
    child.on('error', () => { if (this.child === child) void this.fail(); });
    // 'exit' can precede the last stdout data. Only EOF/close loses a result.
    child.on('close', () => {
      if (this.child !== child) return;
      this.rejectAll();
      this.child = null;
      this.count = 0;
      this.buffer = Buffer.alloc(0);
      this.teardown?.finish();
      this.teardown = null;
      this.stopping = null;
    });
  }

  request(command, params, { onEvent = () => {}, signal } = {}) {
    let id;
    let submitted;
    let cancelled = false;
    let rejectLocal;
    const cancel = () => {
      cancelled = true;
      if (!id) { rejectLocal(publicError('cancelled')); return; }
      const job = this.pending.get(id);
      if (!job || job !== submitted || job.cancelled) return;
      job.cancelled = true;
      if (this.stopping) return; // EOF already requests cancellation and drains commits.
      // A missing acknowledgment starts bounded EOF retirement, not a cancel-
      // kill timer. An acknowledgment (including refusal) is NOT terminal.
      this.acks.set(id, setTimeout(() => { void this.stop(false); }, this.cancelTimeout));
      try { this.write({ v: 1, type: 'cancel', id }); } catch { void this.fail(); }
    };
    const done = new Promise((resolve, reject) => {
      rejectLocal = reject;
      void (async () => {
        if (signal?.aborted) throw publicError('cancelled');
        await this.ready();
        if (cancelled || signal?.aborted) throw publicError('cancelled');
        if (this.closed) throw publicError();
        if (this.stopping) throw publicError('busy');
        if (this.pending.size >= MAX_PENDING || this.acks.size >= MAX_PENDING ||
            this.count >= this.rotateAfter) throw publicError('busy');
        id = `r${++this.count}`;
        const timer = setTimeout(() => { void this.fail(); }, this.requestTimeout);
        submitted = { resolve, reject, seq: 0, onEvent, timer, cancelled: false };
        this.pending.set(id, submitted);
        try { this.write({ v: 1, type: 'request', id, command, params }); }
        catch { void this.fail(); }
      })().catch(reject);
    }).finally(() => signal?.removeEventListener('abort', cancel));
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
      job.onEvent(frame.event, frame.data);
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
    if (this.failed) return this.stopping;
    this.failed = true;
    this.rejectAll(code);
    const stopped = this.stop(false);
    this.teardown?.force(); // A genuine transport/operation deadline, not refusal.
    return stopped;
  }

  stop(permanent = true) {
    if (permanent) this.closed = true;
    if (this.stopping) return this.stopping;
    const child = this.child;
    if (!child) return Promise.resolve();
    // Preserve original operation deadlines while EOF drains sealed writes.
    // Independently allow 30s for shutdown/retirement, then SIGKILL and at most
    // 1s for close (both injectable). Forced outcomes are UNKNOWN, never rollback.
    for (const timer of this.acks.values()) clearTimeout(timer);
    let resolve, deadline, killTimer, finished = false, forced = false;
    const stopping = this.stopping = new Promise((done) => { resolve = done; });
    const finish = () => {
      if (finished) return;
      finished = true;
      clearTimeout(deadline);
      clearTimeout(killTimer);
      resolve();
      // Without confirmed close, keep the retired child/stopping guard. A
      // bounded shutdown attempt is not proof it exited; never overlap children.
    };
    const force = () => {
      if (finished || forced) return;
      forced = true;
      clearTimeout(deadline);
      this.failed = true;
      this.rejectAll();
      killTimer = setTimeout(finish, this.killTimeout);
      try { child.kill('SIGKILL'); } catch { /* Still unknown; bounded wait remains. */ }
    };
    this.teardown = { finish, force };
    deadline = setTimeout(force, this.shutdownTimeout);
    try { child.stdin.end(); } catch { void this.fail(); }
    return stopping;
  }
}