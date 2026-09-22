import type { PDFPageProxy, RenderTask, TextLayer } from 'pdfjs-dist';
import type { PdfLoad } from './pdf-runtime';

export const PDF_LIMITS = Object.freeze({ pages: 500, pixels: 4_000_000, dimension: 8192,
  textItems: 10_000, textCharacters: 1_000_000, textStyles: 256, minZoom: 0.25, maxZoom: 4 });
export class PdfReaderError extends Error {
  constructor(readonly code: 'limit' | 'render') { super(code); }
}
export type LoadedPdf = Awaited<PdfLoad['promise']>;
export type PageView = { page: number; zoom: number; fit: boolean; width: number; pixelRatio: number };
export type PageResult = { zoom: number; reduced: boolean };
export type PageRender = { promise: Promise<PageResult>; destroy(): Promise<void> };

/** A stuck renderer must not overlap its replacement; the caller destroys the document on failure. */
export async function drainPdfWork(work: Promise<unknown>): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([work, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new PdfReaderError('render')), 1000);
    })]);
  } finally { clearTimeout(timer); }
}

export function pageGeometry(page: PDFPageProxy, view: PageView) {
  const base = page.getViewport({ scale: 1 });
  if (![base.width, base.height].every(n => Number.isFinite(n) && n > 0)) throw new PdfReaderError('limit');
  const zoom = Math.min(PDF_LIMITS.maxZoom, Math.max(PDF_LIMITS.minZoom,
    view.fit ? view.width / base.width : view.zoom));
  const viewport = page.getViewport({ scale: zoom });
  if (![viewport.width, viewport.height].every(n => Number.isFinite(n) && n > 0 && n <= PDF_LIMITS.dimension)) {
    throw new PdfReaderError('limit');
  }
  const requested = Math.min(2, Math.max(1, view.pixelRatio || 1));
  const ratio = Math.min(requested, Math.sqrt(PDF_LIMITS.pixels / (viewport.width * viewport.height)),
    PDF_LIMITS.dimension / viewport.width, PDF_LIMITS.dimension / viewport.height);
  const width = Math.max(1, Math.floor(viewport.width * ratio));
  const height = Math.max(1, Math.floor(viewport.height * ratio));
  return { viewport, zoom, width, height, ratio, reduced: ratio < requested };
}

/** One owned canvas and text layer. No page prefetch, thumbnails, annotations or offscreen page cache. */
export function renderPdfPage(loaded: LoadedPdf, view: PageView, host: HTMLElement): PageRender {
  let stopped = false;
  let page: PDFPageProxy | undefined;
  let render: RenderTask | undefined;
  let text: TextLayer | undefined;
  let source: ReadableStreamDefaultReader | undefined;
  let sourceDisposal: Promise<void> | undefined;
  let canvas: HTMLCanvasElement | undefined;
  let root: HTMLDivElement | undefined;
  let container: HTMLDivElement | undefined;
  let disposal: Promise<void> | undefined;
  let rejectCancelled!: (error: Error) => void;
  const cancelled = new Promise<never>((_, reject) => { rejectCancelled = reject; });
  // ReadableStream.cancel() closes reads immediately, but the upstream worker's
  // cancellation can finish later. Keep its first promise; repeated cancel is not a drain.
  const cancelSource = () => sourceDisposal ??= source?.cancel() ?? Promise.resolve();
  const work = (async () => {
    page = await loaded.document.getPage(view.page);
    if (stopped) { page.cleanup(); page = undefined; throw new PdfReaderError('render'); }
    const geometry = pageGeometry(page, view);
    const { viewport, zoom, width, height, ratio, reduced } = geometry;
    root = host.ownerDocument.createElement('div');
    root.className = 'pdf-page';
    root.style.width = `${viewport.width}px`;
    root.style.height = `${viewport.height}px`;
    root.style.visibility = 'hidden';
    root.style.setProperty('--total-scale-factor', String(viewport.scale * page.userUnit));
    canvas = host.ownerDocument.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    canvas.style.width = `${viewport.width}px`;
    canvas.style.height = `${viewport.height}px`;
    canvas.setAttribute('aria-label', `PDF page ${view.page}`);
    canvas.setAttribute('role', 'img');
    container = host.ownerDocument.createElement('div');
    container.className = 'textLayer';
    root.append(canvas, container);
    host.replaceChildren(root);
    render = page.render({ canvas, viewport, transform: [ratio, 0, 0, ratio, 0, 0] });
    await render.promise;
    if (stopped) throw new PdfReaderError('render');
    source = page.streamTextContent().getReader();
    const reader = source;
    let items = 0, characters = 0;
    const styles = new Set<string>();
    const boundedText = new ReadableStream({
      async pull(controller) {
        try {
          const chunk = await reader.read();
          if (stopped) { controller.close(); return; }
          if (chunk.done) { controller.close(); return; }
          items += chunk.value.items.length;
          for (const item of chunk.value.items) characters += typeof item.str === 'string' ? item.str.length : 0;
          for (const key of Object.keys(chunk.value.styles)) styles.add(key);
          if (items > PDF_LIMITS.textItems || characters > PDF_LIMITS.textCharacters || styles.size > PDF_LIMITS.textStyles) {
            void cancelSource().catch(() => {});
            controller.error(new PdfReaderError('limit'));
            return;
          }
          controller.enqueue(chunk.value);
        } catch (error) { controller.error(error); }
      },
      cancel() { return cancelSource(); },
    });
    text = new loaded.TextLayer({ textContentSource: boundedText, container, viewport });
    await text.render();
    if (stopped) throw new PdfReaderError('render');
    root.style.visibility = 'visible';
    return { zoom, reduced };
  })();
  const promise = Promise.race([work, cancelled]);
  void promise.catch(() => {});
  function destroy(): Promise<void> {
    if (disposal) return disposal;
    stopped = true;
    rejectCancelled(new PdfReaderError('render'));
    render?.cancel();
    text?.cancel();
    const sourceDone = cancelSource();
    root?.remove();
    container?.replaceChildren();
    root?.replaceChildren();
    if (canvas) { canvas.width = 0; canvas.height = 0; }
    disposal = drainPdfWork(Promise.all([work.catch(() => {}), sourceDone])).finally(() => {
      page?.cleanup();
      page = undefined;
      render = undefined;
      text = undefined;
      source = undefined;
      sourceDisposal = undefined;
      canvas = undefined;
      root = undefined;
      container = undefined;
    });
    return disposal;
  }
  return { promise, destroy };
}