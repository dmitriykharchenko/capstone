import { api, type CancellablePromise, type PdfData } from './api';
import { loadPdf, PdfLoadError, type PdfLoad, type PdfPasswordPrompt } from './pdf-runtime';
import { renderPdfPage, drainPdfWork, PDF_LIMITS, PdfReaderError, type LoadedPdf, type PageRender } from './pdf-page';

export type ReaderState = { phase: 'idle' | 'loading' | 'password' | 'rendering' | 'ready' | 'error' | 'cancelled';
  page: number; pages: number; zoom: number; fit: boolean; message: string; notice: string };
export const initialReaderState = (): ReaderState => ({ phase: 'idle', page: 1, pages: 0,
  zoom: 1, fit: true, message: '', notice: '' });
const READ_ERROR = 'Cannot open this PDF. It may be missing, empty, corrupt, unsafe, unsupported, or larger than 64 MiB. Check the file and native backend, then retry.';
// Svelte unmount cannot await async destruction. A remounted reader waits for the
// old instance's drain; this barrier contains no PDF data or password values.
let readerDrain: Promise<void> = Promise.resolve();
type Session = { read?: CancellablePromise<PdfData>; load?: PdfLoad; loaded?: LoadedPdf;
  render?: PageRender; stopped: boolean; pumping: boolean; renderVersion: number; passwordVersion: number; queue: Promise<void> };

/** Component-owned: never dispatch bytes, documents, promises or password callbacks to the store. */
export function createPdfReader(host: HTMLElement, changed: (state: ReaderState) => void,
  passwordChanged: (prompt: PdfPasswordPrompt | undefined) => void,
  dependencies = { readPdf: api.readPdf, loadPdf, renderPage: renderPdfPage }) {
  let state = initialReaderState();
  let current: Session | undefined;
  let generation = 0;
  let disposed = false;
  let teardown = Promise.resolve();
  let width = Math.max(1, host.clientWidth - 32);
  const publish = (patch: Partial<ReaderState>) => { state = { ...state, ...patch }; if (!disposed) changed(state); };
  const live = (session: Session) => current === session && !session.stopped && !disposed;

  function stop(): Promise<void> {
    const session = current;
    current = undefined;
    passwordChanged(undefined);
    if (!session) return teardown;
    session.stopped = true;
    session.passwordVersion++;
    session.renderVersion++;
    session.read?.cancel?.();
    // Synchronously cancel/reset surfaces before terminating the owning worker.
    const rendering = session.render?.destroy().catch(() => {});
    const loading = session.load?.destroy().catch(() => {});
    const reading = session.read?.then(() => {}, () => {});
    session.loaded = undefined;
    teardown = Promise.all([teardown, rendering, loading, reading, session.queue]).then(() => {});
    readerDrain = Promise.all([readerDrain, teardown]).then(() => {});
    return teardown;
  }

  function fail(session: Session, message: string) {
    if (!live(session)) return;
    publish({ phase: 'error', message });
    // Do not await this from a render queue which stop itself drains.
    void stop();
  }

  function schedulePage() {
    const session = current;
    if (!session?.loaded || !live(session)) return;
    session.renderVersion++;
    void session.render?.destroy().catch(() => {});
    publish({ phase: 'rendering', message: '', notice: '' });
    if (session.pumping) return;
    session.pumping = true;
    // One pump and one latest desired view, not a promise/task per zoom/resize event.
    session.queue = (async () => {
      try {
        while (live(session) && session.loaded) {
          try {
            await session.render?.destroy();
            session.render = undefined;
            if (!live(session) || !session.loaded) return;
            // No active render/text work. Release shared fonts/images between pages;
            // PDF.js retains at most 500 lightweight page proxies until destroy.
            await drainPdfWork(session.loaded.document.cleanup());
          } catch {
            fail(session, 'Cannot release the previous PDF page safely. Retry the document.');
            return;
          }
          if (!live(session) || !session.loaded) return;
          const version = session.renderVersion;
          try {
            const task = dependencies.renderPage(session.loaded, { page: state.page, zoom: state.zoom, fit: state.fit,
              width, pixelRatio: host.ownerDocument.defaultView?.devicePixelRatio ?? 1 }, host);
            session.render = task;
            const result = await task.promise;
            if (live(session) && version === session.renderVersion) {
              publish({ phase: 'ready', zoom: result.zoom, notice: result.reduced ? 'Display resolution reduced to stay within the page pixel limit.' : '' });
              return;
            }
          } catch (error) {
            if (live(session) && version === session.renderVersion) {
              fail(session, error instanceof PdfReaderError && error.code === 'limit'
                ? 'This page exceeds the safe display or text limit. Try another document.'
                : 'Cannot render this PDF page. Retry or choose another document.');
              return;
            }
          }
        }
      } finally { session.pumping = false; }
    })();
  }

  async function open(id: string | null) {
    if (disposed) return;
    const version = ++generation;
    const pending = stop();
    state = initialReaderState();
    publish({ phase: id ? 'loading' : 'idle' });
    await Promise.all([pending, readerDrain]);
    if (disposed || version !== generation || !id) return;
    const session: Session = { stopped: false, pumping: false, renderVersion: 0, passwordVersion: 0, queue: Promise.resolve() };
    current = session;
    try {
      session.read = dependencies.readPdf(id);
      let pdf: PdfData | undefined = await session.read;
      session.read = undefined;
      if (!live(session)) return;
      session.load = dependencies.loadPdf(pdf.bytes, { onPassword(prompt) {
        if (!live(session)) return;
        const passwordVersion = ++session.passwordVersion;
        let pending: PdfPasswordPrompt | undefined = prompt;
        const valid = () => live(session) && passwordVersion === session.passwordVersion && pending;
        publish({ phase: 'password' });
        passwordChanged({ reason: prompt.reason,
          submit(value) {
            if (!valid()) return false;
            const answer = pending!;
            pending = undefined;
            passwordChanged(undefined);
            publish({ phase: 'loading' });
            return answer.submit(value);
          },
          cancel: () => {
            if (!valid()) return Promise.resolve();
            pending = undefined;
            generation++;
            publish({ phase: 'cancelled', message: 'PDF opening cancelled.' });
            return stop();
          },
        });
      } });
      pdf = undefined;
      session.loaded = await session.load.promise;
      if (!live(session)) { session.loaded = undefined; return; }
      session.passwordVersion++;
      passwordChanged(undefined);
      const pages = session.loaded.document.numPages;
      if (!Number.isInteger(pages) || pages < 1 || pages > PDF_LIMITS.pages) {
        void fail(session, `This PDF exceeds the reader limit of ${PDF_LIMITS.pages} pages or has no readable pages.`);
        return;
      }
      publish({ pages });
      schedulePage();
    } catch (error) {
      if (live(session)) void fail(session, error instanceof PdfLoadError ? error.message : READ_ERROR);
    }
  }
  return {
    open,
    page(value: number) {
      if (!Number.isFinite(value) || !current?.loaded) return;
      const page = Math.min(state.pages, Math.max(1, Math.trunc(value)));
      if (page === state.page) return;
      publish({ page }); schedulePage();
    },
    zoom(value: number) {
      if (!Number.isFinite(value) || !current?.loaded) return;
      publish({ zoom: Math.min(PDF_LIMITS.maxZoom, Math.max(PDF_LIMITS.minZoom, value)), fit: false }); schedulePage();
    },
    fit() { if (current?.loaded) { publish({ fit: true }); schedulePage(); } },
    resize(value: number) {
      if (!Number.isFinite(value) || value <= 32) return;
      const next = Math.floor(value - 32);
      if (width === next) return;
      width = next;
      if (state.fit) schedulePage();
    },
    destroy() {
      if (!disposed) { disposed = true; generation++; }
      return stop();
    },
  };
}