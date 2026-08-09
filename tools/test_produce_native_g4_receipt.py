#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.produce_native_g4_receipt import GateFailure, build_receipt


SUITE = {"schema": "hyphae-native-g4-suite-manifest-v1", "gate": "G4", "requirements": [{"id": "ann-search", "corpora": ["vectors"], "suites": [{"name": "ann", "command": ["cargo", "test", "ann"]}]}], "claims": [], "closure_declared": False}
CORPUS = {"schema": "hyphae-native-g4-corpus-manifest-v1", "gate": "G4", "corpora": [{"id": "vectors", "source": "fixture", "sha256": "f" * 64, "requirements": ["ann-search"]}], "claims": [], "closure_declared": False}
SUITE_RAW = json.dumps(SUITE).encode()
CORPUS_RAW = json.dumps(CORPUS).encode()
SUITE_SHA = hashlib.sha256(SUITE_RAW).hexdigest()
CORPUS_SHA = hashlib.sha256(CORPUS_RAW).hexdigest()
LOG = b'G4_COMMAND: ["cargo","test","ann"]\ntest result: ok. 3 passed; 0 failed; 0 ignored\n'


class ProducerTests(unittest.TestCase):
    def test_emits_exact_sha_manifest_and_corpus_bound_receipt(self):
        result = build_receipt("a" * 40, "ann-search", SUITE_RAW, SUITE_SHA, CORPUS_RAW, CORPUS_SHA, "linux-x86_64", "1.96.0", [("ann", LOG)])
        self.assertEqual(result["test_count"], 3)
        self.assertEqual(result["corpora"], ["vectors"])
        self.assertEqual((result["claims"], result["closure_declared"]), ([], False))

    def test_rejects_digest_log_and_corpus_substitution(self):
        cases = [
            (SUITE_RAW, "0" * 64, CORPUS_RAW, CORPUS_SHA, [("ann", LOG)]),
            (SUITE_RAW, SUITE_SHA, CORPUS_RAW, "0" * 64, [("ann", LOG)]),
            (SUITE_RAW, SUITE_SHA, CORPUS_RAW, CORPUS_SHA, [("other", LOG)]),
        ]
        for suite, suite_sha, corpus, corpus_sha, logs in cases:
            with self.subTest(suite_sha=suite_sha, corpus_sha=corpus_sha, logs=logs), self.assertRaises(GateFailure):
                build_receipt("a" * 40, "ann-search", suite, suite_sha, corpus, corpus_sha, "linux", "1.96.0", logs)

    def test_validates_corpus_source_digest_when_root_is_supplied(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "fixture").write_bytes(b"corpus")
            corpus = json.loads(CORPUS_RAW)
            corpus["corpora"][0]["sha256"] = hashlib.sha256(b"corpus").hexdigest()
            raw = json.dumps(corpus).encode()
            result = build_receipt("a" * 40, "ann-search", SUITE_RAW, SUITE_SHA, raw, hashlib.sha256(raw).hexdigest(), "linux", "1.96.0", [("ann", LOG)], root)
            self.assertEqual(result["corpora"], ["vectors"])
            (root / "fixture").write_bytes(b"changed")
            with self.assertRaises(GateFailure):
                build_receipt("a" * 40, "ann-search", SUITE_RAW, SUITE_SHA, raw, hashlib.sha256(raw).hexdigest(), "linux", "1.96.0", [("ann", LOG)], root)


if __name__ == "__main__":
    unittest.main()
