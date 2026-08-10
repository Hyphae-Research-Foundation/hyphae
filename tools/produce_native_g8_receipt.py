#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Validate one suite-specific G8 artifact and emit a closure-candidate receipt."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import tomllib
from pathlib import Path
from typing import Any

from tools.check_native_g8_receipts import GateFailure, authority
from tools.run_native_g8_test_gate import SUITES


HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
RELEASE_TAG = re.compile(r"v([0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?)\Z")
POWER_LOSS_COMMIT = "7b70d8a6863c5de30933d42a7672d35d01d2dc6c"
ROOT = Path(__file__).resolve().parents[1]
COMMIT_BOUNDARIES = {
    "blob-staged", "blob-promoted", "page-appended", "page-synchronized",
    "wal-appended", "wal-synchronized", "root-published",
}
CHECKPOINT_BOUNDARIES = {
    "manifest-staged", "manifest-published", "wal-appended", "wal-synchronized",
}
SNAPSHOT_PIN_BOUNDARIES = {"record-synchronized", "record-published"}
PROMOTION_BOUNDARIES = {"before-rename", "marker-renamed", "parent-synchronized"}


def require_object(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def require_exact_names(values: object, expected: set[str], label: str) -> list[dict[str, Any]]:
    if not isinstance(values, list) or any(not isinstance(value, dict) for value in values):
        raise GateFailure(f"{label} must be an object list")
    if {value.get("boundary") for value in values} != expected or len(values) != len(expected):
        raise GateFailure(f"{label} boundary coverage differs")
    return values


def validate_soak(payload: dict[str, Any], commit: str, platform: str) -> dict[str, str]:
    if (
        payload.get("schema") != "hyphae-native-g8-soak-v2"
        or payload.get("status") != "passed"
        or payload.get("source_commit") != commit
        or payload.get("platform") != platform
        or not isinstance(payload.get("cycles"), int)
        or payload["cycles"] < 4
        or not isinstance(payload.get("writes_per_cycle"), int)
        or payload["writes_per_cycle"] < 32
        or not isinstance(payload.get("records"), int)
        or payload["records"] != payload["cycles"] * payload["writes_per_cycle"]
        or payload.get("forced_daemon_terminations") != payload["cycles"]
        or payload.get("engines") != ["sql", "structures", "search"]
        or payload.get("restore_state_equivalent") is not True
        or payload.get("doctor_after_restore") != "healthy"
        or HEX40.fullmatch(str(payload.get("source_tree", ""))) is None
        or HEX64.fullmatch(str(payload.get("backup_manifest_sha256", ""))) is None
        or HEX64.fullmatch(str(payload.get("binary_sha256", ""))) is None
        or HEX64.fullmatch(str(payload.get("independent_verifier_sha256", ""))) is None
        or HEX64.fullmatch(str(payload.get("state_digest_sha256", ""))) is None
        or HEX64.fullmatch(str(payload.get("state_root_digest", ""))) is None
        or payload.get("state_equivalence_method")
        != "all-engine-root-complete-sql-semantic-samples"
        or payload.get("semantic_sample_records") != 3
    ):
        raise GateFailure("Native G8 soak artifact is incomplete")
    independent = require_object(payload.get("independent_verification"), "independent verification")
    if (
        independent.get("schema") != "hyphae-independent-backup-verification-v1"
        or independent.get("status") != "passed"
        or independent.get("verifier") != "independent-envelope-v1"
        or not isinstance(independent.get("file_count"), int)
        or independent["file_count"] <= 0
        or not isinstance(independent.get("directory_count"), int)
        or independent["directory_count"] < 0
        or not isinstance(independent.get("total_bytes"), int)
        or independent["total_bytes"] <= 0
        or not isinstance(independent.get("visible_csn"), int)
        or independent["visible_csn"] <= 0
        or HEX64.fullmatch(str(independent.get("checkpoint_digest", ""))) is None
    ):
        raise GateFailure("independent backup verification is incomplete")
    return {
        "kill-restart": f"{payload['forced_daemon_terminations']} forced daemon terminations across {payload['records']} all-engine records followed by healthy reopen",
        "backup-restore": "Native backup restored with all-engine state equivalence",
        "doctor-after-restore": "restored Native directory reported healthy",
        "independent-tool": independent["verifier"],
        "offline": "backup envelope verified without runtime or product dependencies",
        "state-equivalence": "restored SQL, structures, lexical, and vector state matched",
    }


def validate_process_crash(payload: dict[str, Any], commit: str) -> dict[str, str]:
    if (
        payload.get("schema") != "hyphae.native.process-crash-matrix.v4"
        or payload.get("status") != "process-crash-not-power-loss"
        or payload.get("source_commit") != commit
        or payload.get("durability") != "strict"
        or payload.get("all_engine_csn") != 1
        or not isinstance(payload.get("environment"), str)
        or not payload["environment"].strip()
        or not isinstance(payload.get("target"), str)
        or not payload["target"].endswith("-linux")
    ):
        raise GateFailure("process-crash artifact identity is invalid")
    commits = require_exact_names(payload.get("commit_boundaries"), COMMIT_BOUNDARIES, "commit")
    checkpoints = require_exact_names(
        payload.get("checkpoint_boundaries"), CHECKPOINT_BOUNDARIES, "checkpoint"
    )
    pins = require_exact_names(
        payload.get("snapshot_pin_boundaries"), SNAPSHOT_PIN_BOUNDARIES, "snapshot pin"
    )
    promotions = require_exact_names(
        payload.get("promotion_boundaries"), PROMOTION_BOUNDARIES, "promotion"
    )
    for row in [*commits, *checkpoints, *pins, *promotions]:
        if row.get("termination") != "signal-9":
            raise GateFailure("Linux process-crash child was not terminated by SIGKILL")
    expected_commits = {
        "blob-staged": ("prior-empty", None, 0),
        "blob-promoted": ("prior-empty", None, 1),
        "page-appended": ("prior-empty", None, 1),
        "page-synchronized": ("prior-empty", None, 1),
        "wal-appended": ("complete-csn-1", 1, 1),
        "wal-synchronized": ("complete-csn-1", 1, 1),
        "root-published": ("complete-csn-1", 1, 1),
    }
    if any(
        (
            row.get("expected_state"),
            row.get("recovered_csn"),
            row.get("recovered_blob_count"),
        )
        != expected_commits[row["boundary"]]
        for row in commits
    ):
        raise GateFailure("process-crash commit recovery differs")
    expected_checkpoints = {
        "manifest-staged": (0, 0, 0, 1),
        "manifest-published": (1, 0, 1, 0),
        "wal-appended": (1, 1, 0, 0),
        "wal-synchronized": (1, 1, 0, 0),
    }
    if any(
        (
            row.get("manifest_count"),
            row.get("checkpoint_count"),
            row.get("unanchored_manifest_suffix"),
            row.get("recovered_temporary_manifests"),
        )
        != expected_checkpoints[row["boundary"]]
        for row in checkpoints
    ):
        raise GateFailure("process-crash checkpoint recovery differs")
    expected_pins = {
        "record-synchronized": ("absent", 0, 0, 1),
        "record-published": ("complete", 1, 1, 1),
    }
    if any(
        (
            row.get("expected_pin"),
            row.get("recovered_pin_count"),
            row.get("pin_directory_files"),
            row.get("retained_page_generations"),
        )
        != expected_pins[row["boundary"]]
        for row in pins
    ):
        raise GateFailure("process-crash snapshot-pin recovery differs")
    expected_markers = {
        "before-rename": "pending",
        "marker-renamed": "authority",
        "parent-synchronized": "authority",
    }
    if any(
        row.get("expected_marker") != expected_markers[row["boundary"]]
        for row in promotions
    ):
        raise GateFailure("process-crash promotion marker differs")
    return {
        "commit-boundaries": f"{len(commits)} strict commit boundaries recovered atomically",
        "checkpoint-boundaries": f"{len(checkpoints)} checkpoint boundaries recovered consistently",
        "snapshot-pins": f"{len(pins)} snapshot-pin publication boundaries recovered consistently",
        "migration-promotion-boundaries": f"{len(promotions)} migration promotion boundaries exposed exactly one valid marker",
    }


def validate_power_loss(payload: dict[str, Any], commit: str) -> dict[str, str]:
    if (
        payload.get("schema") != "hyphae.native.block-power-loss-replay.v1"
        or payload.get("status") != "block-replay-not-physical-device-cut"
        or payload.get("source_commit") != commit
        or payload.get("replay_tool_commit") != POWER_LOSS_COMMIT
        or payload.get("filesystem") != "ext4"
        or payload.get("image_bytes") != 128 * 1024 * 1024
        or payload.get("log_bytes") != 512 * 1024 * 1024
        or payload.get("all_engine_csn") != 1
        or payload.get("scenario_count") != 14
        or payload.get("cleanup") != "complete"
        or HEX40.fullmatch(str(payload.get("source_tree", ""))) is None
        or not isinstance(payload.get("device_mapper_target"), str)
        or not payload["device_mapper_target"]
    ):
        raise GateFailure("power-loss replay artifact is incomplete")
    observations = payload.get("observations")
    if not isinstance(observations, list) or len(observations) != 14:
        raise GateFailure("power-loss replay observations are incomplete")
    identities = {(row.get("family"), row.get("boundary")) for row in observations if isinstance(row, dict)}
    expected = ({("commit", name) for name in COMMIT_BOUNDARIES}
                | {("checkpoint", name) for name in CHECKPOINT_BOUNDARIES}
                | {("promotion", name) for name in PROMOTION_BOUNDARIES})
    if identities != expected:
        raise GateFailure("power-loss replay boundary coverage differs")
    for row in observations:
        if (
            not isinstance(row, dict)
            or row.get("termination") != "signal-9"
            or row.get("cleanup") != "complete"
            or row.get("ext4_post_recovery_check") != "e2fsck-fn-clean"
            or row.get("interruption_mark")
            != f"interrupt-{row.get('family')}-{row.get('boundary')}"
            or not isinstance(row.get("logged_entries_at_interruption"), int)
            or row["logged_entries_at_interruption"] <= 0
            or not isinstance(row.get("highest_allocated_sector"), int)
            or row["highest_allocated_sector"] < 0
            or not isinstance(row.get("live_mount_options"), str)
            or not row["live_mount_options"]
            or not isinstance(row.get("replay_mount_options"), str)
            or not row["replay_mount_options"]
        ):
            raise GateFailure("power-loss replay observation is incomplete")
    expected_markers = {
        "before-rename": {"pending"},
        "marker-renamed": {"pending", "authority"},
        "parent-synchronized": {"authority"},
    }
    promotion_rows = [row for row in observations if row.get("family") == "promotion"]
    if any(
        row.get("recovered_marker") not in expected_markers[row["boundary"]]
        for row in promotion_rows
    ):
        raise GateFailure("power-loss promotion marker differs")
    return {
        "bounded-replay": "14 ext4 dm-log-writes boundaries replayed under fixed image/log bounds",
        "recovery-equivalence": "every stable-media prefix reopened to its asserted logical state",
        "safety-checks": f"isolated topology cleaned; replay tool pinned to {POWER_LOSS_COMMIT}",
    }


def validate_resource(payload: dict[str, Any], commit: str) -> dict[str, str]:
    if (
        payload.get("schema") != "hyphae-native-resource-exhaustion-v1"
        or payload.get("status") != "passed"
        or payload.get("source_commit") != commit
        or payload.get("isolation") != "owned-loopback-ext4"
        or payload.get("post_failure_doctor") != "healthy"
        or payload.get("cleanup") != "complete"
        or HEX40.fullmatch(str(payload.get("source_tree", ""))) is None
        or payload.get("image_bytes") != 128 * 1024 * 1024
        or payload.get("resource_limits")
        != {
            "filesystem_free_bytes": 32 * 1024,
            "address_space_bytes": 16 * 1024 * 1024,
            "open_files": 8,
            "bounded_input_key_bytes": 70_000,
        }
        or not isinstance(payload.get("environment"), str)
        or not payload["environment"].strip()
        or not isinstance(payload.get("platform"), str)
        or not payload["platform"].endswith("-linux")
    ):
        raise GateFailure("resource-exhaustion artifact is incomplete")
    observations = require_object(payload.get("observations"), "resource observations")
    expected = {"disk-full", "read-only", "memory", "descriptors", "bounded-input"}
    if set(observations) != expected or any(not isinstance(value, dict) or not value for value in observations.values()):
        raise GateFailure("resource-exhaustion observation coverage differs")
    return {
        name: f"{name} failure observed; subsequent Native doctor remained healthy"
        for name in sorted(expected)
    }


def validate_fixed_suite(
    payload: dict[str, Any], requirement: str, commit: str, platform: str
) -> dict[str, str]:
    if (
        payload.get("schema") != "hyphae-native-g8-fixed-suite-v1"
        or payload.get("status") != "passed"
        or payload.get("source_commit") != commit
        or payload.get("requirement") != requirement
        or payload.get("platform") != platform
        or HEX40.fullmatch(str(payload.get("source_tree", ""))) is None
    ):
        raise GateFailure("fixed G8 suite artifact identity is invalid")
    checks = payload.get("checks")
    expected_rows = {
        row[0]: list(row[1:])
        for row in SUITES.get(requirement, ())
    }
    exact_fields = {
        "name", "command", "status", "exit_code", "duration_millis",
        "stdout_sha256", "stderr_sha256",
    }
    if not isinstance(checks, list) or len(checks) != len(expected_rows) or any(
        not isinstance(check, dict)
        or set(check) != exact_fields
        or check.get("status") != "passed"
        or check.get("exit_code") != 0
        or check.get("command") != expected_rows.get(check.get("name"))
        or not isinstance(check.get("duration_millis"), int)
        or check["duration_millis"] < 0
        or HEX64.fullmatch(str(check.get("stdout_sha256", ""))) is None
        or HEX64.fullmatch(str(check.get("stderr_sha256", ""))) is None
        for check in checks
    ):
        raise GateFailure("fixed G8 suite contains a failed check")
    names = {check["name"] for check in checks}
    if requirement == "corruption-matrix":
        expected = {
            "pages-wal-manifest-blobs", "indexes", "proof-envelope-payload",
            "proof-truncation",
        }
        if names != expected:
            raise GateFailure("corruption matrix check coverage differs")
        return {
            "pages": "doctor rejected corrupted Native page state",
            "wal": "doctor rejected corrupted Native WAL state",
            "manifest": "doctor rejected corrupted Native manifest state",
            "blobs": "doctor rejected corrupted Native blob state",
            "indexes": "lexical and ANN corruption had zero silent acceptance or partial writes",
            "proofs": "proof/witness truncation and envelope/payload tampering were rejected",
        }
    expected = {"equivalence-and-promotion", "overlap-and-rollback"}
    if requirement != "format2-to-native-migration" or names != expected:
        raise GateFailure("migration matrix check coverage differs")
    return {
        "read-only-source": "migration retained the format-2 source byte-for-byte",
        "identity-mapping": "deterministic source keys and definitions mapped to stable Native IDs",
        "semantic-equivalence": "documents, SQL projection, lexical indexes, and vectors verified",
        "promotion": "verified pending Native target promoted explicitly",
        "rollback": "overlap was rejected and pending target rollback retained the source",
    }


def validate_package(payload: dict[str, Any], commit: str, platform: str) -> dict[str, str]:
    digest = payload.get("archive_sha256")
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    version = workspace["workspace"]["package"]["version"]
    extension = "zip" if platform.endswith("windows-msvc") else "tar.gz"
    expected_archive = f"hyphae-{version}-{platform}.{extension}"
    if (
        payload.get("schema") != "hyphae-native-installed-package-v1"
        or payload.get("status") != "ok"
        or payload.get("source_commit") != commit
        or payload.get("platform") != platform
        or payload.get("installed_smoke") != "passed"
        or payload.get("native_engines") != ["sql", "structures", "search"]
        or payload.get("engine_version") != version
        or payload.get("archive") != expected_archive
        or payload.get("proofs_verified") != 4
        or HEX40.fullmatch(str(payload.get("source_tree", ""))) is None
        or not isinstance(digest, str)
        or len(digest) != 64
        or any(character not in "0123456789abcdef" for character in digest)
    ):
        raise GateFailure("installed Native package artifact is incomplete")
    return {
        "archive-layout": f"safe deterministic archive extracted and exercised ({digest})",
        "installed-native-smoke": "installed binary passed Native SQL, structures, search, checkpoint, backup, restore, and doctor",
    }


def validate_signed_release(payload: dict[str, Any], commit: str) -> dict[str, str]:
    tag = payload.get("tag")
    tag_match = RELEASE_TAG.fullmatch(tag) if isinstance(tag, str) else None
    version = tag_match.group(1) if tag_match is not None else None
    expected_targets = {
        f"hyphae-{version}-aarch64-apple-darwin.tar.gz",
        f"hyphae-{version}-x86_64-apple-darwin.tar.gz",
        f"hyphae-{version}-x86_64-pc-windows-msvc.zip",
        f"hyphae-{version}-x86_64-unknown-linux-gnu.tar.gz",
    }
    digests = (
        payload.get("spdx_sha256"), payload.get("cyclonedx_sha256"),
        payload.get("checksums_sha256"), payload.get("release_evidence_sha256"),
    )
    if (
        payload.get("schema") != "hyphae-native-g8-signed-release-v1"
        or payload.get("status") != "passed"
        or payload.get("source_commit") != commit
        or payload.get("archive_count") != 4
        or not isinstance(payload.get("signature_verifications"), int)
        or payload["signature_verifications"] not in {12, 13}
        or payload.get("attestation_verifications") != 12
        or not isinstance(payload.get("provenance_targets"), list)
        or set(payload["provenance_targets"]) != expected_targets
        or len(payload["provenance_targets"]) != len(expected_targets)
        or any(
            not isinstance(digest, str) or len(digest) != 64
            or any(character not in "0123456789abcdef" for character in digest)
            for digest in digests
        )
    ):
        raise GateFailure("signed release artifact is incomplete")
    return {
        "spdx": f"SPDX SBOM verified ({digests[0]})",
        "cyclonedx": f"CycloneDX SBOM verified ({digests[1]})",
        "checksums": f"canonical SHA256SUMS verified ({digests[2]})",
        "cosign": f"{payload['signature_verifications']} signatures and 12 attestations cryptographically verified",
        "provenance": "SLSA provenance attestation verified for all four target archives",
    }


def observations(
    requirement: str, payload: dict[str, Any], commit: str, platform: str
) -> dict[str, str]:
    if requirement in {"native-soak", "independent-restore-verification"}:
        return validate_soak(payload, commit, platform)
    if requirement == "process-crash-recovery":
        return validate_process_crash(payload, commit)
    if requirement == "power-loss-replay":
        return validate_power_loss(payload, commit)
    if requirement == "resource-exhaustion":
        return validate_resource(payload, commit)
    if requirement in {"corruption-matrix", "format2-to-native-migration"}:
        return validate_fixed_suite(payload, requirement, commit, platform)
    if requirement == "multiplatform-packaging":
        return validate_package(payload, commit, platform)
    if requirement == "sbom-signatures-provenance":
        return validate_signed_release(payload, commit)
    raise GateFailure(f"no suite-specific G8 validator exists for {requirement}")


def produce(
    repository: Path,
    requirement: str,
    platform: str,
    commit: str,
    artifact: Path,
) -> dict[str, Any]:
    if HEX40.fullmatch(commit) is None:
        raise GateFailure("source commit is not a canonical SHA-1")
    _, rows = authority(repository)
    row = rows.get(requirement)
    if row is None or platform not in row["platforms"]:
        raise GateFailure("G8 requirement or platform is not admitted")
    resolved = artifact.resolve(strict=True)
    if not resolved.is_file():
        raise GateFailure("G8 artifact is not a regular file")
    payload = json.loads(resolved.read_text(encoding="utf-8"))
    payload = require_object(payload, "G8 artifact")
    verified = observations(requirement, payload, commit, platform)
    digest = hashlib.sha256(resolved.read_bytes()).hexdigest()
    acceptance = {}
    for name in row["acceptance"]:
        observation = verified.get(name)
        if observation is None:
            raise GateFailure(f"suite validator did not prove {requirement}/{name}")
        acceptance[name] = {
            "status": "passed",
            "artifact_sha256": digest,
            "observation": observation,
        }
    return {
        "schema": "hyphae-native-g8-receipt-v1",
        "gate": "G8",
        "status": "passed",
        "evidence_class": "closure-candidate",
        "source_commit": commit,
        "requirement": requirement,
        "platform": platform,
        "acceptance": acceptance,
        "artifacts": [{"name": resolved.name, "sha256": digest}],
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--requirement", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        receipt = produce(
            Path(__file__).resolve().parents[1], arguments.requirement,
            arguments.platform, arguments.expected_commit, arguments.artifact,
        )
        arguments.output.write_text(
            json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except (GateFailure, OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"native G8 receipt production failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
