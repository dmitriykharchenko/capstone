import { publicError } from './backend.mjs';
import { pdfInfo, pdfRange } from './adapters.mjs';
import { requireValid } from './validation.mjs';

export const MAX_PDF_SESSIONS = 4;
export const MAX_PDF_PENDING = 8;
export const PDF_IDLE_MS = 60_000;

// Native read_pdf is a lease, not a file transfer. Only the trusted broker owns
// the native request/cancellation handle; renderers see a pathless random ID.
export class PdfSessions {
  constructor(backend) { this.backend = backend; this.sessions = new Map(); this.pending = new Map(); }
  refresh(entry) {
    clearTimeout(entry.timer);
    entry.timer = setTimeout(() => this.close(entry), PDF_IDLE_MS);
    entry.timer.unref?.();
  }
  close(entry, error = publicError('pdf_closed')) {
    if (entry.closed) return;
    entry.closed = true;
    entry.error = error;
    clearTimeout(entry.timer);
    this.sessions.delete(entry.key);
    entry.reject(error);
    entry.job?.cancel();
    for (const job of this.pending.values()) if (job.entry === entry) job.cancel();
  }
  open(owner, token, paperId) {
    if (this.sessions.size >= MAX_PDF_SESSIONS) throw publicError('busy');
    const key = `${owner}:${token}`;
    requireValid(!this.sessions.has(key));
    let resolve, reject;
    const ready = new Promise((yes, no) => { resolve = yes; reject = no; });
    const entry = { owner, token, key, reject, closed: false, info: null };
    this.sessions.set(key, entry);
    this.refresh(entry);
    try {
      entry.job = this.backend.request('read_pdf', { paper_id: paperId }, { timeout: 0, onEvent: (name, data) => {
        try {
          requireValid(!entry.closed && !entry.info && name === 'pdf_open');
          entry.info = pdfInfo(paperId, data);
          requireValid(![...this.sessions.values()].some(other => other !== entry && other.info?.preview_id === entry.info.preview_id));
          this.refresh(entry);
          resolve(entry.info);
        } catch { this.close(entry, publicError('pdf_invalid')); }
      } });
      void entry.job.done.then(() => this.close(entry), error => this.close(entry, publicError(error?.code ?? 'pdf_closed')));
      if (entry.closed) entry.job.cancel();
    } catch { this.close(entry); }
    return ready;
  }
  owned(owner, id) {
    const entry = [...this.sessions.values()].find(entry => entry.owner === owner && entry.info?.preview_id === id && !entry.closed);
    if (!entry) throw publicError('pdf_closed');
    return entry;
  }
  async read(owner, token, args) {
    const entry = this.owned(owner, args.preview_id);
    const key = `${owner}:${token}`;
    requireValid(!this.pending.has(key));
    if (this.pending.size >= MAX_PDF_PENDING) throw publicError('busy');
    if (args.end > entry.info.total_bytes) { this.close(entry); throw publicError('pdf_invalid'); }
    let job;
    try {
      job = this.backend.request('read_pdf_range', args, { onEvent() { throw publicError('pdf_invalid'); } });
      this.pending.set(key, { ...job, entry });
      this.refresh(entry);
      const data = await job.done;
      if (entry.closed) throw entry.error;
      const result = pdfRange(args, data);
      this.refresh(entry);
      return result;
    } catch (error) {
      this.close(entry);
      throw error;
    } finally { if (job) this.pending.delete(key); }
  }
  cancel(owner, token) {
    const key = `${owner}:${token}`;
    const entry = this.sessions.get(key) ?? this.pending.get(key)?.entry;
    if (entry) this.close(entry);
  }
  cancelOwner(owner) {
    for (const entry of this.sessions.values()) if (entry.owner === owner) this.close(entry);
  }
  closeId(owner, id) { this.close(this.owned(owner, id)); return null; }
  invalid(owner, id) {
    try { this.closeId(owner, id); } catch { /* Unknown/foreign IDs never affect another owner. */ }
  }
}