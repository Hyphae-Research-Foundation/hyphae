#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g7_g8_readiness import (
    GateFailure,
    validate,
    validate_g7_execution_workflow,
)


ROOT = Path(__file__).resolve().parents[1]


def copy_authority(root: Path) -> None:
    (root / "config").mkdir()
    (root / ".github/workflows").mkdir(parents=True)
    for name in (
        "native-g7-readiness-profile.json",
        "native-g8-readiness-profile.json",
        "native-g8-suite-manifest.json",
    ):
        (root / "config" / name).write_bytes((ROOT / "config" / name).read_bytes())
    for name in ("native-g8-closure.yml", "native-g7-g8-readiness.yml"):
        (root / ".github/workflows" / name).write_bytes(
            (ROOT / ".github/workflows" / name).read_bytes()
        )


class NativeG7G8ReadinessTests(unittest.TestCase):
    def test_g7_numeric_threshold_authority_fails_closed_when_malformed(self) -> None:
        for field, replacement in (("p50", 0), ("p99", "10000")):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                copy_authority(root)
                path = root / "config/native-g7-readiness-profile.json"
                profile = json.loads(path.read_text(encoding="utf-8"))
                profile["warm_targets_nanoseconds"]["embedded-structure-point-get"][
                    field
                ] = replacement
                path.write_text(json.dumps(profile), encoding="utf-8")
                with self.assertRaisesRegex(GateFailure, "latency targets"):
                    validate(root, "a" * 40)

    def test_g7_authority_requires_the_exact_hot_warmup(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            copy_authority(root)
            path = root / "config/native-g7-readiness-profile.json"
            profile = json.loads(path.read_text(encoding="utf-8"))
            profile["required_hot_warmup"] = 99_999
            path.write_text(json.dumps(profile), encoding="utf-8")
            with self.assertRaisesRegex(GateFailure, "measurement authority"):
                validate(root, "a" * 40)

    def test_g8_sbom_authority_uses_the_exact_release_verifier(self) -> None:
        manifest = json.loads(
            (ROOT / "config/native-g8-suite-manifest.json").read_text()
        )
        row = next(
            requirement
            for requirement in manifest["requirements"]
            if requirement["id"] == "sbom-signatures-provenance"
        )
        self.assertEqual(
            row,
            {
                "id": "sbom-signatures-provenance",
                "status": "implemented-unhosted",
                "platforms": ["release"],
                "runner": "python packaging/g8_release_verification.py",
                "acceptance": [
                    "spdx",
                    "cyclonedx",
                    "manifest-license-authority",
                    "identity-completeness",
                    "checksums",
                    "cosign",
                    "provenance",
                ],
            },
        )

    def test_g8_sbom_authority_drift_fails_closed(self) -> None:
        for field, replacement in (
            ("runner", "python packaging/release_evidence.py verify"),
            ("acceptance", ["spdx", "cyclonedx"]),
        ):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "config").mkdir()
                (root / ".github/workflows").mkdir(parents=True)
                for name in (
                    "native-g7-readiness-profile.json",
                    "native-g8-readiness-profile.json",
                    "native-g8-suite-manifest.json",
                ):
                    (root / "config" / name).write_bytes(
                        (ROOT / "config" / name).read_bytes()
                    )
                path = root / "config/native-g8-suite-manifest.json"
                payload = json.loads(path.read_text())
                row = next(
                    requirement
                    for requirement in payload["requirements"]
                    if requirement["id"] == "sbom-signatures-provenance"
                )
                row[field] = replacement
                path.write_text(json.dumps(payload))
                (root / ".github/workflows/native-g8-closure.yml").write_bytes(
                    (ROOT / ".github/workflows/native-g8-closure.yml").read_bytes()
                )
                (root / ".github/workflows/native-g7-g8-readiness.yml").write_bytes(
                    (ROOT / ".github/workflows/native-g7-g8-readiness.yml").read_bytes()
                )
                with self.assertRaisesRegex(GateFailure, "SBOM authority drifted"):
                    validate(root, "a" * 40)

    def test_checked_in_authority_is_open_and_claim_free(self) -> None:
        result = validate(ROOT, "a" * 40)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["g7"]["status"], "open")
        self.assertEqual(result["g8"]["status"], "open")
        self.assertEqual(result["claims"], [])
        self.assertFalse(result["closure_declared"])

    def test_g8_closed_row_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config").mkdir()
            (root / ".github/workflows").mkdir(parents=True)
            for name in ("native-g7-readiness-profile.json", "native-g8-readiness-profile.json", "native-g8-suite-manifest.json"):
                source = ROOT / "config" / name
                (root / "config" / name).write_bytes(source.read_bytes())
            (root / ".github/workflows/native-g8-closure.yml").write_bytes(
                (ROOT / ".github/workflows/native-g8-closure.yml").read_bytes()
            )
            (root / ".github/workflows/native-g7-g8-readiness.yml").write_bytes(
                (ROOT / ".github/workflows/native-g7-g8-readiness.yml").read_bytes()
            )
            path = root / "config/native-g8-suite-manifest.json"
            payload = json.loads(path.read_text())
            payload["requirements"][0]["status"] = "passed"
            path.write_text(json.dumps(payload))
            with self.assertRaisesRegex(GateFailure, "not completely implemented"):
                validate(root, "a" * 40)

    def test_g8_closure_must_validate_exact_sha_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config").mkdir()
            (root / ".github/workflows").mkdir(parents=True)
            for name in (
                "native-g7-readiness-profile.json",
                "native-g8-readiness-profile.json",
                "native-g8-suite-manifest.json",
            ):
                (root / "config" / name).write_bytes((ROOT / "config" / name).read_bytes())
            workflow = (ROOT / ".github/workflows/native-g8-closure.yml").read_text()
            (root / ".github/workflows/native-g8-closure.yml").write_text(
                workflow.replace("check_native_g8_receipts.py", "unchecked-g8.py")
            )
            (root / ".github/workflows/native-g7-g8-readiness.yml").write_bytes(
                (ROOT / ".github/workflows/native-g7-g8-readiness.yml").read_bytes()
            )
            with self.assertRaisesRegex(GateFailure, "exact-SHA receipts"):
                validate(root, "a" * 40)

    def test_g8_closure_must_not_depend_on_g7(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config").mkdir()
            (root / ".github/workflows").mkdir(parents=True)
            for name in (
                "native-g7-readiness-profile.json",
                "native-g8-readiness-profile.json",
                "native-g8-suite-manifest.json",
            ):
                (root / "config" / name).write_bytes((ROOT / "config" / name).read_bytes())
            workflow = (ROOT / ".github/workflows/native-g8-closure.yml").read_text()
            (root / ".github/workflows/native-g8-closure.yml").write_text(
                workflow + "\n# check_native_g7_matrix.py\n"
            )
            (root / ".github/workflows/native-g7-g8-readiness.yml").write_bytes(
                (ROOT / ".github/workflows/native-g7-g8-readiness.yml").read_bytes()
            )
            with self.assertRaisesRegex(GateFailure, "independent from G7"):
                validate(root, "a" * 40)

    def test_dedicated_g7_execution_requires_hosted_exact_sha_qualification(self) -> None:
        workflow = (ROOT / ".github/workflows/native-g7-g8-readiness.yml").read_text()
        validate_g7_execution_workflow(workflow)
        weakened = workflow.replace(
            "needs: [authority, g7_qualification]",
            "needs: [authority]",
            1,
        )
        with self.assertRaisesRegex(GateFailure, "gated by successful qualification"):
            validate_g7_execution_workflow(weakened)

    def test_g7_workflow_cannot_provision_infrastructure(self) -> None:
        workflow = (ROOT / ".github/workflows/native-g7-g8-readiness.yml").read_text()
        with self.assertRaisesRegex(GateFailure, "must not provision"):
            validate_g7_execution_workflow(workflow + "\n# aws ec2 run-instances\n")


if __name__ == "__main__":
    unittest.main()
