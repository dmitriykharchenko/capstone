import { version } from 'pdfjs-dist/package.json';
import { createPdfLoader, PdfLoadError, type PdfLoad, type PdfLoadOptions } from './pdf-lifecycle';
import { api, type PdfData } from './api';
export { PdfLoadError } from './pdf-lifecycle';
export type { PdfLoad, PdfLoadOptions, PdfPasswordPrompt } from './pdf-lifecycle';
export const PDFJS_VERSION = version;
export const PDFJS_ASSET_PATH = `/assets/pdfjs-${version}/`;

/** No browser/PDF.js side effects at import time (SSR and test discovery safe). */
export function loadPdf(pdf: PdfData, options?: PdfLoadOptions): PdfLoad {
  const loader = createPdfLoader({
    assetBase: typeof window === 'undefined' ? '' : new URL(PDFJS_ASSET_PATH, window.location.href).href,
    async loadLibrary() {
      if (typeof window === 'undefined' || typeof Worker === 'undefined') throw new PdfLoadError('unavailable');
      const page = new URL(window.location.href);
      if (page.username || page.password || !(page.protocol === 'app:' && page.hostname === 'research' && !page.port ||
          page.origin === 'http://127.0.0.1:5173')) throw new PdfLoadError('unavailable');
      const url = new URL(`${PDFJS_ASSET_PATH}build/pdf.mjs`, page).href;
      const library: typeof import('pdfjs-dist') = await import(/* @vite-ignore */ url);
      if (library.version !== version) throw new PdfLoadError('unavailable');
      return library;
    },
    createWorker(library, failed) {
      // Supplying a real port avoids PDF.js's blob wrapper for custom-scheme
      // origins and its automatic fake-worker fallback. Each load owns its port.
      const port = new Worker(new URL(`${PDFJS_ASSET_PATH}build/pdf.worker.mjs`, window.location.href), { type: 'module' });
      port.addEventListener('error', failed);
      port.addEventListener('messageerror', failed);
      try {
        const worker = library.PDFWorker.create({ port, verbosity: 0 });
        return { worker, dispose() {
          port.removeEventListener('error', failed);
          port.removeEventListener('messageerror', failed);
          worker.destroy();
          port.terminate();
        } };
      } catch {
        port.terminate();
        throw new PdfLoadError('unavailable');
      }
    },
  });
  return loader({ length: pdf.total_bytes,
    read(begin, end) {
      const request = api.readPdfRange(pdf.preview_id, begin, end);
      return Object.assign(request.then(result => {
        if (result.preview_id !== pdf.preview_id || result.offset !== begin || result.bytes.length !== end - begin) throw new PdfLoadError('invalid');
        return new Uint8Array(result.bytes);
      }), { cancel: request.cancel });
    },
    close: () => api.closePdf(pdf.preview_id),
  }, options);
}