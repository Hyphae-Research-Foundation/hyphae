#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import copy
import json
import unittest

from tools.check_native_g6_foundation import GateFailure
from tools.produce_native_g6_receipt import build_receipt
from tools.test_native_g6_evidence_support import COMMIT, digests, implemented_raw, suite_logs


class NativeG6ReceiptProducerTests(unittest.TestCase):
    def fixture(self):
        raw = implemented_raw()
        return raw, digests(raw), suite_logs(raw)

    @staticmethod
    def tool_versions() -> dict[str, str]:
        return {"cargo": "cargo 1.96.0", "python": "Python 3.11.15"}

    def test_receipt_binds_exact_sha_acceptance_commands_platform_and_tools(self) -> None:
        raw, manifest_sha256, logs = self.fixture()
        result = build_receipt(COMMIT, "shared-contracts-and-errors", raw, manifest_sha256, "linux", self.tool_versions(), logs)
        self.assertEqual(result["manifest_sha256"], manifest_sha256)
        self.assertEqual(result["workload"]["id"], "surface-error-parity")
        self.assertEqual(set(result["workload"]["acceptance"]), {"stable-code-registry", "unknown-commit", "redaction", "local-http-sdk-parity"})
        self.assertEqual(result["sdks"], ["rust", "python", "typescript"])
        self.assertEqual(result["test_count"], len(logs))
        self.assertTrue(all(row["status"] == "passed" and row["command_sha256"] for row in result["command_results"]))
        self.assertEqual((result["claims"], result["closure_declared"]), ([], False))

    def test_command_substitution_digest_substitution_and_failed_log_fail(self) -> None:
        raw, manifest_sha256, logs = self.fixture()
        changed_logs = copy.deepcopy(logs)
        changed_logs[0] = (changed_logs[0][0], changed_logs[0][1].replace(b"--locked", b"--release"))
        with self.assertRaisesRegex(GateFailure, "exact command"):
            build_receipt(COMMIT, "shared-contracts-and-errors", raw, manifest_sha256, "linux", self.tool_versions(), changed_logs)
        changed_digest = dict(manifest_sha256)
        changed_digest["suite"] = "0" * 64
        with self.assertRaisesRegex(GateFailure, "suite manifest digest mismatch"):
            build_receipt(COMMIT, "shared-contracts-and-errors", raw, changed_digest, "linux", self.tool_versions(), logs)
        failed_logs = copy.deepcopy(logs)
        failed_logs[0] = (failed_logs[0][0], failed_logs[0][1] + b"test result: FAILED\n")
        with self.assertRaisesRegex(GateFailure, "failed or invalid"):
            build_receipt(COMMIT, "shared-contracts-and-errors", raw, manifest_sha256, "linux", self.tool_versions(), failed_logs)

    def test_python_and_node_results_are_supported(self) -> None:
        raw = implemented_raw()
        documents = {name: json.loads(value) for name, value in raw.items()}
        row = documents["suite"]["requirements"][0]
        row["suites"] = [
            {"name": "python", "acceptance": ["stable-code-registry", "unknown-commit"], "coverage": {"sdks": ["python"], "transports": []}, "command": ["python3", "-m", "unittest", "tools.test_example"]},
            {"name": "node", "acceptance": ["redaction", "local-http-sdk-parity"], "coverage": {"sdks": ["typescript"], "transports": []}, "command": ["node", "--test", "sdk/typescript/example.test.js"]},
        ]
        raw = {name: json.dumps(value, sort_keys=True).encode() for name, value in documents.items()}
        logs = [
            ("python", b'G6_COMMAND: ["python3","-m","unittest","tools.test_example"]\nRan 2 tests in 0.1s\nOK\nG6_EXIT_CODE: 0\n'),
            ("node", b'G6_COMMAND: ["node","--test","sdk/typescript/example.test.js"]\n# tests 3\n# pass 3\n# fail 0\nG6_EXIT_CODE: 0\n'),
        ]
        receipt = build_receipt(COMMIT, "shared-contracts-and-errors", raw, digests(raw), "windows", {"python3": "3.13", "node": "24"}, logs)
        self.assertEqual(receipt["sdks"], ["python", "typescript"])
        self.assertEqual(receipt["test_count"], 5)

    def test_zero_test_cargo_target_cannot_be_spoofed_by_unrelated_target(self) -> None:
        raw, manifest_sha256, logs = self.fixture()
        name, log = logs[0]
        spoof = log.replace(
            b"Running unittests src/lib.rs (target/debug/deps/library)\n",
            b"Running tests/unrelated.rs (target/debug/deps/unrelated)\n",
        ).replace(b"1 passed", b"0 passed", 1)
        spoof += b"Running tests/other.rs (target/debug/deps/other)\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        changed = list(logs)
        changed[0] = (name, spoof)
        with self.assertRaisesRegex(GateFailure, "Cargo library suite executed no tests"):
            build_receipt(COMMIT, "shared-contracts-and-errors", raw, manifest_sha256, "linux", self.tool_versions(), changed)

    def test_colored_cargo_output_retains_the_selected_target(self) -> None:
        raw, manifest_sha256, logs = self.fixture()
        name, log = logs[0]
        colored = log.replace(
            b"Running unittests src/lib.rs",
            b"\x1b[1m\x1b[92m     Running\x1b[0m unittests src/lib.rs",
        ).replace(
            b"test result: ok.",
            b"\x1b[1m\x1b[92mtest result: ok.\x1b[0m",
        )
        changed = list(logs)
        changed[0] = (name, colored)
        receipt = build_receipt(
            COMMIT,
            "shared-contracts-and-errors",
            raw,
            manifest_sha256,
            "linux",
            self.tool_versions(),
            changed,
        )
        self.assertGreater(receipt["test_count"], 0)

    def test_zero_test_python_and_npm_fail(self) -> None:
        raw = implemented_raw()
        documents = {name: json.loads(value) for name, value in raw.items()}
        documents["suite"]["requirements"][0]["suites"] = [
            {"name": "python", "acceptance": ["stable-code-registry", "unknown-commit"], "coverage": {"sdks": ["python"], "transports": []}, "command": ["python", "-m", "unittest", "suite"]},
            {"name": "npm", "acceptance": ["redaction", "local-http-sdk-parity"], "coverage": {"sdks": ["typescript"], "transports": []}, "command": ["npm", "test", "--prefix", "suite"]},
        ]
        raw = {name: json.dumps(value, sort_keys=True).encode() for name, value in documents.items()}
        logs = [
            ("python", b'G6_COMMAND: ["python","-m","unittest","suite"]\nRan 0 tests in 0.0s\nOK\nG6_EXIT_CODE: 0\n'),
            ("npm", b'G6_COMMAND: ["npm","test","--prefix","suite"]\n# tests 0\n# pass 0\n# fail 0\nG6_EXIT_CODE: 0\n'),
        ]
        with self.assertRaisesRegex(GateFailure, "no tests|failed or invalid"):
            build_receipt(COMMIT, "shared-contracts-and-errors", raw, digests(raw), "linux", {"python": "3.11", "npm": "10"}, logs)

    def test_npm_accepts_node_test_unicode_summary(self) -> None:
        raw = implemented_raw()
        documents = {name: json.loads(value) for name, value in raw.items()}
        documents["suite"]["requirements"][0]["suites"] = [
            {
                "name": "npm",
                "acceptance": ["stable-code-registry", "unknown-commit", "redaction", "local-http-sdk-parity"],
                "coverage": {"sdks": ["typescript"], "transports": []},
                "command": ["npm", "test", "--prefix", "suite"],
            }
        ]
        raw = {name: json.dumps(value, sort_keys=True).encode() for name, value in documents.items()}
        logs = [
            (
                "npm",
                b'G6_COMMAND: ["npm","test","--prefix","suite"]\n'
                b'\xe2\x84\xb9 tests 19\n\xe2\x84\xb9 pass 19\n\xe2\x84\xb9 fail 0\nG6_EXIT_CODE: 0\n',
            )
        ]
        receipt = build_receipt(
            COMMIT,
            "shared-contracts-and-errors",
            raw,
            digests(raw),
            "linux",
            {"npm": "11"},
            logs,
        )
        self.assertEqual(receipt["test_count"], 19)


if __name__ == "__main__":
    unittest.main()
