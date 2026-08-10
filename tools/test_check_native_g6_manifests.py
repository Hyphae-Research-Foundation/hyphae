#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import copy
import unittest

from tools.check_native_g6_foundation import GateFailure
from tools.check_native_g6_manifests import load_exact, validate, validate_raw
from tools.test_native_g6_evidence_support import COMMIT, ROOT, checked_raw, digests, payloads


class NativeG6ManifestTests(unittest.TestCase):
    def test_checked_in_manifests_bind_all_seven_exact_digests(self) -> None:
        raw = checked_raw()
        exact = load_exact(raw, digests(raw))
        self.assertEqual(len(exact), 7)
        result, _ = validate_raw(ROOT, raw, COMMIT, digests(raw))
        self.assertEqual((result["status"], result["requirements"], result["predecessor_count"]), ("passed", 14, 6))
        self.assertEqual(result["manifest_sha256"], digests(raw))
        self.assertEqual((result["closure_status"], result["claims"], result["closure_declared"]), ("open", [], False))

    def test_digest_substitution_and_missing_manifest_fail_closed(self) -> None:
        raw = checked_raw()
        changed = digests(raw)
        changed["authority"] = "0" * 64
        with self.assertRaisesRegex(GateFailure, "authority manifest digest mismatch"):
            load_exact(raw, changed)
        missing = dict(raw)
        del missing["evidence"]
        with self.assertRaisesRegex(GateFailure, "all seven"):
            load_exact(missing, digests(raw))

    def test_claim_and_implemented_status_mismatch_fail_closed(self) -> None:
        raw = checked_raw()
        documents = payloads(raw)
        changed = copy.deepcopy(documents)
        changed["authority"]["claims"] = ["G6 complete"]
        with self.assertRaisesRegex(GateFailure, "open and claim-free"):
            validate(ROOT, *changed.values(), COMMIT, digests(raw))


if __name__ == "__main__":
    unittest.main()
