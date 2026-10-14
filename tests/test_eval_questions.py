"""Offline integrity checks for the curated RAG evaluation dataset."""

import json
import re
import unicodedata
import unittest
from collections import Counter
from pathlib import Path

from src.evaluation.runner import load_test_cases


ROOT = Path(__file__).resolve().parents[1]
DATASET = ROOT / "tests" / "eval_questions.json"
PERFORMANCE_DATASET = ROOT / "tests" / "eval_questions_performance.json"
CORPUS = ROOT / "ResearchPapers"


def category(case):
    match = re.match(r"Category: ([a-z_]+)[.;]", case["grading_notes"])
    return match.group(1) if match else "legacy"


def normalized_pdf_text(text):
    text = unicodedata.normalize("NFKC", text)
    text = re.sub(r"(?<=\w)-\s*\n\s*(?=\w)", "", text)
    return " ".join(text.split())


class EvaluationQuestionsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cases = json.loads(DATASET.read_text(encoding="utf-8"))

    def test_curated_dataset_stays_at_seventy_questions(self):
        self.assertEqual(len(self.cases), 70)


    def test_questions_and_source_lists_are_unique(self):
        questions = [
            " ".join(case["question"].casefold().split()) for case in self.cases
        ]
        self.assertEqual(len(questions), len(set(questions)))
        for case in self.cases:
            self.assertEqual(len(case["sources"]), len(set(case["sources"])))

    def test_source_names_are_pdf_basenames_and_exist_when_corpus_is_available(self):
        for case in self.cases:
            for source in case["sources"]:
                with self.subTest(source=source):
                    self.assertEqual(Path(source).name, source)
                    self.assertNotIn("\\", source)
                    self.assertTrue(source.endswith(".pdf"))
                    if CORPUS.is_dir():
                        self.assertTrue((CORPUS / source).is_file())

    def test_expansion_preserves_all_seven_question_families(self):
        counts = Counter(category(case) for case in self.cases)
        minimums = {
            "enumeration": 3,
            "nonexistent_source": 2,
            "partial_list": 1,
            "multi_hop": 3,
            "contradiction": 2,
            "precise_quote": 2,
            "boundary": 1,
        }
        for family, minimum in minimums.items():
            with self.subTest(family=family):
                self.assertGreaterEqual(counts[family], minimum)
        self.assertGreaterEqual(
            counts["enumeration"],
            max(counts[k] for k in minimums if k != "enumeration"),
        )

    def test_security_cases_cover_threats_and_safe_responses(self):
        security = [case for case in self.cases if category(case) == "security"]
        self.assertEqual(len(security), 10)
        self.assertEqual(security, self.cases[60:])
        families, behaviors, scenarios = Counter(), Counter(), set()
        for case in security:
            with self.subTest(question=case["question"]):
                metadata = re.match(
                    r"Category: security; family: ([a-z_]+); "
                    r"scenario: ([a-z_]+); expected_behavior: ([a-z_]+)\.",
                    case["grading_notes"],
                )
                self.assertIsNotNone(metadata)
                family, scenario, behavior = metadata.groups()
                families[family] += 1
                behaviors[behavior] += 1
                scenarios.add(scenario)
                self.assertIn("Pass:", case["grading_notes"])
                self.assertIn("Fail:", case["grading_notes"])
                if behavior in {"answer", "answer_safe_part"}:
                    self.assertTrue(case["sources"])
                else:
                    self.assertEqual(case["sources"], [])
        self.assertEqual(families, {"misuse": 3, "adversarial": 4, "edge_case": 3})
        self.assertEqual(
            behaviors,
            {"refuse": 4, "answer_safe_part": 3, "clarify": 2, "answer": 1},
        )
        self.assertEqual(len(scenarios), 10)

    def test_enumeration_gold_sets_span_small_and_large_answers(self):
        sizes = []
        for case in self.cases:
            if category(case) == "enumeration":
                match = re.search(
                    r"expected_unique_works: (\d+)", case["grading_notes"]
                )
                self.assertIsNotNone(match, case["question"])
                size = int(match.group(1))
                sizes.append(size)
                self.assertEqual(size, len(case["sources"]), case["question"])
        self.assertIn(2, sizes)
        self.assertTrue(any(3 <= size <= 5 for size in sizes))
        self.assertTrue(any(size >= 10 for size in sizes))

    def test_multi_hop_and_missing_evidence_cases_are_explicit(self):
        hops = []
        for case in self.cases:
            if category(case) == "multi_hop":
                match = re.search(r"hops: (\d+)", case["grading_notes"])
                self.assertIsNotNone(match, case["question"])
                hops.append(int(match.group(1)))
                self.assertGreaterEqual(len(case["sources"]), 2)
            elif category(case) == "partial_list":
                self.assertIn(
                    "requested_items: 5; supported_items: 3", case["grading_notes"]
                )
        self.assertIn(2, hops)
        self.assertIn(3, hops)
        self.assertIn(4, hops)
        absent = [
            c
            for c in self.cases
            if category(c) == "nonexistent_source" and not c["sources"]
        ]
        self.assertGreaterEqual(len(absent), 1)
        near_misses = [
            c
            for c in self.cases
            if category(c) == "nonexistent_source" and c["sources"]
        ]
        self.assertGreaterEqual(len(near_misses), 1)

    def test_verbatim_gold_quotes_occur_on_the_cited_pdf_pages(self):
        if not CORPUS.is_dir():
            self.skipTest("Local PDF corpus is not installed")
        try:
            from pypdf import PdfReader
        except ImportError:
            self.skipTest(
                "Run in the declared ragcourse environment for PDF evidence checks"
            )
        for case in self.cases:
            if category(case) != "precise_quote":
                continue
            with self.subTest(question=case["question"]):
                quote = re.search(r"Verbatim: “([^”]+)”", case["grading_notes"])
                page = re.search(r"PDF page (\d+)", case["grading_notes"])
                self.assertIsNotNone(quote)
                self.assertIsNotNone(page)
                reader = PdfReader(CORPUS / case["sources"][0])
                text = reader.pages[int(page.group(1)) - 1].extract_text()
                self.assertIn(
                    normalized_pdf_text(quote.group(1)), normalized_pdf_text(text)
                )


class PerformanceQuestionsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cases = json.loads(PERFORMANCE_DATASET.read_text(encoding="utf-8"))

    def test_round_trips_through_production_loader(self):
        loaded = load_test_cases(PERFORMANCE_DATASET)
        self.assertEqual([case.as_dict() for case in loaded], self.cases)

    def test_covers_performance_efficiency_and_reliability(self):
        counts = Counter(category(case) for case in self.cases)
        self.assertEqual(
            counts, {"performance": 3, "efficiency": 3, "reliability": 4}
        )
        for case in self.cases:
            with self.subTest(question=case["question"]):
                self.assertRegex(
                    case["grading_notes"],
                    r"^Category: [a-z_]+; dimension: [a-z_]+; "
                    r"expected_behavior: [a-z_]+\.",
                )
                self.assertIn("Pass:", case["grading_notes"])
                self.assertIn("Fail:", case["grading_notes"])
                for source in case["sources"]:
                    if CORPUS.is_dir():
                        self.assertTrue((CORPUS / source).is_file())

    def test_performance_and_efficiency_cases_carry_checked_budgets(self):
        for case in self.cases:
            kind = category(case)
            with self.subTest(question=case["question"]):
                if kind == "performance":
                    self.assertIn("max_rag_latency_ms", case["performance_budget"])
                elif kind == "efficiency":
                    self.assertEqual(
                        case["performance_budget"]["max_followup_queries"], 0
                    )
                    self.assertIn("max_retrievals", case["performance_budget"])


if __name__ == "__main__":
    unittest.main()
