/** Sparse page geometry, not a page-proxy/DOM cache. Unvisited pages use an estimate. */
export class PdfLayout {
  private sizes = new Map<number, { width: number; height: number }>();
  private corrections: { page: number; sum: number }[] = [];
  private estimate = { width: 600, height: 800 };
  private width = 600;
  private zoom = 1;
  private fit = true;
  constructor(readonly pages: number) {
    if (!Number.isSafeInteger(pages) || pages < 1) throw Error('Invalid page count');
  }
  configure(width: number, zoom: number, fit: boolean) {
    this.width = width; this.zoom = zoom; this.fit = fit; this.rebuild();
  }
  measure(page: number, width: number, height: number) {
    this.sizes.set(page, { width, height });
    if (page === 1) this.estimate = { width, height };
    this.rebuild();
  }
  private scaled(size: { width: number; height: number }) {
    const zoom = this.fit ? this.width / size.width : this.zoom;
    return { width: size.width * zoom, height: size.height * zoom, zoom };
  }
  size(page: number) { return this.scaled(this.sizes.get(page) ?? this.estimate); }
  private rebuild() {
    const height = this.scaled(this.estimate).height;
    let sum = 0;
    this.corrections = [...this.sizes].sort(([a], [b]) => a - b).map(([page, size]) => {
      sum += this.scaled(size).height - height;
      return { page, sum };
    });
  }
  top(page: number) {
    let low = 0, high = this.corrections.length;
    while (low < high) {
      const middle = Math.floor((low + high) / 2);
      if (this.corrections[middle].page < page) low = middle + 1;
      else high = middle;
    }
    return (page - 1) * (this.scaled(this.estimate).height + 16) + (this.corrections[low - 1]?.sum ?? 0);
  }
  get height() { return this.top(this.pages) + this.size(this.pages).height; }
  pageAt(top: number) {
    let low = 1, high = this.pages;
    while (low < high) {
      const middle = Math.ceil((low + high) / 2);
      if (this.top(middle) <= top) low = middle;
      else high = middle - 1;
    }
    return low;
  }
}

// A browser scroll-coordinate span, NOT a PDF size limit. Larger logical documents
// rebase local wheel movement and map scrollbar jumps across the entire document.
export const PDF_SCROLL_SPAN = 8_000_000;
export const PDF_OVERSCAN_PAGES = 2;
export function pdfWindow(layout: PdfLayout, top: number, height: number) {
  const first = layout.pageAt(top), pages = [first];
  let next = first + 1;
  // Cover the viewport before adding bounded overscan. Even arbitrarily short
  // pages are separated by 16px, so active resources scale with viewport height,
  // not document length; a fixed slot ceiling would leave visible pages blank.
  while (next <= layout.pages && layout.top(next) < top + height) pages.push(next++);
  for (let page = first - 1; page >= Math.max(1, first - PDF_OVERSCAN_PAGES); page--) pages.push(page);
  for (let extra = 0; extra < PDF_OVERSCAN_PAGES && next <= layout.pages; extra++) pages.push(next++);
  return pages;
}