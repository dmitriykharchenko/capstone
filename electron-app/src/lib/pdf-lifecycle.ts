import type { PDFDocumentProxy, PDFDocumentLoadingTask, PDFWorker, TextLayer } from 'pdfjs-dist';

export const MAX_PDF_INPUT_BYTES = 64 * 1024 * 1024;
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

/** Internal dependency seam for Node lifecycle tests; the app uses loadPdf only. */
export function createPdfLoader(environment: {
  assetBase: string;
  loadLibrary(): Promise<PdfLibrary>;
  createWorker(library: PdfLibrary, failed: () => void): { worker: PDFWorker; dispose(): void };
}) {
  return (bytes: readonly number[] | Uint8Array, options: PdfLoadOptions = {}): PdfLoad => {
    let data: Uint8Array | undefined;
    let loading: PDFDocumentLoadingTask | undefined;
    let owner: ReturnType<typeof environment.createWorker> | undefined;
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
      data = undefined;
      signal?.removeEventListener('abort', abort);
      reject(new PdfLoadError('cancelled'));
      const task = loading;
      const ownedWorker = owner;
      loading = undefined;
      owner = undefined;
      disposal = (async () => {
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
          if (task) {
            task.onPassword = () => {};
            // A crashed/non-responsive worker cannot acknowledge Terminate forever.
            await Promise.race([task.destroy().catch(() => {}), new Promise<void>((done) => {
              timer = setTimeout(done, PDF_DESTROY_GRACE_MS);
            })]);
          }
        } finally {
          clearTimeout(timer);
          ownedWorker?.dispose();
        }
      })();
      return disposal;
    }
    function fail(code: ConstructorParameters<typeof PdfLoadError>[0]) {
      if (stopped) return;
      reject(new PdfLoadError(code));
      void destroy();
    }

    signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) abort();
    if (!stopped) {
      if (!(Array.isArray(bytes) || bytes instanceof Uint8Array) || bytes.length === 0 ||
          bytes.length > MAX_PDF_INPUT_BYTES) fail('invalid');
      else {
        // Capture now; never transfer/detach the caller's buffer or retain its number[].
        data = new Uint8Array(bytes.length);
        for (let i = 0; i < bytes.length; i++) {
          const byte = bytes[i];
          if (!Number.isInteger(byte) || byte < 0 || byte > 255) { fail('invalid'); break; }
          data[i] = byte;
        }
      }
    }
    if (!stopped) void (async () => {
      try {
        const library = await environment.loadLibrary();
        if (stopped) return;
        owner = environment.createWorker(library, () => fail('unavailable'));
        if (stopped) { owner.dispose(); owner = undefined; return; }
        loading = library.getDocument({
          data, worker: owner.worker, verbosity: 0,
          cMapUrl: `${environment.assetBase}cmaps/`, cMapPacked: true,
          standardFontDataUrl: `${environment.assetBase}standard_fonts/`,
          wasmUrl: `${environment.assetBase}wasm/`,
          useWasm: false, useWorkerFetch: true, useSystemFonts: false,
          enableXfa: false, stopAtErrors: true,
          canvasMaxAreaInBytes: 32 * 1024 * 1024,
        });
        data = undefined;
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