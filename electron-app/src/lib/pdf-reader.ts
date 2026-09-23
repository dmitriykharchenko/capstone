import { api, type CancellablePromise, type PdfData } from './api';
import { loadPdf, PdfLoadError, type PdfLoad, type PdfPasswordPrompt } from './pdf-runtime';
import { renderPdfPage, drainPdfWork, PDF_LIMITS, type LoadedPdf, type PageRender } from './pdf-page';
import { PdfLayout, PDF_SCROLL_SPAN, pdfWindow } from './pdf-layout';

export type ReaderState = { phase: 'idle' | 'loading' | 'password' | 'rendering' | 'ready' | 'error' | 'cancelled';
  page: number; pages: number; rendered: number; zoom: number; fit: boolean; message: string; notice: string };
export const initialReaderState = (): ReaderState => ({ phase: 'idle', page: 1, pages: 0,
  rendered: 0, zoom: 1, fit: true, message: '', notice: '' });
const READ_ERROR = 'Cannot open this PDF. It may be missing, changed, closed, corrupt, unsafe, or unsupported. Reopen the preview.';
// Svelte unmount cannot await async destruction. A remounted reader waits for the
// old instance's drain; this barrier contains no PDF data or password values.
let readerDrain: Promise<void> = Promise.resolve();
type Session = { read?: CancellablePromise<PdfData>; load?: PdfLoad; loaded?: LoadedPdf;
  layout?: PdfLayout; surface?: HTMLDivElement; entries: Map<number, Entry>; retired: Promise<void>[];
  desired: number[]; top: number; physicalTop: number; stopped: boolean; pumping: boolean;
  cleanup: boolean; passwordVersion: number; queue: Promise<void> };
type Entry = { slot: HTMLDivElement; task?: PageRender; zoom?: number };
const RENDER_ERROR = 'Cannot render this PDF page. Retry or choose another document.';
const RELEASE_ERROR = 'Cannot release the previous PDF page safely. Retry the document.';

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
  let height = Math.max(1, host.parentElement?.clientHeight ?? 800);
  const publish = (patch: Partial<ReaderState>) => { state = { ...state, ...patch }; if (!disposed) changed(state); };
  const live = (session: Session) => current === session && !session.stopped && !disposed;

  function retire(session: Session, number: number, entry: Entry) {
    session.entries.delete(number);
    entry.slot.remove();
    if (entry.task) {
      const drained = entry.task.destroy();
      void drained.catch(() => {});
      session.retired.push(drained);
    }
    session.cleanup = true;
  }
  function clearPages(session: Session) {
    for (const [number, entry] of session.entries) retire(session, number, entry);
  }
  function position(session: Session) {
    const layout = session.layout!, surface = session.surface!;
    const total = scrollHeight(layout);
    if (!Number.isFinite(total) || total <= 0) throw Error('Invalid layout');
    const physicalHeight = Math.min(PDF_SCROLL_SPAN, total);
    session.top = Math.max(0, Math.min(session.top, Math.max(0, total - height)));
    session.physicalTop = total <= PDF_SCROLL_SPAN ? session.top :
      session.top / Math.max(1, total - height) * Math.max(0, physicalHeight - height);
    surface.style.height = `${physicalHeight - 32}px`;
    let widest = width;
    for (const [number, entry] of session.entries) {
      const size = layout.size(number);
      entry.slot.style.top = `${layout.top(number) - session.top + session.physicalTop}px`;
      entry.slot.style.width = `${size.width}px`;
      entry.slot.style.height = `${size.height}px`;
      widest = Math.max(widest, size.width);
    }
    surface.style.width = `${widest}px`;
    const scroll = host.parentElement;
    if (scroll && Math.abs(scroll.scrollTop - session.physicalTop) > 0.5) scroll.scrollTop = session.physicalTop;
  }
  function updateWindow(session: Session) {
    if (!live(session) || !session.layout) return;
    const desired = pdfWindow(session.layout, Math.max(0, session.top - 16), height);
    // Preserve overlapping selectable subtrees; detach only pages leaving the
    // window. Shared fonts must survive while retained text still uses them.
    for (const [number, entry] of session.entries) if (!desired.includes(number)) retire(session, number, entry);
    session.desired = desired;
    // Insert only new nodes in reading order. Re-appending an existing node,
    // even at the same index, resets browser selection ranges in its subtree.
    for (const [index, number] of [...desired].sort((a, b) => a - b).entries()) if (!session.entries.has(number)) {
      const slot = host.ownerDocument.createElement('div');
      slot.className = 'pdf-slot';
      slot.setAttribute('data-page-number', String(number));
      session.surface!.insertBefore(slot, session.surface!.children[index] ?? null);
      session.entries.set(number, { slot });
    }
    position(session);
    const page = session.layout.pageAt(Math.max(0, session.top - 16));
    publish({ page, zoom: session.layout.size(page).zoom,
      rendered: [...session.entries.values()].filter(entry => entry.zoom !== undefined).length });
  }
  function scrollHeight(layout: PdfLayout) {
    return Math.max(layout.height + 32, layout.top(layout.pages) + height + 16);
  }

  function stop(): Promise<void> {
    const session = current;
    current = undefined;
    passwordChanged(undefined);
    if (!session) return teardown;
    session.stopped = true;
    session.passwordVersion++;
    session.read?.cancel?.();
    // Synchronously cancel/reset surfaces before terminating the owning worker.
    clearPages(session);
    session.surface?.remove();
    const rendering = Promise.all(session.retired).catch(() => {});
    const loading = session.load?.destroy().catch(() => {});
    const reading = session.read?.then(() => {}, () => {});
    session.loaded = undefined;
    teardown = Promise.all([teardown, rendering, loading, reading, session.queue]).then(() => {});
    readerDrain = Promise.all([readerDrain, teardown]).then(() => {});
    return teardown;
  }

  function fail(session: Session, message: string) {
    if (!live(session)) return;
    publish({ phase: 'error', rendered: 0, message, notice: '' });
    // Do not await this from a render queue which stop itself drains.
    void stop();
  }

  function schedulePages(rebuild = false) {
    const session = current;
    if (!session?.loaded || !session.layout || !live(session)) return;
    try {
      if (rebuild) {
        const page = state.page;
        const fraction = (session.top - 16 - session.layout.top(page)) / session.layout.size(page).height;
        clearPages(session);
        session.layout.configure(width, state.zoom, state.fit);
        session.top = session.layout.top(page) + Math.max(0, fraction) * session.layout.size(page).height + 16;
      }
      updateWindow(session);
      if ([...session.entries.values()].some(entry => entry.zoom === undefined)) publish({ phase: 'rendering', message: '' });
    } catch { fail(session, RENDER_ERROR); return; }
    if (session.pumping) return;
    session.pumping = true;
    // One sequential pump; events only replace the desired window. Disposed tasks
    // drain before any replacement starts, including late getPage/text promises.
    session.queue = (async () => {
      try {
        while (live(session) && session.loaded) {
          try {
            const retired = session.retired.splice(0);
            await Promise.all(retired);
            if (!live(session) || !session.loaded) return;
            if (session.cleanup) {
              session.cleanup = false;
              await drainPdfWork(session.loaded.document.cleanup([...session.entries.values()].some(entry => entry.zoom !== undefined)));
            }
          } catch {
            fail(session, RELEASE_ERROR); return;
          }
          if (!live(session) || !session.loaded) return;
          if (session.retired.length || session.cleanup) continue;
          const number = session.desired.find(number => !session.entries.get(number)?.task);
          if (number === undefined) { publish({ phase: 'ready' }); return; }
          const entry = session.entries.get(number)!;
          publish({ phase: 'rendering', message: '' });
          try {
            const task = dependencies.renderPage(session.loaded, { page: number, zoom: state.zoom, fit: state.fit, width,
              pixelRatio: host.ownerDocument.defaultView?.devicePixelRatio ?? 1,
              measured(pageWidth, pageHeight) {
                if (!live(session) || session.entries.get(number) !== entry) return;
                const anchor = state.page, offset = session.top - session.layout!.top(anchor);
                session.layout!.measure(number, pageWidth, pageHeight);
                session.top = session.layout!.top(anchor) + offset;
                updateWindow(session);
              },
            }, entry.slot);
            entry.task = task;
            const result = await task.promise;
            if (!live(session)) return;
            if (session.entries.get(number) === entry) {
              entry.zoom = result.zoom;
              updateWindow(session);
            }
            session.cleanup = true;
          } catch {
            if (live(session) && session.entries.get(number) === entry) { fail(session, RENDER_ERROR); return; }
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
    const session: Session = { entries: new Map(), retired: [], desired: [], top: 0, physicalTop: 0,
      stopped: false, pumping: false, cleanup: true, passwordVersion: 0, queue: Promise.resolve() };
    current = session;
    try {
      session.read = dependencies.readPdf(id);
      let pdf: PdfData | undefined = await session.read;
      if (!live(session)) return;
      session.load = dependencies.loadPdf(pdf, { onError(error) { fail(session, error.message); }, onPassword(prompt) {
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
      if (!Number.isSafeInteger(pages) || pages < 1) {
        void fail(session, 'This PDF has no readable pages.');
        return;
      }
      session.layout = new PdfLayout(pages);
      session.layout.configure(width, state.zoom, state.fit);
      session.surface = host.ownerDocument.createElement('div');
      session.surface.className = 'pdf-document';
      host.append(session.surface);
      publish({ pages });
      schedulePages();
    } catch (error) {
      if (live(session)) void fail(session, error instanceof PdfLoadError ? error.message : READ_ERROR);
    }
  }
  return {
    open,
    page(value: number) {
      if (!Number.isFinite(value) || !current?.layout) return;
      const page = Math.min(state.pages, Math.max(1, Math.trunc(value)));
      current.top = current.layout.top(page) + 16;
      schedulePages();
    },
    scrolled(top: number) {
      if (!Number.isFinite(top) || !current?.layout) return;
      const delta = top - current.physicalTop;
      if (Math.abs(delta) < 0.5) return; // A queued event from our own rebasing.
      const total = scrollHeight(current.layout);
      if (total <= PDF_SCROLL_SPAN) {
        current.top += delta;
      } else {
        current.top = top / Math.max(1, PDF_SCROLL_SPAN - height) * Math.max(0, total - height);
      }
      schedulePages();
    },
    scrollBy(delta: number) {
      if (!Number.isFinite(delta) || !current?.layout || scrollHeight(current.layout) <= PDF_SCROLL_SPAN) return false;
      current.top += delta;
      schedulePages();
      return true;
    },
    scrollKey(key: string) {
      if (!current?.layout || scrollHeight(current.layout) <= PDF_SCROLL_SPAN) return false;
      const steps: Record<string, number> = { ArrowDown: 40, ArrowUp: -40, PageDown: height, PageUp: -height,
        Home: -current.top, End: scrollHeight(current.layout) - height - current.top };
      if (!Object.hasOwn(steps, key)) return false;
      const delta = steps[key];
      current.top += delta;
      schedulePages();
      return true;
    },
    zoom(value: number) {
      if (!Number.isFinite(value) || !current?.loaded) return;
      publish({ zoom: Math.min(PDF_LIMITS.maxZoom, Math.max(PDF_LIMITS.minZoom, value)), fit: false }); schedulePages(true);
    },
    fit() { if (current?.loaded) { publish({ fit: true }); schedulePages(true); } },
    resize(value: number, viewportHeight = host.parentElement?.clientHeight ?? height) {
      if (!Number.isFinite(value) || value <= 32) return;
      const next = Math.floor(value - 32);
      const resized = width !== next;
      width = next;
      if (Number.isFinite(viewportHeight) && viewportHeight > 0) height = viewportHeight;
      schedulePages(resized && state.fit);
    },
    destroy() {
      if (!disposed) { disposed = true; generation++; }
      return stop();
    },
  };
}