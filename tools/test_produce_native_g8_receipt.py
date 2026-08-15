#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

import json
import tempfile
import unittest
from pathlib import Path

from tools.check_native_g8_receipts import GateFailure, authority, validate_receipt
from tools.produce_native_g8_receipt import (
    CHECKPOINT_BOUNDARIES,
    COMMIT_BOUNDARIES,
    PROMOTION_BOUNDARIES,
    POWER_LOSS_COMMIT,
    SNAPSHOT_PIN_BOUNDARIES,
    produce,
)
from tools.run_native_g8_test_gate import SUITES


ROOT = Path(__file__).resolve().parents[1]
COMMIT = "a" * 40


def write(directory: str, payload: dict) -> Path:
    path = Path(directory) / "artifact.json"
    path.write_text(json.dumps(payload), encoding="utf-8")
    return path


def soak() -> dict:
    return {
        "schema": "hyphae-native-g8-soak-v2",
        "status": "passed",
        "source_commit": COMMIT,
        "source_tree": "b" * 40,
        "platform": "linux",
        "cycles": 4,
        "writes_per_cycle": 32,
        "records": 128,
        "forced_daemon_terminations": 4,
        "engines": ["sql", "structures", "search"],
        "restore_state_equivalent": True,
        "doctor_after_restore": "healthy",
        "backup_manifest_sha256": "1" * 64,
        "binary_sha256": "2" * 64,
        "independent_verifier_sha256": "3" * 64,
        "state_digest_sha256": "4" * 64,
        "state_root_digest": "6" * 64,
        "state_equivalence_method": "all-engine-root-complete-sql-semantic-samples",
        "semantic_sample_records": 3,
        "independent_verification": {
            "schema": "hyphae-independent-backup-verification-v1",
            "status": "passed",
            "verifier": "independent-envelope-v1",
            "file_count": 3,
            "directory_count": 1,
            "total_bytes": 1024,
            "visible_csn": 1,
            "checkpoint_digest": "5" * 64,
        },
    }


def signed_release() -> dict:
    version = "1.1.0"
    return {
        "schema": "hyphae-native-g8-signed-release-v1",
        "status": "passed",
        "source_commit": COMMIT,
        "tag": f"v{version}",
        "archive_count": 4,
        "signature_verifications": 12,
        "attestation_verifications": 12,
        "software_license": "AGPL-3.0-only",
        "license_authority": "tracked-package-manifests-and-local-locks-v1",
        "first_party_artifact_count": 79,
        "first_party_identity_count": 33,
        "spdx_hyphae_components": ["hyphae-native-runtime"],
        "cyclonedx_hyphae_components": ["hyphae-native-runtime"],
        "spdx_sha256": "1" * 64,
        "cyclonedx_sha256": "2" * 64,
        "checksums_sha256": "3" * 64,
        "release_evidence_sha256": "4" * 64,
        "provenance_targets": [
            f"hyphae-{version}-aarch64-apple-darwin.tar.gz",
            f"hyphae-{version}-x86_64-apple-darwin.tar.gz",
            f"hyphae-{version}-x86_64-pc-windows-msvc.zip",
            f"hyphae-{version}-x86_64-unknown-linux-gnu.tar.gz",
        ],
    }


class G8ProducerTests(unittest.TestCase):
    def assert_valid(self, requirement: str, payload: dict, platform: str = "linux") -> dict:
        with tempfile.TemporaryDirectory() as directory:
            receipt = produce(ROOT, requirement, platform, COMMIT, write(directory, payload))
        _, rows = authority(ROOT)
        self.assertEqual(validate_receipt(receipt, COMMIT, rows[requirement])["status"], "passed")
        return receipt

    def test_soak_proves_native_and_independent_restore_requirements(self) -> None:
        self.assert_valid("native-soak", soak())
        self.assert_valid("independent-restore-verification", soak())

    def test_soak_rejects_a_token_kill_restart_run(self) -> None:
        payload = soak()
        payload.update({"cycles": 1, "writes_per_cycle": 1, "records": 1})
        payload["forced_daemon_terminations"] = 1
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(GateFailure):
            produce(ROOT, "native-soak", "linux", COMMIT, write(directory, payload))

    def test_process_crash_requires_every_sigkill_boundary(self) -> None:
        payload = {
            "schema": "hyphae.native.process-crash-matrix.v4",
            "status": "process-crash-not-power-loss",
            "source_commit": COMMIT,
            "environment": "g8-github-actions-ubuntu-24.04",
            "target": "x86_64-linux",
            "durability": "strict",
            "all_engine_csn": 1,
            "commit_boundaries": [
                {
                    "boundary": name,
                    "termination": "signal-9",
                    "expected_state": (
                        "complete-csn-1"
                        if name in {"wal-appended", "wal-synchronized", "root-published"}
                        else "prior-empty"
                    ),
                    "recovered_csn": (
                        1
                        if name in {"wal-appended", "wal-synchronized", "root-published"}
                        else None
                    ),
                    "recovered_blob_count": 0 if name == "blob-staged" else 1,
                }
                for name in sorted(COMMIT_BOUNDARIES)
            ],
            "checkpoint_boundaries": [
                {
                    "boundary": name,
                    "termination": "signal-9",
                    "manifest_count": 0 if name == "manifest-staged" else 1,
                    "checkpoint_count": int(name.startswith("wal-")),
                    "unanchored_manifest_suffix": int(name == "manifest-published"),
                    "recovered_temporary_manifests": int(name == "manifest-staged"),
                }
                for name in sorted(CHECKPOINT_BOUNDARIES)
            ],
            "snapshot_pin_boundaries": [
                {
                    "boundary": name,
                    "termination": "signal-9",
                    "expected_pin": "complete" if name == "record-published" else "absent",
                    "recovered_pin_count": int(name == "record-published"),
                    "pin_directory_files": int(name == "record-published"),
                    "retained_page_generations": 1,
                }
                for name in sorted(SNAPSHOT_PIN_BOUNDARIES)
            ],
            "promotion_boundaries": [
                {
                    "boundary": name,
                    "termination": "signal-9",
                    "expected_marker": (
                        "pending" if name == "before-rename" else "authority"
                    ),
                }
                for name in sorted(PROMOTION_BOUNDARIES)
            ],
        }
        self.assert_valid("process-crash-recovery", payload)
        staged = next(
            row
            for row in payload["checkpoint_boundaries"]
            if row["boundary"] == "manifest-staged"
        )
        staged["recovered_temporary_manifests"] = 0
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(GateFailure):
            produce(ROOT, "process-crash-recovery", "linux", COMMIT, write(directory, payload))
        staged["recovered_temporary_manifests"] = 1
        payload["commit_boundaries"][0]["termination"] = "exit-code-1"
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(GateFailure):
            produce(ROOT, "process-crash-recovery", "linux", COMMIT, write(directory, payload))

    def test_power_loss_and_resource_artifacts_are_coverage_checked(self) -> None:
        observations = [
            {"family": family, "boundary": name}
            for family, names in (
                ("commit", COMMIT_BOUNDARIES),
                ("checkpoint", CHECKPOINT_BOUNDARIES),
                ("promotion", PROMOTION_BOUNDARIES),
            )
            for name in sorted(names)
        ]
        for row in observations:
            row.update({
                "termination": "signal-9",
                "cleanup": "complete",
                "ext4_post_recovery_check": "e2fsck-fn-clean",
                "interruption_mark": f"interrupt-{row['family']}-{row['boundary']}",
                "logged_entries_at_interruption": 1,
                "highest_allocated_sector": 1,
                "live_mount_options": "rw,relatime",
                "replay_mount_options": "rw,relatime",
            })
            if row["family"] == "promotion":
                row["recovered_marker"] = (
                    "pending" if row["boundary"] == "before-rename" else "authority"
                )
        self.assert_valid("power-loss-replay", {
            "schema": "hyphae.native.block-power-loss-replay.v1",
            "status": "block-replay-not-physical-device-cut",
            "source_commit": COMMIT,
            "replay_tool_commit": POWER_LOSS_COMMIT,
            "filesystem": "ext4",
            "source_tree": "b" * 40,
            "device_mapper_target": "log-writes v1.0.0",
            "image_bytes": 128 * 1024 * 1024,
            "log_bytes": 512 * 1024 * 1024,
            "all_engine_csn": 1,
            "scenario_count": 14,
            "cleanup": "complete",
            "observations": observations,
        })
        self.assert_valid("resource-exhaustion", {
            "schema": "hyphae-native-resource-exhaustion-v1",
            "status": "passed",
            "source_commit": COMMIT,
            "source_tree": "b" * 40,
            "environment": "self-hosted-g8",
            "platform": "x86_64-linux",
            "isolation": "owned-loopback-ext4",
            "image_bytes": 128 * 1024 * 1024,
            "resource_limits": {
                "filesystem_free_bytes": 32 * 1024,
                "address_space_bytes": 16 * 1024 * 1024,
                "open_files": 8,
                "bounded_input_key_bytes": 70_000,
            },
            "post_failure_doctor": "healthy",
            "cleanup": "complete",
            "observations": {
                name: {"error": "observed"}
                for name in ("disk-full", "read-only", "memory", "descriptors", "bounded-input")
            },
        })

    def test_unsupported_generic_requirement_cannot_be_fabricated(self) -> None:
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(GateFailure):
            produce(ROOT, "sbom-signatures-provenance", "release", COMMIT, write(directory, {}))

    def test_fixed_corruption_and_migration_suites_bind_named_checks(self) -> None:
        for requirement, names in (
            ("corruption-matrix", (
                "pages-wal-manifest-blobs", "indexes", "proof-envelope-payload", "proof-truncation",
            )),
            ("format2-to-native-migration", (
                "equivalence-and-promotion", "overlap-and-rollback",
            )),
        ):
            with self.subTest(requirement=requirement):
                self.assert_valid(requirement, {
                    "schema": "hyphae-native-g8-fixed-suite-v1",
                    "status": "passed",
                    "source_commit": COMMIT,
                    "requirement": requirement,
                    "platform": "linux",
                    "source_tree": "b" * 40,
                    "checks": [{
                        "name": name,
                        "command": list(SUITES[requirement][index][1:]),
                        "status": "passed",
                        "exit_code": 0,
                        "duration_millis": 1,
                        "stdout_sha256": "c" * 64,
                        "stderr_sha256": "d" * 64,
                    } for index, name in enumerate(names)],
                })

    def test_installed_package_requires_all_native_engines(self) -> None:
        target = "x86_64-unknown-linux-gnu"
        self.assert_valid("multiplatform-packaging", {
            "schema": "hyphae-native-installed-package-v1",
            "status": "ok",
            "source_commit": COMMIT,
            "source_tree": "b" * 40,
            "platform": target,
            "installed_smoke": "passed",
            "native_engines": ["sql", "structures", "search"],
            "engine_version": "1.1.0",
            "archive": f"hyphae-1.1.0-{target}.tar.gz",
            "proofs_verified": 4,
            "archive_sha256": "c" * 64,
        }, platform=target)

    def test_signed_release_requires_all_four_provenance_targets(self) -> None:
        self.assert_valid(
            "sbom-signatures-provenance", signed_release(), platform="release"
        )

    def test_signed_release_requires_semantically_verified_agpl_sboms(self) -> None:
        for field, value in (
            ("software_license", "GPL-3.0-only"),
            ("license_authority", "untrusted"),
            ("first_party_artifact_count", 78),
            ("first_party_identity_count", 32),
            ("spdx_hyphae_components", []),
            ("cyclonedx_hyphae_components", []),
            ("spdx_hyphae_components", ["third-party-runtime"]),
        ):
            with self.subTest(field=field):
                payload = signed_release()
                payload[field] = value
                with tempfile.TemporaryDirectory() as directory:
                    with self.assertRaises(GateFailure):
                        produce(
                            ROOT,
                            "sbom-signatures-provenance",
                            "release",
                            COMMIT,
                            write(directory, payload),
                        )

    def test_signed_release_rejects_noncanonical_target_names(self) -> None:
        payload = signed_release()
        payload["provenance_targets"] = ["a", "b", "c", "d"]
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(GateFailure):
                produce(
                    ROOT, "sbom-signatures-provenance", "release", COMMIT,
                    write(directory, payload),
                )


if __name__ == "__main__":
    unittest.main()
