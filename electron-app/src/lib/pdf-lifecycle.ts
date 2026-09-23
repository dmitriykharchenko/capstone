import type { PDFDocumentProxy, PDFDocumentLoadingTask, PDFWorker, TextLayer } from 'pdfjs-dist';
import { pdfRangeTransport, PDF_RANGE_BYTES, type PdfSource } from './pdf-range';

export const PDF_DESTROY_GRACE_MS = 1000;
type PdfLibrary = typeof import('pdfjs-dist');
export type PdfPasswordPrompt = {
  reason: 'required' | 'incorrect';
  /** Single use; false means this prompt is stale. Never dispatch/persist the value. */
  submit(password: string): boolean;
  cancel(): Promise<void>;
};
export type PdfLoadOptions = {
  signal?: AbortSignal;
  onPassword?: (prompt: PdfPasswordPrompt) => void;
  onError?: (error: PdfLoadError) => void;
};
export type PdfLoad = {
  /** Borrowed resources, valid only until destroy/signal abort. Do not store in Redux. */
  promise: Promise<{ document: PDFDocumentProxy; TextLayer: typeof TextLayer }>;
  /** Cancels loading AND destroys loaded documents. Idempotent, bounded worker drain. */
  destroy(): Promise<void>;
};
export class PdfLoadError extends Error {
  constructor(readonly code: 'cancelled' | 'invalid' | 'unavailable' | 'password') {
    super({ cancelled: 'PDF loading cancelled.', invalid: 'PDF is invalid or unsupported.',
      unavailable: 'Local PDF renderer unavailable.', password: 'PDF password required.' }[code]);
    this.name = 'PdfLoadError';
  }
}

/** Public getDocument.ownerDocument seam; never patch the application document. */
function ownFonts(document: Document) {
  const registered = document.fonts;
  if (!registered) throw new PdfLoadError('unavailable');
  const owned = new Set<FontFace>();
  let disposed = false;
  const remove = (font: FontFace) => owned.delete(font) && registered.delete(font);
  const clear = () => { for (const font of owned) remove(font); };
  const fonts = new Proxy(registered, {
    get(target, key) {
      if (key === 'add') return (font: FontFace) => {
        if (!disposed && !registered.has(font)) {
          registered.add(font);
          owned.add(font);
        }
        return fonts;
      };
      if (key === 'delete') return remove;
      if (key === 'clear') return clear;
      const value = Reflect.get(target, key, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
  const ownerDocument = new Proxy(document, {
    get(target, key) {
      if (key === 'fonts') return fonts;
      // Native DOM methods/getters require the real document as their receiver.
      const value = Reflect.get(target, key, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
  return { ownerDocument, dispose() { disposed = true; clear(); } };
}

/** Internal dependency seam for Node lifecycle tests; the app uses loadPdf only. */
export function createPdfLoader(environment: {
  assetBase: string;
  ownerDocument?: Document;
  loadLibrary(): Promise<PdfLibrary>;
  createWorker(library: PdfLibrary, failed: () => void): { worker: PDFWorker; dispose(): void };
}) {
  return (source: PdfSource, options: PdfLoadOptions = {}): PdfLoad => {
    let range: ReturnType<typeof pdfRangeTransport> | undefined;
    let loading: PDFDocumentLoadingTask | undefined;
    let owner: ReturnType<typeof environment.createWorker> | undefined;
    let fonts: ReturnType<typeof ownFonts> | undefined;
    let stopped = false;
    let promptGeneration = 0;
    let releasePassword: (() => void) | undefined;
    let onPassword = options.onPassword;
    const signal = options.signal;
    let disposal: Promise<void> | undefined;
    let resolve!: (value: { document: PDFDocumentProxy; TextLayer: typeof TextLayer }) => void;
    let reject!: (reason: PdfLoadError) => void;
    const promise: PdfLoad['promise'] = new Promise((yes, no) => { resolve = yes; reject = no; });
    // The caller still receives rejection; avoid an unhandled rejection on immediate close.
    void promise.catch(() => {});
    const abort = () => { void destroy(); };

    function clearPassword() {
      promptGeneration++;
      releasePassword?.();
      releasePassword = undefined;
    }
    function destroy(): Promise<void> {
      if (disposal) return disposal;
      stopped = true;
      clearPassword();
      onPassword = undefined;
      signal?.removeEventListener('abort', abort);
      reject(new PdfLoadError('cancelled'));
      const task = loading;
      const ownedWorker = owner;
      const ownedFonts = fonts;
      loading = undefined;
      owner = undefined;
      fonts = undefined;
      const closing = range ? range.release() : Promise.resolve().then(() => source?.close()).catch(() => {});
      range = undefined;
      disposal = (async () => {
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
          if (task) task.onPassword = () => {};
          // A crashed/non-responsive worker cannot acknowledge Terminate forever.
          await Promise.race([Promise.all([task?.destroy().catch(() => {}), closing]), new Promise<void>((done) => {
            timer = setTimeout(done, PDF_DESTROY_GRACE_MS);
          })]);
        } finally {
          clearTimeout(timer);
          // PDF.js may still be awaiting Terminate. Release only this load's
          // registrations now, and refuse late adds through its document seam.
          try { ownedFonts?.dispose(); } finally { ownedWorker?.dispose(); }
        }
      })();
      return disposal;
    }
    function fail(code: ConstructorParameters<typeof PdfLoadError>[0]) {
      if (stopped) return;
      reject(new PdfLoadError(code));
      void destroy();
      try { options.onError?.(new PdfLoadError(code)); } catch { /* Consumer failures cannot retain ownership. */ }
    }

    signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) abort();
    if (!stopped) {
      if (!source || !Number.isSafeInteger(source.length) || source.length <= 0 ||
          typeof source.read !== 'function' || typeof source.close !== 'function') fail('invalid');
    }
    if (!stopped) void (async () => {
      try {
        const library = await environment.loadLibrary();
        if (stopped) return;
        const documentContext = environment.ownerDocument ?? globalThis.document;
        if (documentContext) fonts = ownFonts(documentContext);
        owner = environment.createWorker(library, () => fail('unavailable'));
        if (stopped) { owner.dispose(); owner = undefined; return; }
        range = pdfRangeTransport(library, source, () => fail('invalid'));
        loading = library.getDocument({
          ownerDocument: fonts?.ownerDocument,
          range: range.transport, rangeChunkSize: PDF_RANGE_BYTES,
          disableAutoFetch: true, disableStream: true, worker: owner.worker, verbosity: 0,
          cMapUrl: `${environment.assetBase}cmaps/`, cMapPacked: true,
          standardFontDataUrl: `${environment.assetBase}standard_fonts/`,
          wasmUrl: `${environment.assetBase}wasm/`,
          useWasm: false, useWorkerFetch: true, useSystemFonts: false,
          enableXfa: false, stopAtErrors: true,
        });
        loading.onPassword = (update: (password: string) => void, reason: number) => {
          if (stopped) return;
          if (!onPassword) { fail('password'); return; }
          clearPassword();
          const generation = promptGeneration;
          let sendPassword: typeof update | undefined = update;
          releasePassword = () => { sendPassword = undefined; };
          let used = false;
          const prompt: PdfPasswordPrompt = {
            reason: reason === library.PasswordResponses.INCORRECT_PASSWORD ? 'incorrect' : 'required',
            submit(password) {
              if (stopped || used || generation !== promptGeneration || typeof password !== 'string' || !sendPassword) return false;
              used = true;
              const send = sendPassword;
              sendPassword = undefined;
              try { send(password); return true; } catch { fail('password'); return false; }
            },
            cancel: () => generation === promptGeneration && !used ? destroy() : Promise.resolve(),
          };
          try { onPassword(prompt); } catch { fail('password'); }
        };
        const document = await loading.promise;
        if (stopped) return;
        clearPassword();
        onPassword = undefined;
        loading.onPassword = () => {};
        resolve({ document, TextLayer: library.TextLayer });
      } catch { fail(loading ? 'invalid' : 'unavailable'); }
    })();
    return { promise, destroy };
  };
}