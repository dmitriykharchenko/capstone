# Tauri PDF viewer reference

[Index](README.md) · [Desktop host](native-desktop.md) · [Frontend](frontend.md) · [Paper indexing](paper-indexing.md) · [Operations](operations-and-testing.md)

**Source inspection: October 14, 2026.** This application reads a complete local PDF through Rust, creates a browser Blob URL and displays it in an iframe. No application-owned PDF.js worker, native range lease, custom password form or sparse page renderer is part of this path. Source inspection does not establish rendering in a particular host webview.

## Selection and byte flow

1. [PapersList.svelte](../app/src/components/PapersList.svelte) dispatches `openPaper(id)`. Paper-search results can also request a paper; source-card pages are not PDF page-jump controls.
2. [papersSaga](../app/src/slices/papers/sagas/papers-saga.ts) uses `takeLatest(openPaper)`, revokes the prior stored Blob URL, and calls [api.readPdf](../app/src/lib/api.ts).
3. Tauri [read_pdf](../app/src-tauri/src/lib.rs) runs [PaperRepository::read_pdf](../app/src-tauri/src/papers.rs) on the blocking pool. It returns ID, filename, `application/pdf` MIME and the entire byte vector.
4. `pdfBytesToObjectUrl` creates the Blob URL and `setPdfUrl` stores it. [PdfViewer.svelte](../app/src/components/PdfViewer.svelte) displays the URL in an iframe.

The selected paper is store state, not a `/papers` URL parameter. The sidebar collapses when focus enters the iframe. PDF presentation depends on the host webview; an iframe element or successful byte transfer is not evidence of visible pages.

## Filesystem boundary

`papers_directory` uses nonblank shell `CAPSTONE_PAPERS_DIR` or project `ResearchPapers/`. A missing listing directory yields an empty list. Safe IDs begin with ASCII alphanumeric and continue with alphanumeric/period/underscore/hyphen. The repository adds `.pdf`, canonicalizes both root/candidate, and requires a regular file beneath the canonical root. IDs are not arbitrary paths.

`read_pdf` uses `std::fs::read` after resolution. This is a whole-file allocation and serialization, not bounded range access or constant-memory streaming. Path containment is checked, but no pinned descriptor/freshness lease prevents external edits after resolution. Large PDFs can incur memory/IPC overhead. The host, not an application-defined page parser, handles final presentation.

## What opening does not do

- No text extraction, embeddings, metadata extraction, graph writes or indexing occur merely from preview.
- No application-defined page/zoom/text-layer/password lifecycle is provided. Any controls or protected-document behavior supplied by the webview are host-dependent, not guaranteed by this component.
- No OCR is added. A scanned file opening successfully does not make it searchable.
- No durable per-paper reading position, selected-paper deep link, thumbnail/outline/annotation workflow is defined by this component.
- `takeLatest` stops the old saga worker, but ordinary invoke cancellation is not implemented by closing the frontend channel.

## Troubleshooting and checks

For a blank preview, distinguish failed native read from a valid Blob URL that the actual host does not render. Check safe ID, root agreement, file existence/readability and the iframe's selected URL without exposing document bytes or private paths. Do not reset indexes to repair viewing. If preview works but indexing fails, inspect extraction/protection/chunk/metadata limits separately in [indexing](paper-indexing.md).

Use existing [papers-state tests](../app/tests/papers-state.test.mjs) and [papers saga](../app/src/slices/papers/sagas/papers-saga.ts) as ownership seams; changes need tests for replacement/stale reads and Blob URL revocation. The [native repository](../app/src-tauri/src/papers.rs) and [command registration](../app/src-tauri/src/lib.rs) define filesystem access. Real ordinary/protected PDF display, switching and focus behavior require an intentional desktop check, not inference from a source assertion. No app, provider, corpus extraction or visual test ran for this documentation pass.
