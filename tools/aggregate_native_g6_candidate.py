#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Select hosted G6 audits, inject evidence, and evaluate open readiness."""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

from tools.check_native_g6_foundation import PLATFORMS, REQUIREMENTS
from tools.check_native_g6_manifests import MANIFEST_NAMES
from tools.check_native_g6_conformance import aggregate as aggregate_conformance, validate_receipt as validate_conformance_receipt
from tools.inject_native_g6_evidence import inject


ROOT = Path(__file__).resolve().parents[1]


def validate_platform_artifacts(root: Path, source_commit: str, digests: dict[str, str]) -> dict[tuple[str, str], Path]:
    cells: dict[tuple[str, str], Path] = {}
    conformance_receipts = []
    for platform in PLATFORMS:
        summary_path = root / platform / "platform-summary.json"
        summary = json.loads(summary_path.read_text(encoding="utf-8"))
        rows = summary.get("requirement_receipts")
        if (
            summary.get("schema") != "hyphae-native-g6-platform-candidate-v1"
            or summary.get("gate") != "G6"
            or summary.get("status") != "passed"
            or summary.get("evidence_class") != "supporting-not-closure"
            or summary.get("source_commit") != source_commit
            or summary.get("platform") != platform
            or summary.get("manifest_sha256") != digests
            or not isinstance(summary.get("conformance_receipt_sha256"), str)
            or not isinstance(summary.get("conformance_audit_sha256"), str)
            or summary.get("requirements") != len(REQUIREMENTS)
            or summary.get("claims") != []
            or summary.get("closure_declared") is not False
            or not isinstance(rows, list)
            or [row.get("id") for row in rows if isinstance(row, dict)] != REQUIREMENTS
        ):
            raise ValueError(f"invalid G6 {platform} platform summary")
        conformance_path = root / platform / "native-g6-conformance-receipt.json"
        conformance_audit_path = root / platform / "native-g6-conformance-audit.json"
        if (
            hashlib.sha256(conformance_path.read_bytes()).hexdigest() != summary["conformance_receipt_sha256"]
            or hashlib.sha256(conformance_audit_path.read_bytes()).hexdigest() != summary["conformance_audit_sha256"]
        ):
            raise ValueError(f"invalid G6 {platform} conformance evidence binding")
        conformance = validate_conformance_receipt(json.loads(conformance_path.read_text(encoding="utf-8")))
        if conformance["source_commit"] != source_commit or conformance["platform"] != platform:
            raise ValueError(f"invalid G6 {platform} conformance identity")
        conformance_receipts.append(conformance)
        for row in rows:
            receipt = root / platform / "receipts" / f"{row['id']}.json"
            audit = root / platform / "audits" / f"{row['id']}.json"
            if (
                set(row) != {"id", "implementation_status", "uncovered_acceptance", "receipt_sha256", "audit_sha256"}
                or hashlib.sha256(receipt.read_bytes()).hexdigest() != row["receipt_sha256"]
                or hashlib.sha256(audit.read_bytes()).hexdigest() != row["audit_sha256"]
            ):
                raise ValueError(f"invalid G6 {platform} receipt identity for {row['id']}")
            audit_payload = json.loads(audit.read_text(encoding="utf-8"))
            cell = (row["id"], platform)
            if (
                cell in cells
                or audit_payload.get("requirement") != row["id"]
                or audit_payload.get("platform") != platform
                or audit_payload.get("source_commit") != source_commit
                or audit_payload.get("manifest_sha256") != digests
            ):
                raise ValueError(f"invalid or duplicate G6 matrix cell {row['id']}/{platform}")
            cells[cell] = audit
    expected = {(requirement, platform) for requirement in REQUIREMENTS for platform in PLATFORMS}
    if set(cells) != expected:
        raise ValueError("incomplete G6 requirement/platform receipt matrix")
    aggregate_conformance(conformance_receipts)
    return cells


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--manifest-audit", type=Path, required=True)
    parser.add_argument("--manifest-digests", type=Path, required=True)
    parser.add_argument("--platform-root", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    output = args.output_dir
    evidence_dir = output / "evidence"
    evidence_dir.mkdir(parents=True, exist_ok=True)
    digests = json.loads(args.manifest_digests.read_text(encoding="utf-8"))
    if set(digests) != set(MANIFEST_NAMES):
        raise ValueError("incomplete G6 manifest digest set")
    manifest_audit = json.loads(args.manifest_audit.read_text(encoding="utf-8"))
    if (
        manifest_audit.get("schema") != "hyphae-native-g6-manifest-audit-v1"
        or manifest_audit.get("status") != "passed"
        or manifest_audit.get("source_commit") != args.source_commit
        or manifest_audit.get("manifest_sha256") != digests
    ):
        raise ValueError("mismatched G6 manifest audit artifact")
    manifest_root = args.manifest_audit.parent / "config"
    files = {
        "profile": "native-g6-readiness-profile.json",
        "evidence": "native-g6-readiness-evidence.json",
        "inventory": "native-g6-inventory.json",
        "authority": "native-g6-authority-manifest.json",
        "workload": "native-g6-workload-manifest.json",
        "suite": "native-g6-suite-manifest.json",
        "predecessor": "native-g6-predecessor-manifest.json",
    }
    for name, filename in files.items():
        if hashlib.sha256((manifest_root / filename).read_bytes()).hexdigest() != digests[name]:
            raise ValueError(f"mismatched G6 {name} manifest artifact")
    cells = validate_platform_artifacts(args.platform_root, args.source_commit, digests)
    evidence = json.loads((manifest_root / files["evidence"]).read_text(encoding="utf-8"))

    predecessor = evidence_dir / "predecessor.json"
    shutil.copyfile(args.manifest_audit, predecessor)
    evidence = inject(output, evidence, "predecessor", Path("evidence/predecessor.json"), args.source_commit, digests)

    for requirement in REQUIREMENTS:
        for platform in PLATFORMS:
            source = cells[(requirement, platform)]
            target = evidence_dir / f"{requirement}--{platform}.json"
            shutil.copyfile(source, target)
            evidence = inject(
                output,
                evidence,
                "requirement",
                Path("evidence") / target.name,
                args.source_commit,
                digests,
                requirement,
                platform,
            )

    evidence_path = output / "native-g6-readiness-evidence.json"
    evidence_path.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    command = [
        sys.executable, str(ROOT / "tools/check_native_g6_readiness.py"),
        "--root", str(output),
        "--profile", str(manifest_root / files["profile"]),
        "--manifest-root", str(manifest_root),
        "--evidence", str(evidence_path),
        "--expected-commit", args.source_commit,
    ]
    for name in MANIFEST_NAMES:
        command += [f"--{name}-sha256", digests[name]]
    command += ["--output", str(output / "native-g6-readiness.json")]
    completed = subprocess.run(command, cwd=ROOT, check=False)
    return 0 if completed.returncode in {0, 1} else completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())
