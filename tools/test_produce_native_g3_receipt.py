#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import tempfile
import unittest
from pathlib import Path

from tools import produce_native_g3_receipt as producer


class ProducerTests(unittest.TestCase):
    def test_rejects_zero_test_success(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "log"
            log.write_text("test result: ok. 0 passed; 0 failed\n")
            with self.assertRaises(SystemExit):
                self._run(log, root / "out.json")

    def test_emits_content_bound_exact_sha_audit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "log"
            log.write_text("test result: ok. 3 passed; 0 failed; 0 ignored\n")
            output = root / "out.json"
            self.assertEqual(self._run(log, output), 0)
            payload = json.loads(output.read_text())
            self.assertEqual(payload["source_commit"], "a" * 40)
            self.assertEqual(payload["test_count"], 3)

    @staticmethod
    def _run(log: Path, output: Path) -> int:
        import sys
        previous = sys.argv
        sys.argv = [
            "producer",
            "--requirement", "streams",
            "--source-commit", "a" * 40,
            "--log", str(log),
            "--output", str(output),
        ]
        try:
            return producer.main()
        finally:
            sys.argv = previous


if __name__ == "__main__":
    unittest.main()
