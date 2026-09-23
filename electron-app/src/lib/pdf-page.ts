import type { PDFPageProxy, RenderTask, TextLayer } from 'pdfjs-dist';
import type { PdfLoad } from './pdf-runtime';

export const PDF_LIMITS = Object.freeze({ minZoom: 0.25, maxZoom: 4 });
export class PdfReaderError extends Error {
  constructor(readonly code: 'render') { super(code); }
}
export type LoadedPdf = Awaited<PdfLoad['promise']>;
export type PageView = { page: number; zoom: number; fit: boolean; width: number; pixelRatio: number;
  measured?(width: number, height: number): void };
export type PageResult = { zoom: number };
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
  if (![base.width, base.height, page.userUnit, view.width, view.zoom, view.pixelRatio].every(n => Number.isFinite(n) && n > 0)) throw new PdfReaderError('render');
  const zoom = view.fit ? view.width / base.width : view.zoom;
  const viewport = page.getViewport({ scale: zoom });
  if (![viewport.width, viewport.height, zoom].every(n => Number.isFinite(n) && n > 0)) {
    throw new PdfReaderError('render');
  }
  const ratio = view.pixelRatio;
  const width = Math.ceil(viewport.width * ratio), height = Math.ceil(viewport.height * ratio);
  if (![width, height].every(n => Number.isSafeInteger(n) && n > 0)) throw new PdfReaderError('render');
  return { viewport, zoom, width, height, ratio, base };
}

/** One owned page in a continuous surface; only the caller's sequential pump starts page work. */
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
    const { viewport, zoom, width, height, ratio, base } = geometry;
    view.measured?.(base.width, base.height);
    if (stopped) throw new PdfReaderError('render');
    root = host.ownerDocument.createElement('div');
    root.className = 'pdf-page';
    root.style.width = `${viewport.width}px`;
    root.style.height = `${viewport.height}px`;
    root.style.visibility = 'hidden';
    root.style.setProperty('--total-scale-factor', String(viewport.scale * page.userUnit));
    root.setAttribute('data-page-number', String(view.page));
    canvas = host.ownerDocument.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    // Canvas setters can wrap unsupported dimensions; a non-null context alone
    // does not prove that the browser allocated a usable backing store.
    if (canvas.width !== width || canvas.height !== height) throw new PdfReaderError('render');
    const context = canvas.getContext('2d');
    if (!context) throw new PdfReaderError('render');
    context.fillRect(width - 1, height - 1, 1, 1);
    if (context.getImageData(width - 1, height - 1, 1, 1).data[3] !== 255) throw new PdfReaderError('render');
    context.clearRect(width - 1, height - 1, 1, 1);
    canvas.style.width = `${viewport.width}px`;
    canvas.style.height = `${viewport.height}px`;
    canvas.setAttribute('aria-label', `PDF page ${view.page}`);
    canvas.setAttribute('role', 'img');
    container = host.ownerDocument.createElement('div');
    container.className = 'pdf-text';
    root.append(canvas, container);
    host.append(root);
    render = page.render({ canvas, viewport, transform: [ratio, 0, 0, ratio, 0, 0] });
    await render.promise;
    if (stopped) throw new PdfReaderError('render');
    source = page.streamTextContent({ includeMarkedContent: false }).getReader();
    const styles: Record<string, import('pdfjs-dist/types/src/display/api').TextStyle> = Object.create(null);
    while (!stopped) {
      const chunk = await source.read();
      if (stopped || chunk.done) break;
      const value = chunk.value;
      if (!value || !Array.isArray(value.items) || !value.styles || typeof value.styles !== 'object' || Array.isArray(value.styles) ||
          (value.lang != null && typeof value.lang !== 'string')) throw new PdfReaderError('render');
      for (const [name, style] of Object.entries(value.styles) as [string, typeof styles[string]][]) {
        if (!style || typeof style.fontFamily !== 'string' || typeof style.vertical !== 'boolean' ||
            ![style.ascent, style.descent].every(n => n === undefined || Number.isFinite(n))) throw new PdfReaderError('render');
        styles[name] = style;
      }
      // PDF.js silently stops a single TextLayer after 100k divs. Render every
      // item in sequential batches instead; this is not a page/text content cap.
      for (let offset = 0; offset < value.items.length; offset += 4096) {
        const items = value.items.slice(offset, offset + 4096);
        for (const item of items) {
          if (!item || typeof item.str !== 'string' || !['ltr', 'rtl', 'ttb'].includes(item.dir) ||
              typeof item.fontName !== 'string' || !styles[item.fontName] || typeof item.hasEOL !== 'boolean' ||
              !Array.isArray(item.transform) || item.transform.length !== 6 || !Array.from(item.transform).every(Number.isFinite) ||
              ![item.width, item.height].every(n => Number.isFinite(n) && n >= 0)) throw new PdfReaderError('render');
        }
        const batch = host.ownerDocument.createElement('div');
        batch.className = 'textLayer';
        container.append(batch);
        text = new loaded.TextLayer({ textContentSource: { items, styles, lang: value.lang ?? null }, container: batch, viewport });
        await text.render();
        if (stopped) throw new PdfReaderError('render');
        text = undefined;
      }
    }
    if (stopped) throw new PdfReaderError('render');
    source.releaseLock();
    // Only visible output survives; page operators/images are not retained here.
    page.cleanup();
    page = undefined;
    render = undefined;
    text = undefined;
    source = undefined;
    root.style.visibility = 'visible';
    return { zoom };
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
    for (const batch of container?.children ?? []) batch.replaceChildren();
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