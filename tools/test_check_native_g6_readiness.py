#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import copy
import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g6_foundation import GateFailure, PLATFORMS, PREDECESSORS, REQUIREMENTS, SDKS, TRANSPORTS
from tools.check_native_g6_readiness import evaluate
from tools.inject_native_g6_evidence import inject
from tools.produce_native_g6_receipt import _canonical_sha256, _coverage
from tools.test_native_g6_evidence_support import COMMIT, checked_raw, digests, payloads


class NativeG6ReadinessTests(unittest.TestCase):
    def inputs(self):
        documents = payloads(checked_raw())
        return documents["profile"], documents["evidence"], documents["suite"], digests(checked_raw())

    @staticmethod
    def write(root: Path, name: str, payload: dict) -> Path:
        path = root / name
        path.write_text(json.dumps(payload), encoding="utf-8")
        return path

    def manifest_audit(self, manifest_sha256: dict[str, str]) -> dict:
        return {
            "schema": "hyphae-native-g6-manifest-audit-v1", "gate": "G6", "status": "passed",
            "evidence_class": "authority-not-closure", "source_commit": COMMIT,
            "manifest_sha256": manifest_sha256, "requirements": 14, "implemented_requirements": 14,
            "partial_requirements": 0, "planned_requirements": 0,
            "predecessors": [{"gate": gate, "source_commit": "b" * 40, "artifact_sha256": "c" * 64} for gate in PREDECESSORS],
            "predecessor_count": 6, "closure_status": "open", "claims": [], "closure_declared": False,
        }

    def receipt_audit(self, requirement: str, platform: str, manifest_sha256: dict[str, str], suite_manifest: dict) -> dict:
        suite_row = next(row for row in suite_manifest["requirements"] if row["id"] == requirement)
        suites = [item for item in suite_row["suites"] if platform in item.get("platforms", PLATFORMS)]
        results = []
        tools = {}
        for item in suites:
            command = item.get("platform_commands", {}).get(platform, item["command"])
            tools[command[0]] = "version"
            results.append({
                "name": item["name"], "command": command, "command_sha256": _canonical_sha256(command),
                "status": "passed", "exit_code": 0, "test_count": 1, "log_sha256": "2" * 64,
            })
        sdks, transports = _coverage(suites)
        return {
            "schema": "hyphae-native-g6-receipt-audit-v1", "gate": "G6", "status": "passed",
            "evidence_class": "supporting-not-closure", "source_commit": COMMIT, "requirement": requirement,
            "manifest_sha256": manifest_sha256,
            "authority": {"scope": "scope", "evidence_class": "authority", "identity_sha256": "d" * 64},
            "workload": {"id": requirement, "oracle": "oracle", "acceptance": sorted(self.acceptance(requirement)), "identity_sha256": "e" * 64},
            "suite_identity_sha256": _canonical_sha256(suite_row), "platform": platform,
            "tool_versions": tools, "sdks": sdks, "transports": transports,
            "command_results": results, "test_count": len(results), "suite_count": len(results),
            "implementation_status": suite_row["status"], "uncovered_acceptance": suite_row["uncovered_acceptance"],
            "claims": [], "closure_declared": False,
        }

    @staticmethod
    def acceptance(requirement: str) -> set[str]:
        from tools.check_native_g6_foundation import WORKLOAD_ACCEPTANCE
        return WORKLOAD_ACCEPTANCE[requirement]

    def complete_evidence(self, root: Path, evidence: dict, manifest_sha256: dict[str, str], suite_manifest: dict) -> dict:
        predecessor = self.write(root, "predecessor.json", self.manifest_audit(manifest_sha256))
        evidence = inject(root, evidence, "predecessor", Path(predecessor.name), COMMIT, manifest_sha256)
        for requirement in REQUIREMENTS:
            for platform in PLATFORMS:
                payload = self.receipt_audit(requirement, platform, manifest_sha256, suite_manifest)
                path = self.write(root, f"{requirement}--{platform}.json", payload)
                evidence = inject(root, evidence, "requirement", Path(path.name), COMMIT, manifest_sha256, requirement, platform)
        return evidence

    def test_checked_in_baseline_is_open_zero_of_fourteen(self) -> None:
        profile, evidence, suite, manifest_sha256 = self.inputs()
        result = evaluate(Path(__file__).resolve().parents[1], profile, evidence, COMMIT, manifest_sha256, suite)
        self.assertEqual((result["status"], result["passed"], result["matrix_cells_passed"]), ("not-ready", 0, 0))

    def test_full_fourteen_by_three_matrix_forms_open_candidate_when_implemented(self) -> None:
        profile, evidence, suite, manifest_sha256 = self.inputs()
        implemented = copy.deepcopy(suite)
        for row in implemented["requirements"]:
            row["status"] = "implemented-unhosted"
            row["uncovered_acceptance"] = []
            covered = {value for item in row["suites"] for value in item["acceptance"]}
            missing = self.acceptance(row["id"]) - covered
            row["suites"][0]["acceptance"].extend(sorted(missing))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = self.complete_evidence(root, evidence, manifest_sha256, implemented)
            result = evaluate(root, profile, evidence, COMMIT, manifest_sha256, implemented)
        self.assertEqual((result["status"], result["passed"], result["matrix_cells_passed"]), ("candidate-evidence-complete", 14, 42))
        self.assertEqual(result["platforms_passed"], PLATFORMS)
        self.assertEqual(result["sdks_passed"], SDKS)
        self.assertEqual(result["transports_passed"], TRANSPORTS)

    def test_missing_one_matrix_cell_fails_closed(self) -> None:
        profile, evidence, suite, manifest_sha256 = self.inputs()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = self.complete_evidence(root, evidence, manifest_sha256, suite)
            del evidence["evidence"][REQUIREMENTS[0]]["windows"]
            with self.assertRaisesRegex(GateFailure, "incomplete G6 platform matrix"):
                evaluate(root, profile, evidence, COMMIT, manifest_sha256, suite)

    def test_duplicate_platform_is_rejected_during_injection(self) -> None:
        _, evidence, suite, manifest_sha256 = self.inputs()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = self.receipt_audit(REQUIREMENTS[0], "linux", manifest_sha256, suite)
            path = self.write(root, "linux.json", payload)
            evidence = inject(root, evidence, "requirement", Path(path.name), COMMIT, manifest_sha256, REQUIREMENTS[0], "linux")
            with self.assertRaisesRegex(GateFailure, "duplicate"):
                inject(root, evidence, "requirement", Path(path.name), COMMIT, manifest_sha256, REQUIREMENTS[0], "linux")

    def test_mismatched_coverage_fails_closed(self) -> None:
        profile, evidence, suite, manifest_sha256 = self.inputs()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = self.complete_evidence(root, evidence, manifest_sha256, suite)
            cell = evidence["evidence"]["rust-python-typescript-sdks"]["linux"]
            path = root / cell["reference"]
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["sdks"] = ["rust"]
            path.write_text(json.dumps(payload), encoding="utf-8")
            import hashlib
            cell["artifact_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
            with self.assertRaisesRegex(GateFailure, "hosted audit"):
                evaluate(root, profile, evidence, COMMIT, manifest_sha256, suite)

    def test_zero_test_spoof_fails_readiness(self) -> None:
        profile, evidence, suite, manifest_sha256 = self.inputs()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = self.complete_evidence(root, evidence, manifest_sha256, suite)
            cell = evidence["evidence"][REQUIREMENTS[0]]["linux"]
            path = root / cell["reference"]
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["command_results"][0]["test_count"] = 0
            payload["test_count"] -= 1
            path.write_text(json.dumps(payload), encoding="utf-8")
            import hashlib
            cell["artifact_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
            with self.assertRaisesRegex(GateFailure, "command audit"):
                evaluate(root, profile, evidence, COMMIT, manifest_sha256, suite)


if __name__ == "__main__":
    unittest.main()
