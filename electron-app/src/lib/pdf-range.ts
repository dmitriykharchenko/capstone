import type { PDFDataRangeTransport } from 'pdfjs-dist';
import type { CancellablePromise } from './api';

export const PDF_RANGE_BYTES = 64 * 1024;
export const PDF_PENDING_RANGES = 8;
export type PdfSource = {
  length: number;
  read(begin: number, end: number): CancellablePromise<Uint8Array>;
  close(): Promise<unknown>;
};

/** PDF.js may coalesce an arbitrary span. Only its requested response buffer is
 * allocated here (approved PDF.js storage); native/Electron reads stay 64KiB.
 * No eager fetch, document copy/cache, URLs, paths, or private PDF.js APIs. */
export function pdfRangeTransport(library: { PDFDataRangeTransport: typeof PDFDataRangeTransport },
  source: PdfSource, failed: () => void) {
  let stopped = false, pumping = false;
  let active: CancellablePromise<Uint8Array> | undefined;
  let close: Promise<unknown> | undefined;
  const queue: { begin: number; end: number }[] = [];
  class Transport extends library.PDFDataRangeTransport {
    requestDataRange(begin: number, end: number) {
      if (stopped) return;
      if (!Number.isSafeInteger(begin) || !Number.isSafeInteger(end) || begin < 0 || begin >= end ||
          end > source.length || queue.length + Number(pumping) >= PDF_PENDING_RANGES) { failed(); return; }
      queue.push({ begin, end });
      void pump();
    }
    abort() { void release(); }
  }
  const transport = new Transport(source.length, new Uint8Array(), true);
  async function pump() {
    if (pumping || stopped) return;
    pumping = true;
    try {
      while (queue.length && !stopped) {
        const { begin, end } = queue.shift()!;
        const bytes = new Uint8Array(end - begin);
        for (let offset = begin; offset < end && !stopped; offset += PDF_RANGE_BYTES) {
          const limit = Math.min(end, offset + PDF_RANGE_BYTES);
          active = source.read(offset, limit);
          const chunk = await active;
          active = undefined;
          if (stopped) return;
          if (!(chunk instanceof Uint8Array) || chunk.length !== limit - offset) throw new Error('Invalid PDF range.');
          bytes.set(chunk, offset - begin);
        }
        if (!stopped) transport.onDataRange(begin, bytes);
      }
    } catch { if (!stopped) failed(); }
    finally { active = undefined; pumping = false; }
  }
  function release(): Promise<unknown> {
    if (close) return close;
    stopped = true;
    queue.length = 0;
    active?.cancel?.();
    try { close = Promise.resolve(source.close()).catch(() => {}); }
    catch { close = Promise.resolve(); }
    return close;
  }
  return { transport, release };
}