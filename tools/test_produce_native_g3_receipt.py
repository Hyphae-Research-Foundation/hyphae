#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import hashlib
import json
import unittest

from tools.produce_native_g3_receipt import GateFailure, build_receipt


MANIFEST = {
    "schema": "hyphae-native-g3-suite-manifest-v1",
    "gate": "G3",
    "requirements": [{
        "id": "streams",
        "suites": [{"name": "stream-model", "command": ["cargo", "test", "stream_model_g3"]}],
    }],
}
DIGEST = hashlib.sha256(json.dumps(MANIFEST).encode()).hexdigest()
MARKER = 'G3_COMMAND: ["cargo","test","stream_model_g3"]\n'


class ProducerTests(unittest.TestCase):
    def test_emits_suite_bound_exact_sha_receipt(self):
        payload = build_receipt("a" * 40, "streams", MANIFEST, DIGEST, "linux-x86_64", "1.96.0", [
            ("stream-model", (MARKER + "test result: ok. 3 passed; 0 failed; 0 ignored\n").encode()),
        ])
        self.assertEqual(payload["source_commit"], "a" * 40)
        self.assertEqual(payload["test_count"], 3)
        self.assertEqual(payload["suites"][0]["name"], "stream-model")

    def test_rejects_wrong_or_zero_test_suite(self):
        with self.assertRaises(GateFailure):
            build_receipt("a" * 40, "streams", MANIFEST, DIGEST, "linux", "1.96.0", [
                ("unrelated", b"test result: ok. 1 passed; 0 failed;\n"),
            ])
        with self.assertRaises(GateFailure):
            build_receipt("a" * 40, "streams", MANIFEST, DIGEST, "linux", "1.96.0", [
                ("stream-model", (MARKER + "test result: ok. 0 passed; 0 failed;\n").encode()),
            ])


if __name__ == "__main__":
    unittest.main()
