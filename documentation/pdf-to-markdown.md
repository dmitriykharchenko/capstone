# Standalone PDF-to-Markdown converter

This local, offline utility uses PyMuPDF4LLM to extract text, layout-aware Markdown
tables, CSV table files, and PNG figures from a PDF. It is separate from paper
indexing, chat, evaluation, and both desktop applications. It loads no provider credentials,
application configuration, or databases and never modifies input PDFs.

## Usage

From the repository root, use the Python environment described in
`environment.yml`. Run `python -m src.indexing.pdf_to_markdown --help` for options.

- One PDF: `python -m src.indexing.pdf_to_markdown paper.pdf --output-dir converted`
- Directory: `python -m src.indexing.pdf_to_markdown ResearchPapers --output-dir converted`
- Explicit replacement: add `--overwrite` to either command.

Quote input and output paths containing spaces. Directory input includes only
immediate regular files with a `.pdf` extension (case-insensitive), in sorted
filename order. It does not recurse. An empty directory is an error.

Each PDF produces `<stem>.md` plus adjacent `<stem>-image-0001.png`,
`<stem>-image-0002.png`, etc. Each detected table also produces an adjacent
`<stem>-page-0001-table-0001.csv` file, linked directly below its Markdown table.
Pages and tables are numbered from 1; table numbering restarts on each page in
detected reading order. Tables spanning pages are exported as separate per-page
tables, not joined. PDFs without detected tables produce no CSV files.

Image and CSV links contain only portable relative filenames; move or copy the
Markdown **and its PNG and CSV files together**. Tables stay as Markdown pipe
tables, including their header separator row, rather than flattened text or HTML.
No application ingestion is performed.

### Page-aware artifacts and missing-file repair

New Markdown starts with a versioned HTML-comment header containing the PDF's
SHA-256 and page count. Each page has a `pdf-page` comment with its original
one-based page number, including blank pages. Each table is enclosed in
`pdf-table` comments, preserving its readable Markdown and CSV link. These hidden
markers allow consumers to retain source pages and avoid indexing both the
Markdown copy of a table and its CSV as duplicate evidence. Keep the markers
when editing prose. The PDF hash is an integrity check, not a signature.

Python consumers can call `inspect_artifacts(pdf_path)` without writing anything.
It returns `ConversionArtifacts` with the Markdown path, ordered `MarkdownPage`
objects (text, positive page number, and `MarkdownTable` references), image paths,
and a `missing` tuple. Only assets declared by that document are inspected;
unrelated or obsolete CSVs/images are ignored. Zero-table documents with valid
page markers are complete without CSV files. Table files are not parsed and
images are not decoded by this inspection boundary. Inspection remains read-only:
legacy Markdown raises a conversion error directing callers to `ensure_artifacts`.

`ensure_artifacts(pdf_path)` reuses complete artifacts without conversion, or
converts privately using a **Hybrid policy**: regenerate unmarked legacy Markdown,
preserve valid page-aware Markdown, and create **only missing CSV/image assets**.
Both functions default to artifacts beside the PDF and accept an optional
`output_dir` for standalone conversion directories. Existing CSV, PNG, valid
page-aware Markdown, and unrelated files are never replaced, even when edited.
New or regenerated Markdown is published after the assets it references.
Missing assets for an existing document are repaired only if they can be found
in the regenerated output;
a missing CSV whose Markdown table was edited requires explicit refresh instead
of silently mixing the edited table with regenerated cells.

Legacy Markdown with no converter metadata comments is automatically replaced
with fresh page-aware output. **Legacy Markdown edits are not retained or backed
up automatically**; copy them elsewhere before using repair if needed. Generation,
completeness validation, and target checks finish before any publication, so a
conversion failure preserves the original files. The legacy Markdown bytes are
checked before publication and again immediately before replacement; a detected
concurrent edit stops replacement with a retry error.

Malformed/unsupported converter metadata (including a header with missing page
markers), a changed PDF hash, unsafe references, and symlink/directory targets
still produce actionable errors rather than granting overwrite permission.
Back up edits first, then explicitly run the standalone converter with
`--overwrite` to refresh these invalid or stale outputs. Its overwrite behavior
is unchanged and can replace existing CSV/image assets too. Adjacent generated
PNG names and each table's exact page-numbered CSV name are the only accepted asset paths;
remote links and directory traversal are never followed. Other ordinary document
hyperlinks are not fetched.

Repair is atomic per file, not as a whole document. A disk error/interruption may
leave some newly created files; rerunning repair can complete the set. Missing
files use no-clobber publication, so concurrent creators win. Legacy Markdown
replacement uses an atomic rename after the checks above, not a filesystem
compare-and-swap or an editor lock; avoid editing it while repair is running.
A fresh completeness check follows publication. These APIs do not change
indexing behavior on their own.

### CSV format

CSV files use UTF-8 without a BOM, commas, double-quote escaping, and CRLF record
separators (Python's standard `csv` dialect). Commas, quotes, and embedded line
breaks are quoted correctly. Every extracted row is included, including the
original first/header row. Empty or missing cells become empty fields; duplicate
headers, Unicode, whitespace, and numeric-looking strings are not rewritten.
Read using `csv.reader` with `encoding="utf-8"` and `newline=""` to preserve cells.

CSV uses the same detected table's plain-text cell grid as the Markdown renderer,
not a second detection pass or a parse of Markdown formatting. This relies on the
layout adapter in the pinned PyMuPDF4LLM version; rerun the generated-PDF tests
when upgrading it. Merged-cell geometry cannot be represented in CSV, and any
upstream text normalization or detection errors affect both outputs. If a
detected table has no extractable cell grid, conversion fails instead of silently
omitting its CSV.

Cell values are preserved rather than sanitized for spreadsheet execution. When
opening untrusted CSVs in a spreadsheet, import cells as text and disable formula
evaluation; formula-like text is not prefixed or modified by this converter.

Portable ASCII stems of up to 80 characters are preserved. Other names (spaces,
Unicode, reserved Windows names, excessive length, or punctuation unsafe in links)
use a readable `pdf-` prefix, a sanitized stem, and a stable 12-character filename
hash. This prevents lossy slug collisions without exposing local paths in links.
Inputs whose output stems collide case-insensitively are rejected; rename one.
Output names do not depend on the input directory, output directory, or run time.

## Existing files and failures

Existing Markdown **or generated image/CSV targets** are refused unless `--overwrite`
is supplied. Output symbolic links and directory/file conflicts are refused even
with that flag. Unrelated files and obsolete images/CSVs from previous conversions
are never deleted. Use a new empty output directory when a clean asset set matters.

Conversion happens in temporary directories on the output volume. Every PDF is
converted and every target checked before any output is published. Invalid PDFs,
password protection, and conflicts therefore do not replace old output. Each
published file is atomic; the entire multi-file batch is **not** a transaction.
An interruption, disk error, or concurrent write during publication may leave a
partial set. Retry in an empty output directory, or use `--overwrite` deliberately.
The destination filesystem must support hard links for no-overwrite publication.
An empty output directory may remain after a failed conversion.

The CLI returns 0 for success, 1 for input/conversion/filesystem errors, 2 for
invalid command arguments, and 130 for keyboard cancellation. Errors provide
fixed actionable messages, without raw library errors, input contents, or stack
traces. The extraction subprocess's diagnostics are suppressed; its working
directory is private, so paths containing spaces and punctuation remain safe.

## Extraction limits

Layout and table recognition are heuristic. Review complex tables, merged cells,
equations, reading order, and figure captions before relying on the output.
Extracted PNGs are rendered figure regions at the library's default 150 DPI, not
necessarily the original compressed image bytes. OCR is disabled: scanned PDFs
can produce figure images but not searchable text/tables without an existing text
layer. Provide an unlocked copy of a password-protected PDF. Conversion uses local
PyMuPDF layout processing only; there are no hosted models or live providers.

## Licensing

**PyMuPDF4LLM and PyMuPDF are offered under AGPL-3.0 or a commercial Artifex
license.** Their related layout component is included by the package dependency.
The project owner explicitly approved this dependency after disclosure. This is
not a claim that the rest of the repository has been relicensed. Distribution,
modification, and network-service deployment may carry AGPL obligations; obtain
legal review or an appropriate commercial license for your use case. This is
informational, not legal advice. See the
[upstream licensing information](https://pymupdf.readthedocs.io/en/latest/about.html#license-and-copyright).

## Offline verification

Run `python -m unittest discover -s tests -p test_pdf_to_markdown.py -v` in an
environment with the declared dependency installed. The suite generates PDFs
locally containing text, a table, and an image; no checked-in PDFs, user documents,
providers, or external services are needed. It also tests a fakeable extraction
boundary, deterministic names, link relocation, batch conversion, overwrite
protection, multiple tables/pages, CSV cell round-tripping, zero-table documents,
page provenance, reuse/repair with existing edits, invalid/legacy artifacts,
malformed/password-protected inputs, and CLI behavior. Generated-PDF
tests are explicitly skipped if PyMuPDF4LLM is absent; use the dependency-enabled
environment to verify the complete converter.
