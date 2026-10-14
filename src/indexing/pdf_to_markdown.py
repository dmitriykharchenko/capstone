"""Standalone, offline PDF conversion: python -m src.indexing.pdf_to_markdown --help.

PyMuPDF4LLM / PyMuPDF are AGPL-3.0 or commercially licensed. See
documentation/pdf-to-markdown.md before distributing or deploying this tool.
This module does not import the application's configuration or providers.
"""

from __future__ import annotations

import argparse
import csv
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from typing import Callable


class ConversionError(Exception):
    """An actionable message that is safe to display without a traceback."""


class _LegacyMarkdownError(ConversionError):
    def __init__(self, path: Path, original: bytes):
        super().__init__(
            "Legacy Markdown has no page metadata; use ensure_artifacts to regenerate it."
        )
        self.path = path
        self.original = original


@dataclass(frozen=True)
class MarkdownTable:
    table_number: int
    csv_path: Path
    markdown: str


@dataclass(frozen=True)
class MarkdownPage:
    page_number: int
    text: str
    tables: tuple[MarkdownTable, ...]


@dataclass(frozen=True)
class ConversionArtifacts:
    markdown_path: Path
    pages: tuple[MarkdownPage, ...]
    images: tuple[Path, ...]
    missing: tuple[Path, ...]

    @property
    def paths(self) -> tuple[Path, ...]:
        return (
            self.markdown_path,
            *self.images,
            *(table.csv_path for page in self.pages for table in page.tables),
        )


_ARTIFACT_HEADER = re.compile(
    r"<!-- pdf-artifacts: v1 pages=([1-9][0-9]*) sha256=([0-9a-f]{64}) -->"
)
_PAGE_MARKER = re.compile(r"^<!-- pdf-page: ([1-9][0-9]*) -->$", re.MULTILINE)
_TABLE_BLOCK = re.compile(
    r"^<!-- pdf-table: ([1-9][0-9]*) -->\n.*?^<!-- /pdf-table -->$",
    re.MULTILINE | re.DOTALL,
)
_IMAGE_LINK = re.compile(r"!\[[^\]\n]*\]\(([^)\n]+)\)")
_REFRESH_GUIDANCE = (
    "Existing Markdown lacks valid page-aware conversion metadata; "
    "back up any edits, then explicitly refresh with the converter's --overwrite option."
)
_FROZEN_WORKER_ARGUMENT = "--capstone-pdf-worker"
_EXTRACTION_TIMEOUT_SECONDS = 300


def _output_stem(source: Path) -> str:
    """Keep portable stems; hash altered names to avoid lossy slug collisions."""
    stem = source.stem
    reserved = {"CON", "PRN", "AUX", "NUL"} | {
        f"{prefix}{number}" for prefix in ("COM", "LPT") for number in range(1, 10)
    }
    if (
        re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,79}", stem)
        and not stem.endswith(".")
        and stem.split(".")[0].upper() not in reserved
    ):
        return stem
    slug = re.sub(r"[^A-Za-z0-9._-]+", "-", stem).strip(".-_")[:60] or "document"
    digest = hashlib.sha256(source.name.encode("utf-8")).hexdigest()[:12]
    return f"pdf-{slug}-{digest}"


def _inputs(source: Path) -> list[Path]:
    if source.is_dir():
        files = sorted(
            (p for p in source.iterdir() if p.is_file() and p.suffix.lower() == ".pdf"),
            key=lambda p: (p.name.casefold(), p.name),
        )
        if not files:
            raise ConversionError(
                "Input directory contains no PDF files (not recursive)."
            )
    elif source.is_file() and source.suffix.lower() == ".pdf":
        files = [source]
    else:
        raise ConversionError(
            "Input must be an existing PDF file or directory of PDFs."
        )
    stems = [_output_stem(p).casefold() for p in files]
    if len(stems) != len(set(stems)):
        raise ConversionError(
            "PDF names have conflicting output names; rename the inputs."
        )
    return files


def _check_target(target: Path, overwrite: bool) -> None:
    if target.is_symlink() or (target.exists() and not target.is_file()):
        raise ConversionError(
            "An output target is a link or directory; choose another output directory."
        )
    if target.exists() and not overwrite:
        raise ConversionError(
            "Output already exists; choose another directory or use --overwrite."
        )


def _export_tables(parsed_document, directory: Path) -> None:
    """Export the layout's plain-text grids without reparsing Markdown cells."""
    for page in parsed_document.pages:
        number = 0
        for box in page.boxes:
            if box.boxclass != "table":
                continue
            rows = box.table["extract"]
            if not rows:
                raise ValueError("Detected table has no extractable cells.")
            number += 1
            name = f"document-page-{page.page_number:04d}-table-{number:04d}.csv"
            with (directory / name).open("w", encoding="utf-8", newline="") as stream:
                csv.writer(stream).writerows(rows)
            box.table["markdown"] = (
                f"<!-- pdf-table: {number} -->\n{box.table['markdown']}"
                f"\n\n[Table {number} on page {page.page_number} (CSV)]({name})"
                "\n<!-- /pdf-table -->"
            )


def _extract_worker(source: str) -> int:
    """Run only in a disposable subprocess; third-party diagnostics stay private."""
    try:
        import pymupdf
        from pymupdf4llm.helpers.document_layout import parse_document
    except ImportError:
        return 3
    try:
        # An unnamed in-memory document avoids unsafe source names in image paths.
        source_bytes = Path(source).read_bytes()
        with pymupdf.open(stream=source_bytes, filetype="pdf") as document:
            if document.needs_pass:
                return 4
            if not document.is_pdf or not document.page_count:
                return 5
            # This is the same layout parse used by the pinned to_markdown API.
            # Retain its raw table cells so CSV and Markdown share one detection.
            parsed = parse_document(
                document,
                filename="document",
                write_images=True,
                embed_images=False,
                image_path=".",
                image_format="png",
                show_progress=False,
                use_ocr=False,
                force_text=True,
            )
            _export_tables(parsed, Path("."))
            chunks = parsed.to_markdown(show_progress=False, page_chunks=True)
            digest = hashlib.sha256(source_bytes).hexdigest()
            markdown = (
                f"<!-- pdf-artifacts: v1 pages={len(chunks)} sha256={digest} -->\n"
            )
            for chunk in chunks:
                page = chunk["metadata"]["page_number"]
                markdown += f"\n<!-- pdf-page: {page} -->\n{chunk['text']}\n"
        Path("document.md").write_text(markdown, encoding="utf-8")
    except Exception:
        return 5
    return 0


def _extract_pdf(source: Path, staging: Path) -> None:
    # A private cwd also works around third-party image-path normalization of
    # spaces/parentheses. Never change the caller's cwd or logging configuration.
    if getattr(sys, "frozen", False):
        command = [sys.executable, _FROZEN_WORKER_ARGUMENT, str(source.resolve())]
    else:
        worker = "import runpy,sys; sys.exit(runpy.run_path(sys.argv[1])['_extract_worker'](sys.argv[2]))"
        command = [
            sys.executable,
            "-c",
            worker,
            str(Path(__file__).resolve()),
            str(source.resolve()),
        ]
    try:
        result = subprocess.run(
            command,
            cwd=staging,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=_EXTRACTION_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        raise ConversionError(
            "PDF conversion timed out; check that the PDF is readable and valid."
        ) from None
    if result.returncode == 3:
        raise ConversionError(
            "PyMuPDF4LLM is unavailable; install the dependencies in environment.yml."
        )
    if result.returncode == 4:
        raise ConversionError(
            "Password-protected PDF: provide an unlocked copy and retry."
        )
    if result.returncode:
        raise ConversionError(
            "PDF conversion failed; check that the PDF is readable and valid."
        )


def convert(
    source: str | Path,
    output_dir: str | Path,
    *,
    overwrite: bool = False,
    extractor: Callable[[Path, Path], None] | None = None,
) -> list[Path]:
    """Convert a PDF or a nonrecursive directory, returning the Markdown paths.

    The injectable extractor writes document.md, adjacent PNGs, and
    document-page-NNNN-table-NNNN.csv assets in its private staging directory.
    All conversions and conflict checks finish before any
    output is published. Publication is atomic per file, not across a batch.
    Existing unrelated or obsolete assets are never pruned, even on overwrite.
    """
    try:
        files = _inputs(Path(source))
        output = Path(output_dir).absolute()
        if output.is_symlink() or (output.exists() and not output.is_dir()):
            raise ConversionError(
                "Output must be a directory, not a file or symbolic link."
            )
        destinations = [output / f"{_output_stem(p)}.md" for p in files]
        for target in destinations:
            _check_target(target, overwrite)
        output.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(
            prefix=".pdf-markdown-", dir=output
        ) as temporary:
            pending: list[tuple[Path, Path]] = []
            for index, (pdf, destination) in enumerate(zip(files, destinations)):
                staging = Path(temporary) / str(index)
                staging.mkdir()
                try:
                    (extractor or _extract_pdf)(pdf, staging)
                except ConversionError:
                    raise
                except Exception:
                    raise ConversionError(
                        "PDF conversion failed; check the input and retry."
                    ) from None
                markdown_path = staging / "document.md"
                markdown = markdown_path.read_text(encoding="utf-8")
                for number, image in enumerate(sorted(staging.glob("*.png")), 1):
                    name = f"{destination.stem}-image-{number:04d}.png"
                    markdown = markdown.replace(f"]({image.name})", f"]({name})")
                    pending.append((image, output / name))
                for table in sorted(staging.glob("document-page-*-table-*.csv")):
                    name = f"{destination.stem}{table.name.removeprefix('document')}"
                    markdown = markdown.replace(f"]({table.name})", f"]({name})")
                    pending.append((table, output / name))
                markdown_path.write_text(markdown, encoding="utf-8")
                pending.append((markdown_path, destination))
            for _, target in pending:
                _check_target(target, overwrite)
            for staged, target in pending:
                if overwrite:
                    _check_target(target, overwrite)
                    os.replace(staged, target)
                else:
                    # Unlike rename(), hard-link publication cannot replace a
                    # concurrently created target. Staging is on the same volume.
                    os.link(staged, target)
        return destinations
    except ConversionError:
        raise
    except FileExistsError:
        raise ConversionError(
            "Output already exists; choose another directory or use --overwrite."
        ) from None
    except (OSError, ValueError):
        raise ConversionError(
            "Cannot read input or write output; check paths, permissions, and free space."
        ) from None


def inspect_artifacts(
    source: str | Path, output_dir: str | Path | None = None
) -> ConversionArtifacts:
    """Inspect only this PDF's declared assets; never follow arbitrary links.

    Missing Markdown is reported as incomplete. Legacy Markdown requires
    regeneration through ensure_artifacts; malformed or stale metadata is an error.
    This validates provenance and paths, not CSV cell contents or image decodability.
    """
    try:
        pdf = Path(source)
        if not pdf.is_file() or pdf.suffix.lower() != ".pdf":
            raise ConversionError("Input must be an existing PDF file.")
        output = Path(output_dir) if output_dir is not None else pdf.parent
        if output.is_symlink() or (output.exists() and not output.is_dir()):
            raise ConversionError(
                "Output must be a directory, not a file or symbolic link."
            )
        stem = _output_stem(pdf)
        markdown_path = output / f"{stem}.md"
        _check_target(markdown_path, overwrite=True)
        if not markdown_path.exists():
            return ConversionArtifacts(markdown_path, (), (), (markdown_path,))
        original = markdown_path.read_bytes()
        markdown = original.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
        first_line, _, body = markdown.partition("\n")
        header = _ARTIFACT_HEADER.fullmatch(first_line)
        if header is None:
            if "<!-- pdf-" not in markdown and "<!-- /pdf-" not in markdown:
                raise _LegacyMarkdownError(markdown_path, original)
            raise ConversionError(_REFRESH_GUIDANCE)
        with pdf.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        if header[2] != digest:
            raise ConversionError(
                "PDF changed since conversion; back up edits and explicitly refresh "
                "with the converter's --overwrite option."
            )
        markers = list(_PAGE_MARKER.finditer(body))
        if (
            len(markers) != int(header[1])
            or [int(marker[1]) for marker in markers]
            != list(range(1, len(markers) + 1))
            or body[: markers[0].start()].strip()
            or body.count("<!-- pdf-page:") != len(markers)
        ):
            raise ConversionError(_REFRESH_GUIDANCE)
        pages = []
        images = []
        for index, marker in enumerate(markers):
            page_number = int(marker[1])
            end = markers[index + 1].start() if index + 1 < len(markers) else len(body)
            text = body[marker.end() : end].strip("\n")
            blocks = list(_TABLE_BLOCK.finditer(text))
            if (
                [int(block[1]) for block in blocks] != list(range(1, len(blocks) + 1))
                or text.count("<!-- pdf-table:") != len(blocks)
                or text.count("<!-- /pdf-table -->") != len(blocks)
            ):
                raise ConversionError(
                    "Invalid table markers; back up edits and explicitly refresh conversion."
                )
            tables = []
            for block in blocks:
                number = int(block[1])
                name = f"{stem}-page-{page_number:04d}-table-{number:04d}.csv"
                link = f"[Table {number} on page {page_number} (CSV)]({name})"
                if not block[0].endswith(f"{link}\n<!-- /pdf-table -->"):
                    raise ConversionError(
                        "Invalid table reference; explicitly refresh conversion."
                    )
                tables.append(MarkdownTable(number, output / name, block[0]))
            for name in _IMAGE_LINK.findall(text):
                if not re.fullmatch(re.escape(stem) + r"-image-[0-9]{4,}\.png", name):
                    raise ConversionError(
                        "Invalid image reference; only adjacent generated PNG assets are supported."
                    )
                if output / name not in images:
                    images.append(output / name)
            pages.append(MarkdownPage(page_number, text, tuple(tables)))
        artifacts = ConversionArtifacts(markdown_path, tuple(pages), tuple(images), ())
        missing = []
        for path in artifacts.paths:
            _check_target(path, overwrite=True)
            if not path.exists():
                missing.append(path)
        return ConversionArtifacts(
            markdown_path, tuple(pages), tuple(images), tuple(missing)
        )
    except ConversionError:
        raise
    except (OSError, ValueError):
        raise ConversionError(
            "Cannot inspect conversion files; check paths, encoding, and permissions."
        ) from None


def _check_legacy_target(path: Path, original: bytes) -> None:
    _check_target(path, overwrite=True)
    if not path.exists() or path.read_bytes() != original:
        raise ConversionError(
            "Markdown changed during regeneration; no replacement was made. Retry."
        )


def ensure_artifacts(
    source: str | Path,
    output_dir: str | Path | None = None,
    *,
    extractor: Callable[[Path, Path], None] | None = None,
) -> ConversionArtifacts:
    """Reuse page-aware Markdown, regenerate legacy Markdown, repair missing assets.

    Defaults to files beside the PDF. Regeneration is private; all required
    candidates are validated before publication. Only unmarked legacy Markdown
    may be replaced; existing CSVs, images, valid page-aware Markdown and unrelated
    or obsolete assets remain untouched. Publication is per-file only.
    """
    legacy_markdown = None
    try:
        existing = inspect_artifacts(source, output_dir)
    except _LegacyMarkdownError as error:
        legacy_markdown = error.original
        existing = ConversionArtifacts(error.path, (), (), (error.path,))
    if not existing.missing:
        return existing
    output = existing.markdown_path.parent
    try:
        output.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(
            prefix=".pdf-repair-", dir=output
        ) as temporary:
            generated_dir = Path(temporary) / "generated"
            convert(source, generated_dir, extractor=extractor)
            generated = inspect_artifacts(source, generated_dir)
            if generated.missing:
                raise ConversionError(
                    "Conversion produced incomplete assets; no files were repaired."
                )
            candidates = {path.name: path for path in generated.paths}
            targets = (
                existing.missing
                if existing.pages
                else tuple(output / name for name in candidates)
            )
            pending = []
            for target in targets:
                _check_target(target, overwrite=True)
                if target.name not in candidates:
                    raise ConversionError(
                        "Missing asset cannot be restored from this PDF; explicitly refresh conversion."
                    )
                if not target.exists() or (
                    legacy_markdown is not None and target == existing.markdown_path
                ):
                    pending.append((candidates[target.name], target))
            old_tables = {
                table.csv_path.name: table
                for page in existing.pages
                for table in page.tables
            }
            new_tables = {
                table.csv_path.name: table
                for page in generated.pages
                for table in page.tables
            }
            for _, target in pending:
                if (
                    target.name in old_tables
                    and old_tables[target.name].markdown
                    != new_tables[target.name].markdown
                ):
                    raise ConversionError(
                        "Missing CSV does not match the edited Markdown table; back up edits and explicitly refresh."
                    )
            # Install assets before the Markdown file that declares them complete.
            pending.sort(key=lambda item: item[1].suffix == ".md")
            if legacy_markdown is not None:
                _check_legacy_target(existing.markdown_path, legacy_markdown)
            for staged, target in pending:
                if legacy_markdown is not None and target == existing.markdown_path:
                    _check_legacy_target(target, legacy_markdown)
                    os.replace(staged, target)
                    continue
                try:
                    os.link(staged, target)
                except FileExistsError:
                    # Preserve a concurrent creator's file, but still reject links.
                    _check_target(target, overwrite=True)
        repaired = inspect_artifacts(source, output)
        if repaired.missing:
            raise ConversionError(
                "Conversion files are still incomplete; retry after other file changes finish."
            )
        return repaired
    except ConversionError:
        raise
    except (OSError, ValueError):
        raise ConversionError(
            "Cannot repair conversion files; check paths, permissions, and free space."
        ) from None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Convert a PDF or directory of PDFs to Markdown, adjacent PNG images, and CSV tables (offline).",
        epilog="PyMuPDF4LLM / PyMuPDF: AGPL-3.0 or commercial license. See documentation/pdf-to-markdown.md.",
    )
    parser.add_argument(
        "input", type=Path, help="PDF file or directory (not recursive)"
    )
    parser.add_argument(
        "--output-dir", required=True, type=Path, help="destination directory"
    )
    parser.add_argument(
        "--overwrite", action="store_true", help="replace existing output files"
    )
    args = parser.parse_args(argv)
    try:
        paths = convert(args.input, args.output_dir, overwrite=args.overwrite)
    except ConversionError as error:
        print(f"PDF conversion error: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("PDF conversion cancelled.", file=sys.stderr)
        return 130
    print(f"Converted {len(paths)} PDF(s).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
