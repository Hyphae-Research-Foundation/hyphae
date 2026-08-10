#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

import copy
import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g5_manifests import GateFailure as ManifestFailure, validate as validate_manifests
from tools.check_native_g5_predecessors import GateFailure as PredecessorFailure, audit as audit_predecessors
from tools.check_native_g5_readiness import GateFailure as ReadinessFailure, evaluate
from tools.check_native_g5_receipt import GateFailure as ReceiptAuditFailure, validate as validate_receipt
from tools.inject_native_g5_evidence import GateFailure as InjectionFailure, inject
from tools.produce_native_g5_receipt import GateFailure as ProducerFailure, build_receipt

ROOT = Path(__file__).resolve().parents[1]
COMMIT = "a" * 40
DIGESTS = ("b" * 64, "c" * 64, "d" * 64, "e" * 64)


def load(name):
    return json.loads((ROOT / "config" / name).read_text(encoding="utf-8"))


class ManifestTests(unittest.TestCase):
    def payloads(self):
        return [load(name) for name in (
            "native-g5-readiness-profile.json", "native-g5-inventory.json",
            "native-g5-authority-manifest.json", "native-g5-workload-manifest.json",
            "native-g5-suite-manifest.json", "native-g5-predecessor-manifest.json",
        )]

    def test_checked_in_authorities_are_complete_open_and_consistent(self):
        result = validate_manifests(ROOT, *self.payloads())
        self.assertEqual((result["status"], result["requirements"], result["predecessors"]), ("passed", 8, 3))
        self.assertFalse(result["closure_declared"])

    def test_missing_gap_and_unauthorized_command_fail_closed(self):
        payloads = self.payloads()
        payloads[1]["requirements"][0]["gaps"] = []
        with self.assertRaises(ManifestFailure):
            validate_manifests(ROOT, *payloads)
        payloads = self.payloads()
        payloads[4]["requirements"][0]["suites"][0]["command"][0] = "bash"
        with self.assertRaises(ManifestFailure):
            validate_manifests(ROOT, *payloads)

    def test_predecessor_substitution_fails_closed(self):
        payloads = self.payloads()
        payloads[5]["predecessors"][0]["sha256"] = "0" * 64
        with self.assertRaises(ManifestFailure):
            validate_manifests(ROOT, *payloads)


class ReceiptTests(unittest.TestCase):
    def fixture(self):
        suite = {"schema": "hyphae-native-g5-suite-manifest-v1", "gate": "G5", "requirements": [{"id": "all-engine-atomicity", "workloads": ["three-engine-commit"], "suites": [{"name": "real", "command": ["cargo", "test", "real"]}]}], "claims": [], "closure_declared": False}
        raw = json.dumps(suite).encode()
        log = b'G5_COMMAND: ["cargo","test","real"]\ntest result: ok. 2 passed; 0 failed; 0 ignored\n'
        return raw, log

    def test_producer_and_checker_bind_all_authorities(self):
        raw, log = self.fixture()
        suite_digest = hashlib.sha256(raw).hexdigest()
        receipt = build_receipt(COMMIT, "all-engine-atomicity", raw, suite_digest, *DIGESTS[1:], "linux", "1.96.0", [("real", log)])
        audit = validate_receipt(receipt, COMMIT, "all-engine-atomicity", suite_digest, *DIGESTS[1:])
        self.assertEqual((audit["test_count"], audit["evidence_class"]), (2, "supporting-not-closure"))

    def test_log_command_and_authority_substitution_fail_closed(self):
        raw, log = self.fixture()
        digest = hashlib.sha256(raw).hexdigest()
        with self.assertRaises(ProducerFailure):
            build_receipt(COMMIT, "all-engine-atomicity", raw, digest, *DIGESTS[1:], "linux", "1.96.0", [("other", log)])
        receipt = build_receipt(COMMIT, "all-engine-atomicity", raw, digest, *DIGESTS[1:], "linux", "1.96.0", [("real", log)])
        with self.assertRaises(ReceiptAuditFailure):
            validate_receipt(receipt, COMMIT, "all-engine-atomicity", digest, "0" * 64, *DIGESTS[2:])


class EvidenceTests(unittest.TestCase):
    def profile(self):
        return load("native-g5-readiness-profile.json")

    def baseline(self):
        return load("native-g5-readiness-evidence.json")

    def test_checked_in_baseline_is_open_zero_of_eight(self):
        result = evaluate(ROOT, self.profile(), self.baseline(), COMMIT, *DIGESTS)
        self.assertEqual((result["status"], result["passed"], result["required"]), ("not-ready", 0, 8))
        self.assertEqual(result["closure_status"], "open")

    def test_complete_supporting_evidence_never_declares_closure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            predecessor = {"schema": "hyphae-native-g5-predecessor-audit-v1", "gate": "G5", "status": "passed", "manifest_sha256": DIGESTS[3], "predecessors": [{"gate": gate} for gate in ("G2", "G3", "G4")], "claims": [], "closure_declared": False}
            pred_path = root / "predecessor.json"
            pred_path.write_text(json.dumps(predecessor))
            evidence = inject(root, self.baseline(), "predecessor", Path("predecessor.json"), COMMIT)
            for row in self.profile()["requirements"]:
                identifier = row["id"]
                payload = {"schema": "hyphae-native-g5-receipt-audit-v1", "gate": "G5", "status": "passed", "evidence_class": "supporting-not-closure", "source_commit": COMMIT, "requirement": identifier, "suite_manifest_sha256": DIGESTS[0], "authority_manifest_sha256": DIGESTS[1], "workload_manifest_sha256": DIGESTS[2], "predecessor_audit_sha256": DIGESTS[3], "workloads": [identifier], "suite_count": 1, "test_count": 1, "claims": [], "closure_declared": False}
                path = root / f"{identifier}.json"
                path.write_text(json.dumps(payload))
                evidence = inject(root, evidence, "requirement", Path(path.name), COMMIT, identifier)
            result = evaluate(root, self.profile(), evidence, COMMIT, *DIGESTS)
            self.assertEqual((result["status"], result["passed"]), ("candidate-evidence-complete", 8))
            self.assertEqual((result["closure_status"], result["closure_declared"], result["claims"]), ("open", False, []))

    def test_duplicate_injection_and_unknown_evidence_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "audit.json"
            path.write_text(json.dumps({"schema": "hyphae-native-g5-receipt-audit-v1", "source_commit": COMMIT, "requirement": "all-engine-atomicity", "claims": [], "closure_declared": False}))
            evidence = inject(root, self.baseline(), "requirement", Path("audit.json"), COMMIT, "all-engine-atomicity")
            with self.assertRaises(InjectionFailure):
                inject(root, evidence, "requirement", Path("audit.json"), COMMIT, "all-engine-atomicity")
        evidence = self.baseline()
        evidence["evidence"]["invented"] = {}
        with self.assertRaises(ReadinessFailure):
            evaluate(ROOT, self.profile(), evidence, COMMIT, *DIGESTS)


class PredecessorTests(unittest.TestCase):
    def test_checked_in_predecessors_audit_and_tamper_fails(self):
        manifest = load("native-g5-predecessor-manifest.json")
        result = audit_predecessors(ROOT, manifest, "f" * 64)
        self.assertEqual([row["gate"] for row in result["predecessors"]], ["G2", "G3", "G4"])
        changed = copy.deepcopy(manifest)
        changed["predecessors"][2]["source_commit"] = "0" * 40
        with self.assertRaises(PredecessorFailure):
            audit_predecessors(ROOT, changed, "f" * 64)


if __name__ == "__main__":
    unittest.main()
