#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import copy
import json
import unittest

from tools.check_native_g6_foundation import GateFailure
from tools.check_native_g6_receipt import validate
from tools.produce_native_g6_receipt import build_receipt
from tools.test_native_g6_evidence_support import COMMIT, digests, implemented_raw, suite_logs


class NativeG6ReceiptAuditTests(unittest.TestCase):
    def fixture(self):
        raw = implemented_raw()
        manifest_sha256 = digests(raw)
        receipt = build_receipt(COMMIT, "shared-contracts-and-errors", raw, manifest_sha256, "linux", {"cargo": "cargo 1.96.0", "python": "Python 3.11.15"}, suite_logs(raw))
        documents = {name: json.loads(value) for name, value in raw.items()}
        return receipt, manifest_sha256, documents

    def test_audit_preserves_exact_identity_and_command_results(self) -> None:
        receipt, manifest_sha256, documents = self.fixture()
        audit = validate(receipt, COMMIT, "shared-contracts-and-errors", manifest_sha256, documents["authority"], documents["workload"], documents["suite"], documents["inventory"])
        self.assertEqual(audit["schema"], "hyphae-native-g6-receipt-audit-v1")
        self.assertEqual(audit["suite_count"], len(receipt["command_results"]))
        self.assertEqual(audit["test_count"], receipt["test_count"])
        self.assertFalse(audit["closure_declared"])

    def test_extra_field_command_tamper_and_authority_tamper_fail(self) -> None:
        receipt, manifest_sha256, documents = self.fixture()
        changed = copy.deepcopy(receipt)
        changed["claim"] = "complete"
        with self.assertRaisesRegex(GateFailure, "fields mismatch"):
            validate(changed, COMMIT, "shared-contracts-and-errors", manifest_sha256, documents["authority"], documents["workload"], documents["suite"])
        changed = copy.deepcopy(receipt)
        changed["command_results"][0]["command_sha256"] = "0" * 64
        with self.assertRaisesRegex(GateFailure, "command result"):
            validate(changed, COMMIT, "shared-contracts-and-errors", manifest_sha256, documents["authority"], documents["workload"], documents["suite"])
        changed = copy.deepcopy(receipt)
        changed["authority"]["identity_sha256"] = "0" * 64
        with self.assertRaisesRegex(GateFailure, "authority identity"):
            validate(changed, COMMIT, "shared-contracts-and-errors", manifest_sha256, documents["authority"], documents["workload"], documents["suite"])

    def test_failed_exit_result_fails_closed(self) -> None:
        receipt, manifest_sha256, documents = self.fixture()
        receipt["command_results"][0]["exit_code"] = 1
        with self.assertRaisesRegex(GateFailure, "command result"):
            validate(receipt, COMMIT, "shared-contracts-and-errors", manifest_sha256, documents["authority"], documents["workload"], documents["suite"])


if __name__ == "__main__":
    unittest.main()
