"""Offline converter contracts and generated-PDF integration tests."""

import csv
import hashlib
import importlib.util
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from src.indexing import pdf_to_markdown as converter


def fake_extract(source, staging):
    table = {
        "extract": [["Method", "Score"], ["Alpha", "12"]],
        "markdown": "|Method|Score|\n|---|---|\n|Alpha|12|\n",
    }
    layout = SimpleNamespace(
        pages=[
            SimpleNamespace(
                page_number=1,
                boxes=[SimpleNamespace(boxclass="table", table=table)],
            )
        ]
    )
    converter._export_tables(layout, staging)
    digest = hashlib.sha256(source.read_bytes()).hexdigest()
    (staging / "document.md").write_text(
        f"<!-- pdf-artifacts: v1 pages=1 sha256={digest} -->\n"
        "\n<!-- pdf-page: 1 -->\n"
        "# Example\n\n![](document-0001-01.png)\n\n" + table["markdown"],
        encoding="utf-8",
    )
    (staging / "document-0001-01.png").write_bytes(b"fixture image")


class ConverterFixture(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="pdf-converter-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source = self.root / "sample.pdf"
        self.source.write_bytes(b"minimal fake PDF")
        self.output = self.root / "output (with spaces)"

    def convert(self, source=None, **kwargs):
        return converter.convert(
            source or self.source,
            self.output,
            extractor=kwargs.pop("extractor", fake_extract),
            **kwargs,
        )

    def snapshot(self):
        return {p.name: p.read_bytes() for p in self.output.iterdir()}

    def read_csv(self, path):
        with path.open(encoding="utf-8", newline="") as stream:
            return list(csv.reader(stream))

    def cli(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "src.indexing.pdf_to_markdown", *map(str, args)],
            cwd=Path(__file__).resolve().parents[1],
            capture_output=True,
            text=True,
            check=False,
        )


class ConverterTests(ConverterFixture):


    def test_invalid_missing_empty_and_wrong_extension_inputs(self):
        empty = self.root / "empty"
        empty.mkdir()
        text = self.root / "input.txt"
        text.write_text("not a PDF")
        for source in (self.root / "absent.pdf", empty, text):
            with self.subTest(source=source):
                with self.assertRaises(converter.ConversionError):
                    self.convert(source)
        self.assertFalse(self.output.exists())


    def test_io_errors_are_fixed_messages(self):
        with patch.object(converter.os, "link", side_effect=OSError("private path")):
            with self.assertRaisesRegex(
                converter.ConversionError, "permissions"
            ) as error:
                self.convert()
        self.assertNotIn("private path", str(error.exception))

    def test_worker_errors_and_diagnostics_are_safely_mapped(self):
        for code, message in (
            (3, "unavailable"),
            (4, "unlocked"),
            (5, "valid"),
            (-11, "valid"),
        ):
            with (
                self.subTest(code=code),
                patch.object(converter.subprocess, "run") as run,
            ):
                run.return_value.returncode = code
                with self.assertRaisesRegex(converter.ConversionError, message):
                    converter._extract_pdf(self.source, self.root)
                self.assertEqual(run.call_args.kwargs["stdout"], subprocess.DEVNULL)
                self.assertEqual(run.call_args.kwargs["stderr"], subprocess.DEVNULL)


    def test_cli_cancellation_has_fixed_message_and_exit_code(self):
        with (
            patch.object(converter, "convert", side_effect=KeyboardInterrupt),
            patch("sys.stderr") as stderr,
        ):
            self.assertEqual(
                converter.main([str(self.source), "--output-dir", str(self.output)]),
                130,
            )
            self.assertIn(
                "PDF conversion cancelled.", stderr.write.call_args_list[0].args
            )

    def test_cli_help_validation_and_safe_conversion_error(self):
        help_result = self.cli("--help")
        self.assertEqual(help_result.returncode, 0)
        self.assertIn("--overwrite", help_result.stdout)
        self.assertIn("CSV", help_result.stdout)
        self.assertIn("AGPL-3.0", help_result.stdout)
        self.assertEqual(self.cli().returncode, 2)
        invalid = self.cli("missing.pdf", "--output-dir", self.output)
        self.assertEqual(invalid.returncode, 1)
        self.assertIn("Input must be", invalid.stderr)
        self.assertNotIn("Traceback", invalid.stderr)


class ArtifactTests(ConverterFixture):
    def ensure(self, **kwargs):
        return converter.ensure_artifacts(
            self.source,
            self.output,
            extractor=kwargs.pop("extractor", fake_extract),
            **kwargs,
        )


    def test_legacy_replacement_failure_preserves_original_markdown_and_assets(self):
        result = self.ensure()
        result.markdown_path.write_text("# Legacy")
        before = self.snapshot()
        with patch.object(converter.os, "replace", side_effect=OSError("denied")):
            with self.assertRaises(converter.ConversionError):
                self.ensure()
        self.assertEqual(self.snapshot(), before)


    def test_external_or_mismatched_asset_references_are_refused(self):
        result = self.ensure()
        original = result.markdown_path.read_text()
        variants = [
            original.replace("sample-image-0001.png", "../outside.png"),
            original.replace("sample-image-0001.png", "https://example.test/image.png"),
            original.replace("sample-page-0001-table-0001.csv", "../outside.csv"),
            original.replace(
                "sample-page-0001-table-0001.csv", "other-page-0001-table-0001.csv"
            ),
            original.replace("<!-- /pdf-table -->", ""),
        ]
        for text in variants:
            with self.subTest(text=text):
                result.markdown_path.write_text(text)
                with self.assertRaises(converter.ConversionError):
                    self.ensure()


HAS_ENGINE = importlib.util.find_spec("pymupdf4llm") is not None


@unittest.skipUnless(
    HAS_ENGINE, "Install environment.yml to run generated-PDF integration tests"
)
class GeneratedPdfTests(ConverterFixture):
    def setUp(self):
        super().setUp()
        import pymupdf

        self.source = self.root / "paper [one] é.pdf"
        with pymupdf.open() as document:
            page = document.new_page(width=600, height=800)
            page.insert_text((50, 50), "Converter fixture", fontsize=20)
            page.insert_text(
                (50, 85), "Offline text, figure, and table extraction.", fontsize=12
            )
            for y in (150, 185, 220, 255):
                page.draw_line((50, y), (450, y))
            for x in (50, 250, 450):
                page.draw_line((x, 150), (x, 255))
            for row, cells in enumerate(
                (("Method", "Score"), ("Alpha", "12"), ("Beta", "34"))
            ):
                for column, cell in enumerate(cells):
                    page.insert_text(
                        (60 + column * 200, 173 + row * 35), cell, fontsize=12
                    )
            image = pymupdf.Pixmap(pymupdf.csRGB, (0, 0, 128, 128), 0)
            for x in range(128):
                for y in range(128):
                    image.set_pixel(x, y, (x * 2, y * 2, 90))
            page.insert_image(pymupdf.Rect(50, 330, 250, 530), pixmap=image)
            page.insert_text((50, 555), "Figure 1. Generated gradient.", fontsize=11)
            document.save(self.source)

    def test_text_only_multiple_pages_need_no_image_assets(self):
        import pymupdf

        source = self.root / "text-only.pdf"
        with pymupdf.open() as document:
            for title in ("First page text", "Second page text"):
                document.new_page().insert_text((50, 50), title, fontsize=14)
            document.save(source)
        original_cwd = Path.cwd()
        paths = converter.convert(source, self.output)
        self.assertEqual(Path.cwd(), original_cwd)
        self.assertEqual(set(self.snapshot()), {"text-only.md"})
        markdown = paths[0].read_text()
        self.assertIn("First page text", markdown)
        self.assertIn("Second page text", markdown)
        artifacts = converter.inspect_artifacts(source, self.output)
        self.assertEqual([page.page_number for page in artifacts.pages], [1, 2])
        self.assertIn("First page text", artifacts.pages[0].text)
        self.assertNotIn("Second page text", artifacts.pages[0].text)
        self.assertIn("Second page text", artifacts.pages[1].text)
        self.assertTrue(all(not page.tables for page in artifacts.pages))
        with patch.object(
            converter, "convert", side_effect=AssertionError("no regeneration")
        ):
            self.assertEqual(converter.ensure_artifacts(source, self.output), artifacts)

    def test_generated_pdf_text_image_table_and_relocation(self):
        before = hashlib.sha256(self.source.read_bytes()).hexdigest()
        paths = converter.convert(self.source, self.output)
        markdown = paths[0].read_text()
        self.assertIn("Converter fixture", markdown)
        rows = [
            line.split("|")[1:-1]
            for line in markdown.splitlines()
            if line.startswith("|")
        ]
        self.assertEqual(
            rows, [["Method", "Score"], ["---", "---"], ["Alpha", "12"], ["Beta", "34"]]
        )
        links = re.findall(r"!\[[^\]]*\]\(([^)]+)\)", markdown)
        self.assertTrue(links)
        relocated = self.root / "relocated"
        shutil.copytree(self.output, relocated)
        for link in links:
            self.assertEqual(link, Path(link).name)
            self.assertTrue(
                (relocated / link).read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
            )
        csv_links = re.findall(r"\]\(([^)]+\.csv)\)", markdown)
        self.assertEqual(len(csv_links), 1)
        self.assertEqual(csv_links[0], f"{paths[0].stem}-page-0001-table-0001.csv")
        self.assertEqual(
            self.read_csv(relocated / csv_links[0]),
            [["Method", "Score"], ["Alpha", "12"], ["Beta", "34"]],
        )
        self.assertEqual(hashlib.sha256(self.source.read_bytes()).hexdigest(), before)
        first = self.snapshot()
        converter.convert(self.source, self.output, overwrite=True)
        self.assertEqual(self.snapshot(), first)

    def test_generated_repair_preserves_edits_and_restores_csv_and_png(self):
        artifacts = converter.ensure_artifacts(self.source, self.output)
        table = artifacts.pages[0].tables[0].csv_path
        image = artifacts.images[0]
        expected_table, expected_image = table.read_bytes(), image.read_bytes()
        table.rename(self.root / "saved.csv")
        image.rename(self.root / "saved.png")
        artifacts.markdown_path.write_text(
            artifacts.markdown_path.read_text().replace(
                "Converter fixture", "User annotation"
            )
        )
        before_markdown = artifacts.markdown_path.read_bytes()
        repaired = converter.ensure_artifacts(self.source, self.output)
        self.assertEqual(repaired.missing, ())
        self.assertEqual(artifacts.markdown_path.read_bytes(), before_markdown)
        self.assertEqual(table.read_bytes(), expected_table)
        self.assertEqual(image.read_bytes(), expected_image)

    def test_generated_blank_page_keeps_original_page_number(self):
        import pymupdf

        source = self.root / "blank-page.pdf"
        with pymupdf.open() as document:
            document.new_page()
            document.new_page().insert_text((50, 50), "Only page two has text")
            document.save(source)
        artifacts = converter.ensure_artifacts(source, self.output)
        self.assertEqual([page.page_number for page in artifacts.pages], [1, 2])
        self.assertEqual(artifacts.pages[0].text.strip(), "")
        self.assertIn("Only page two has text", artifacts.pages[1].text)

    def test_generated_legacy_regeneration_keeps_csv_and_images(self):
        artifacts = converter.ensure_artifacts(self.source, self.output)
        expected = artifacts.markdown_path.read_bytes()
        artifacts.markdown_path.write_text("# Legacy conversion without pages")
        artifacts.pages[0].tables[0].csv_path.write_bytes(b"edited,cells\r\n")
        before_assets = {
            path: path.read_bytes()
            for path in artifacts.paths
            if path != artifacts.markdown_path
        }
        repaired = converter.ensure_artifacts(self.source, self.output)
        self.assertEqual(repaired.markdown_path.read_bytes(), expected)
        self.assertEqual(repaired.missing, ())
        for path, content in before_assets.items():
            self.assertEqual(path.read_bytes(), content)

    def test_generated_multiple_pages_and_tables_export_separate_csvs(self):
        import pymupdf

        source = self.root / "tables.pdf"
        with pymupdf.open() as document:
            for page_number in (1, 2):
                page = document.new_page(width=600, height=800)
                for table_number, top in enumerate((150, 500), 1):
                    page.insert_text((50, top - 40), "Measurements", fontsize=16)
                    for y in (top, top + 35, top + 70, top + 105):
                        page.draw_line((50, y), (450, y))
                    for x in (50, 250, 450):
                        page.draw_line((x, top), (x, top + 105))
                    rows = (
                        ("Item", "Value"),
                        (f"P{page_number}T{table_number}", "001"),
                        ("Other", "2"),
                    )
                    for row, cells in enumerate(rows):
                        for col, cell in enumerate(cells):
                            page.insert_text(
                                (60 + col * 200, top + 23 + row * 35), cell
                            )
            document.save(source)
        paths = converter.convert(source, self.output)
        markdown = paths[0].read_text()
        self.assertEqual(len(list(self.output.glob("*.csv"))), 4)
        artifacts = converter.inspect_artifacts(source, self.output)
        self.assertEqual([page.page_number for page in artifacts.pages], [1, 2])
        self.assertEqual([len(page.tables) for page in artifacts.pages], [2, 2])
        for page in (1, 2):
            for table in (1, 2):
                name = f"tables-page-{page:04d}-table-{table:04d}.csv"
                self.assertIn(f"]({name})", markdown)
                self.assertIn(f"P{page}T{table}", markdown)
                self.assertEqual(
                    self.read_csv(self.output / name),
                    [["Item", "Value"], [f"P{page}T{table}", "001"], ["Other", "2"]],
                )

    def test_real_cli_single_directory_overwrite_and_malformed_pdf(self):
        result = self.cli(self.source, "--output-dir", self.output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "Converted 1 PDF(s).\n")
        self.assertEqual(result.stderr, "")
        before = self.snapshot()
        self.assertEqual(
            self.cli(self.source, "--output-dir", self.output).returncode, 1
        )
        self.assertEqual(self.snapshot(), before)
        self.assertEqual(
            self.cli(
                self.source, "--output-dir", self.output, "--overwrite"
            ).returncode,
            0,
        )
        inputs = self.root / "batch"
        inputs.mkdir()
        shutil.copyfile(self.source, inputs / "a.pdf")
        shutil.copyfile(self.source, inputs / "b.PDF")
        batch = self.cli(inputs, "--output-dir", self.root / "batch-output")
        self.assertEqual(batch.returncode, 0, batch.stderr)
        self.assertEqual(batch.stdout, "Converted 2 PDF(s).\n")
        csv_paths = sorted((self.root / "batch-output").glob("*.csv"))
        self.assertEqual(
            [p.name for p in csv_paths],
            ["a-page-0001-table-0001.csv", "b-page-0001-table-0001.csv"],
        )
        for path in csv_paths:
            self.assertEqual(
                self.read_csv(path),
                [["Method", "Score"], ["Alpha", "12"], ["Beta", "34"]],
            )
        malformed = self.root / "bad.pdf"
        malformed.write_bytes(b"%PDF-1.7\nnot a valid document")
        error = self.cli(malformed, "--output-dir", self.root / "bad-output")
        self.assertEqual(error.returncode, 1)
        self.assertEqual(error.stdout, "")
        self.assertEqual(
            error.stderr,
            "PDF conversion error: PDF conversion failed; check that the PDF is readable and valid.\n",
        )

    def test_password_protected_pdf_has_actionable_error(self):
        import pymupdf

        protected = self.root / "protected.pdf"
        with pymupdf.open(self.source) as document:
            document.save(
                protected,
                encryption=pymupdf.PDF_ENCRYPT_AES_256,
                owner_pw="fixture-owner",
                user_pw="fixture-user",
            )
        result = self.cli(protected, "--output-dir", self.output)
        self.assertEqual(result.returncode, 1)
        self.assertIn("provide an unlocked copy", result.stderr)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
