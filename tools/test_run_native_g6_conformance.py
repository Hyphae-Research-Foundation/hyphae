# SPDX-License-Identifier: GPL-3.0-only
from __future__ import annotations

import json
import unittest

from tools.run_native_g6_conformance import RunFailure, evidence_commands, parse_transcript
from tools.test_check_native_g6_conformance import transcript


class NativeG6RunnerTests(unittest.TestCase):
    def test_parser_requires_one_nonempty_json_transcript(self) -> None:
        value = transcript("embedded-rust")
        parsed = parse_transcript(json.dumps(value), "embedded-rust")
        self.assertEqual(parsed["lane"], "embedded-rust")

        with self.assertRaisesRegex(RunFailure, "2 nonempty"):
            parse_transcript("{}\n{}\n", "embedded-rust")
        with self.assertRaisesRegex(Exception, "lane identity"):
            parse_transcript(json.dumps(value), "http")

    def test_platform_daemon_suite_is_selected_by_runner(self) -> None:
        self.assertIn("daemon_uds", evidence_commands()[-2])


if __name__ == "__main__":
    unittest.main()
