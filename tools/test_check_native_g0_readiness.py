from __future__ import annotations

import json
import unittest
from pathlib import Path

from tools.check_native_g0_readiness import (
    GateFailure,
    evaluate_readiness,
    inject_evidence,
    validate_passed_artifacts,
)


ROOT = Path(__file__).resolve().parents[1]


class NativeG0ReadinessTests(unittest.TestCase):
    def test_missing_required_evidence_keeps_g0_not_configured(self) -> None:
        profile = {
            "schema": "hyphae-native-g0-profile-v1",
            "gate": "G0",
            "requirements": [
                {
                    "id": "canonical-types",
                    "required_evidence_level": "local-integration",
                },
                {
                    "id": "dependency-audit",
                    "required_evidence_level": "hosted",
                },
            ],
        }

        result = evaluate_readiness(profile, {})

        self.assertEqual(result["status"], "not-configured")
        self.assertEqual(result["passed"], 0)
        self.assertEqual(result["required"], 2)
        self.assertEqual(
            result["requirements"],
            [
                {
                    "id": "canonical-types",
                    "required_evidence_level": "local-integration",
                    "status": "not-configured",
                    "artifact": None,
                },
                {
                    "id": "dependency-audit",
                    "required_evidence_level": "hosted",
                    "status": "not-configured",
                    "artifact": None,
                },
            ],
        )

    def test_inject_evidence_validates_passed_json_and_binds_exact_bytes(self) -> None:
        import hashlib
        import tempfile

        profile = {
            "schema": "hyphae-native-g0-profile-v1",
            "gate": "G0",
            "requirements": [
                {
                    "id": "canonical-type-goldens-and-properties",
                    "required_evidence_level": "hosted",
                }
            ],
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "types.json"
            artifact.write_text('{"status":"passed"}\n', encoding="utf-8")
            evidence = inject_evidence(
                root,
                {},
                profile,
                "canonical-type-goldens-and-properties",
                "hosted",
                "types.json",
            )
            record = evidence["canonical-type-goldens-and-properties"]
            self.assertEqual(record["status"], "passed")
            self.assertEqual(record["evidence_level"], "hosted")
            self.assertEqual(
                record["artifact_sha256"],
                hashlib.sha256(artifact.read_bytes()).hexdigest(),
            )
            artifact.write_text('{"status":"failed"}\n', encoding="utf-8")
            with self.assertRaisesRegex(GateFailure, "does not report passed"):
                inject_evidence(
                    root,
                    {},
                    profile,
                    "canonical-type-goldens-and-properties",
                    "hosted",
                    "types.json",
                )

    def test_types_hosted_workflow_runs_complete_cross_crate_corpus(self) -> None:
        workflow = (ROOT / ".github/workflows/security.yml").read_text(encoding="utf-8")
        self.assertIn("Audit canonical native types", workflow)
        self.assertIn("canonical_value_golden_fixtures", workflow)
        self.assertIn("-p hyphae-native-types", workflow)
        self.assertIn("-p hyphae-native-records", workflow)
        self.assertIn("-p hyphae-native-pages", workflow)
        self.assertIn("-p hyphae-native-wal", workflow)
        self.assertIn("-p hyphae-native-runtime", workflow)
        self.assertIn("native-types-audit.json", workflow)
        self.assertIn("native-contract-conformance.json", workflow)
        self.assertIn("--inject-requirement sql-structure-search-ann-contracts", workflow)
        self.assertIn("native-g0-evidence-with-contracts.json", workflow)
        self.assertIn("--inject-requirement page-row-blob-wal-mvcc-goldens", workflow)
        self.assertIn("native-g0-evidence-with-goldens.json", workflow)
        self.assertIn("--inject-requirement benchmark-and-quality-corpus", workflow)
        self.assertIn("native-quality-aggregate.json", workflow)
        self.assertIn("native-g0-evidence-with-quality.json", workflow)
        conformance_workflow = (
            ROOT / ".github/workflows/native-conformance.yml"
        ).read_text(encoding="utf-8")
        self.assertIn(
            "--inject-requirement local-protocol-goldens-and-conformance",
            conformance_workflow,
        )
        self.assertIn("native-g0-evidence-with-conformance.json", conformance_workflow)
        self.assertIn(
            "--inject-requirement native-dependency-license-unsafe-audit",
            workflow,
        )
        self.assertIn("native-g0-evidence-with-dependencies.json", workflow)
        self.assertIn("Generate clean-room evidence when externally approved", workflow)
        self.assertIn("human-review-required", workflow)
        self.assertIn("--inject-requirement clean-room-porting-ledger-review", workflow)
        self.assertIn("native-clean-room-receipt.json", workflow)
        self.assertIn("Audit accepted native specification set", workflow)
        self.assertIn(
            "--inject-requirement architecture-and-versioned-specifications",
            workflow,
        )
        self.assertIn("native-specification-receipt.json", workflow)

    def test_final_workflow_assembles_exact_eight_row_readiness(self) -> None:
        workflow = (ROOT / ".github/workflows/native-g0-final.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("head_sha: sha", workflow)
        self.assertIn("Verify receipt commit identity", workflow)
        self.assertIn("source_commit {actual} != {expected}", workflow)
        self.assertIn("tools/assemble_native_g0_evidence.py", workflow)
        self.assertIn("native-g0-final-readiness.json", workflow)
        self.assertIn("'required': 8", workflow)
        self.assertIn("'passed': 8", workflow)

    def test_all_exact_evidence_must_pass_at_the_required_level(self) -> None:
        profile = {
            "schema": "hyphae-native-g0-profile-v1",
            "gate": "G0",
            "requirements": [
                {"id": "types", "required_evidence_level": "local-integration"},
                {"id": "dependencies", "required_evidence_level": "hosted"},
            ],
        }
        evidence = {
            "types": {
                "status": "passed",
                "evidence_level": "local-integration",
                "artifact": "evidence/types.json",
                "artifact_sha256": "a" * 64,
            },
            "dependencies": {
                "status": "passed",
                "evidence_level": "hosted",
                "artifact": "evidence/dependencies.json",
                "artifact_sha256": "b" * 64,
            },
        }

        result = evaluate_readiness(profile, evidence)

        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["passed"], 2)

    def test_lower_scope_or_failed_evidence_cannot_close_g0(self) -> None:
        profile = {
            "schema": "hyphae-native-g0-profile-v1",
            "gate": "G0",
            "requirements": [
                {"id": "types", "required_evidence_level": "hosted"},
                {"id": "corpus", "required_evidence_level": "production"},
            ],
        }
        evidence = {
            "types": {
                "status": "passed",
                "evidence_level": "local-integration",
                "artifact": "evidence/types.json",
                "artifact_sha256": "a" * 64,
            },
            "corpus": {
                "status": "failed",
                "evidence_level": "production",
                "artifact": "evidence/corpus.json",
            },
        }

        result = evaluate_readiness(profile, evidence)

        self.assertEqual(result["status"], "failed")
        self.assertEqual(
            [row["status"] for row in result["requirements"]],
            ["insufficient-evidence", "failed"],
        )

    def test_passed_evidence_requires_a_canonical_sha256_binding(self) -> None:
        profile = {
            "schema": "hyphae-native-g0-profile-v1",
            "gate": "G0",
            "requirements": [
                {"id": "types", "required_evidence_level": "hosted"}
            ],
        }
        with self.assertRaisesRegex(GateFailure, "SHA-256"):
            evaluate_readiness(
                profile,
                {
                    "types": {
                        "status": "passed",
                        "evidence_level": "hosted",
                        "artifact": "evidence/types.json",
                    }
                },
            )
        with self.assertRaisesRegex(GateFailure, "SHA-256"):
            evaluate_readiness(
                profile,
                {
                    "types": {
                        "status": "passed",
                        "evidence_level": "hosted",
                        "artifact": "evidence/types.json",
                        "artifact_sha256": "not-a-digest",
                    }
                },
            )

        result = evaluate_readiness(
            profile,
            {
                "types": {
                    "status": "passed",
                    "evidence_level": "hosted",
                    "artifact": "evidence/types.json",
                    "artifact_sha256": "a" * 64,
                }
            },
        )
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["requirements"][0]["artifact_sha256"], "a" * 64)

    def test_passed_artifact_bytes_must_match_the_bound_digest(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "evidence.json"
            artifact.write_bytes(b"canonical")
            result = {
                "requirements": [
                    {
                        "id": "types",
                        "status": "passed",
                        "artifact": "evidence.json",
                        "artifact_sha256": (
                            "a3a7b7569607cbd40b16e3717c44068e174f1f5442923f01d09a30c4e8d8f9e6"
                        ),
                    }
                ]
            }
            with self.assertRaisesRegex(GateFailure, "SHA-256 mismatch"):
                validate_passed_artifacts(root, result)

            result["requirements"][0]["artifact_sha256"] = __import__(
                "hashlib"
            ).sha256(b"canonical").hexdigest()
            validate_passed_artifacts(root, result)

            result["requirements"][0]["artifact"] = "../outside.json"
            with self.assertRaisesRegex(GateFailure, "escapes repository root"):
                validate_passed_artifacts(root, result)

    def test_profile_and_evidence_drift_fail_closed(self) -> None:
        with self.assertRaisesRegex(GateFailure, "duplicate requirement"):
            evaluate_readiness(
                {
                    "schema": "hyphae-native-g0-profile-v1",
                    "gate": "G0",
                    "requirements": [
                        {"id": "types", "required_evidence_level": "hosted"},
                        {"id": "types", "required_evidence_level": "hosted"},
                    ],
                },
                {},
            )

        with self.assertRaisesRegex(GateFailure, "unknown evidence"):
            evaluate_readiness(
                {
                    "schema": "hyphae-native-g0-profile-v1",
                    "gate": "G0",
                    "requirements": [
                        {"id": "types", "required_evidence_level": "hosted"}
                    ],
                },
                {
                    "unexpected": {
                        "status": "passed",
                        "evidence_level": "hosted",
                        "artifact": "evidence/unexpected.json",
                    }
                },
            )
    def test_checked_in_profile_is_complete_and_current_evidence_stays_open(self) -> None:
        profile = json.loads(
            (ROOT / "config/native-g0-readiness-profile.json").read_text(encoding="utf-8")
        )
        evidence = json.loads(
            (ROOT / "config/native-g0-readiness-evidence.json").read_text(encoding="utf-8")
        )

        self.assertEqual(
            [entry["id"] for entry in profile["requirements"]],
            [
                "architecture-and-versioned-specifications",
                "canonical-type-goldens-and-properties",
                "page-row-blob-wal-mvcc-goldens",
                "sql-structure-search-ann-contracts",
                "local-protocol-goldens-and-conformance",
                "benchmark-and-quality-corpus",
                "native-dependency-license-unsafe-audit",
                "clean-room-porting-ledger-review",
            ],
        )
        result = evaluate_readiness(profile, evidence)
        self.assertEqual(result["status"], "failed")
        self.assertLess(result["passed"], result["required"])
        architecture = result["requirements"][0]
        self.assertEqual(
            architecture["artifact_sha256"],
            "61550f71005a1a880f929bb309c3f26cea6259e76928342ed8f71930b56705c6",
        )

        security_workflow = (ROOT / ".github/workflows/security.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("Assess native G0 readiness", security_workflow)
        self.assertIn("tools/test_check_native_g0_readiness.py", security_workflow)
        self.assertIn("config/native-g0-readiness-profile.json", security_workflow)


if __name__ == "__main__":
    unittest.main()
