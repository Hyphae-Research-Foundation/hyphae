#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Deterministic coverage for the declarative extractive reader."""

from __future__ import annotations

import unittest

from tools.reader_extractive import (
    read_span,
    bleu1,
    extract_entities,
    qa_metrics,
    read_list,
    read_single,
    token_f1,
)


class ReaderExtractiveTests(unittest.TestCase):
    def test_f1_has_exact_endpoints(self) -> None:
        self.assertAlmostEqual(token_f1("Alice was born in March", "Alice was born in March"), 1.0)
        self.assertAlmostEqual(token_f1("completely different words", "Alice was born in March"), 0.0)
        self.assertAlmostEqual(token_f1("", ""), 1.0)
        self.assertAlmostEqual(token_f1("", "Alice"), 0.0)

    def test_f1_ignores_articles_case_and_punctuation(self) -> None:
        self.assertAlmostEqual(
            token_f1("The cat, sat.", "a cat sat"), 1.0
        )

    def test_f1_partial_overlap_is_harmonic(self) -> None:
        value = token_f1("Alice born July", "Alice was born in March")
        self.assertGreater(value, 0.0)
        self.assertLess(value, 1.0)

    def test_bleu1_precision_based(self) -> None:
        self.assertAlmostEqual(bleu1("Alice was born", "Alice was born in March"), 1.0)
        self.assertAlmostEqual(bleu1("March in born was Alice", "Alice was born in March"), 1.0)
        self.assertAlmostEqual(bleu1("no shared content here", "Alice was born in March"), 0.0)

    def test_extract_entities_deduplicates_case_insensitively(self) -> None:
        entities = extract_entities("Caroline loves Paris. paris again. 42 miles and 3 pm.")
        self.assertIn("Caroline", entities)
        self.assertIn("Paris", entities)
        self.assertIn("42 miles", entities)
        self.assertIn("3 pm", entities)
        self.assertEqual(len([e for e in entities if e.lower() == "paris"]), 1)

    def test_read_list_bounds_and_orders(self) -> None:
        text = "Tim, Sam, Evan, Zoe, Ana, Kim, Leo, Max, Rex, Uma visited."
        listed = read_list(text, limit=8)
        self.assertEqual(len(listed.split(", ")), 8)

    def test_read_single_is_identity_trimmed(self) -> None:
        self.assertEqual(read_single("  exact turn  "), "exact turn")

    def test_read_span_prefers_entities_dates_and_numbers(self) -> None:
        span = read_span("Caroline went to Paris on 7 May 2023 with 2 friends.")
        self.assertIn("Caroline", span)
        self.assertIn("Paris", span)
        self.assertIn("7 May 2023", span)
        self.assertIn("2", span)

    def test_qa_metrics_take_best_reference(self) -> None:
        metrics = qa_metrics("Alice was born in March", ["wrong", "Alice was born in March"])
        self.assertAlmostEqual(metrics["f1"], 1.0)
        self.assertAlmostEqual(metrics["bleu1"], 1.0)
        with self.assertRaises(ValueError):
            qa_metrics("x", [])


if __name__ == "__main__":
    unittest.main()
