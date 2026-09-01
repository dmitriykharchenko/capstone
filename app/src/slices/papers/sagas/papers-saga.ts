import { call, put, select, takeLatest } from "typed-redux-saga";
import { api, pdfBytesToObjectUrl } from "../../../lib/api";
import {
  loadPapers,
  openPaper,
  papersFailed,
  papersLoading,
  pdfFailed,
  pdfLoading,
  setPapers,
  setPdfUrl,
} from "../papers-slice";

// List the PDFs on disk (native Rust command) and, on open, fetch the bytes and
// build an object URL for inline rendering. The previous URL is revoked first so
// switching papers does not leak blob URLs.
export function* papersSaga() {
  yield* takeLatest(loadPapers, function* () {
    yield* put(papersLoading());
    try {
      const papers = yield* call(api.listPapers);
      yield* put(setPapers(papers));
    } catch (error) {
      yield* put(papersFailed(String(error)));
    }
  });

  yield* takeLatest(openPaper, function* (action) {
    const paperId = action.payload[0];
    const previous = (yield* select((state) => state.papers.pdfUrl)) as
      | string
      | null;
    if (previous) {
      URL.revokeObjectURL(previous);
    }
    yield* put(pdfLoading(paperId));
    try {
      const pdf = yield* call(api.readPdf, paperId);
      const url = yield* call(pdfBytesToObjectUrl, pdf);
      yield* put(setPdfUrl(paperId, url));
    } catch (error) {
      yield* put(pdfFailed(String(error)));
    }
  });
}
