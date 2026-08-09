#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Reverify the complete signed release layout and emit G8 evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RELEASE_TAG = re.compile(r"v([0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?)\Z")


def expected_archives(tag: str) -> set[str]:
    match = RELEASE_TAG.fullmatch(tag)
    if match is None:
        raise ValueError("release tag must be canonical")
    version = match.group(1)
    return {
        f"hyphae-{version}-aarch64-apple-darwin.tar.gz",
        f"hyphae-{version}-x86_64-apple-darwin.tar.gz",
        f"hyphae-{version}-x86_64-pc-windows-msvc.zip",
        f"hyphae-{version}-x86_64-unknown-linux-gnu.tar.gz",
    }


def run(*arguments: str | Path) -> None:
    subprocess.run(
        tuple(str(value) for value in arguments), cwd=ROOT, check=True, timeout=300
    )


def verify_blob(path: Path, bundle: Path, identity: str) -> None:
    run(
        "cosign", "verify-blob", "--bundle", bundle,
        "--certificate-identity", identity,
        "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com",
        path,
    )


def verify_attestation(
    path: Path, bundle: Path, kind: str, identity: str
) -> None:
    run(
        "cosign", "verify-blob-attestation", "--bundle", bundle, "--type", kind,
        "--certificate-identity", identity,
        "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com",
        path,
    )


def verify(
    directory: Path,
    commit: str,
    tag: str,
    certificate_identity: str,
    tag_object: str | None = None,
    tag_target: str | None = None,
) -> dict:
    directory = directory.resolve(strict=True)
    if len(commit) != 40 or any(character not in "0123456789abcdef" for character in commit):
        raise ValueError("release commit must be canonical lowercase SHA-1")
    if (tag_object is None) != (tag_target is None):
        raise ValueError("release tag object and target must be provided together")
    manifest = directory / f"hyphae-{tag}.release-evidence.json"
    evidence_arguments: list[str | Path] = [
        sys.executable, "packaging/release_evidence.py", "verify",
        "--directory", directory, "--manifest", manifest, "--commit", commit,
    ]
    if tag_object is not None and tag_target is not None:
        evidence_arguments.extend(
            ("--tag-object", tag_object, "--tag-target", tag_target)
        )
    run(*evidence_arguments)
    run(
        sys.executable, "packaging/finalize_release.py",
        "--directory", directory, "--tag", tag, "--verify",
    )
    archives = sorted([*directory.glob("*.tar.gz"), *directory.glob("*.zip")])
    if {archive.name for archive in archives} != expected_archives(tag):
        raise RuntimeError("G8 release must contain the four canonical target archives")
    signatures = 0
    attestations = 0
    for path in sorted(directory.iterdir()):
        if not path.is_file() or path.name.endswith(".sigstore.json"):
            continue
        verify_blob(path, Path(f"{path}.sigstore.json"), certificate_identity)
        signatures += 1
    for archive in archives:
        verify_attestation(
            archive, Path(f"{archive}.intoto.sigstore.json"),
            "slsaprovenance1", certificate_identity,
        )
        verify_attestation(
            archive, Path(f"{archive}.spdx.attestation.sigstore.json"),
            "spdxjson", certificate_identity,
        )
        verify_attestation(
            archive, Path(f"{archive}.cyclonedx.attestation.sigstore.json"),
            "cyclonedx", certificate_identity,
        )
        attestations += 3
    spdx = directory / f"hyphae-{tag}.spdx.json"
    cyclonedx = directory / f"hyphae-{tag}.cdx.json"
    checksums = directory / "SHA256SUMS"
    if not all(path.is_file() for path in (spdx, cyclonedx, checksums, manifest)):
        raise RuntimeError("G8 release metadata set is incomplete")
    return {
        "schema": "hyphae-native-g8-signed-release-v1",
        "status": "passed",
        "source_commit": commit,
        "tag": tag,
        "archive_count": len(archives),
        "signature_verifications": signatures,
        "attestation_verifications": attestations,
        "spdx_sha256": hashlib.sha256(spdx.read_bytes()).hexdigest(),
        "cyclonedx_sha256": hashlib.sha256(cyclonedx.read_bytes()).hexdigest(),
        "checksums_sha256": hashlib.sha256(checksums.read_bytes()).hexdigest(),
        "release_evidence_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
        "provenance_targets": [archive.name for archive in archives],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--tag-object")
    parser.add_argument("--tag-target")
    parser.add_argument("--certificate-identity", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    result = verify(
        arguments.directory, arguments.commit, arguments.tag,
        arguments.certificate_identity, arguments.tag_object, arguments.tag_target,
    )
    arguments.output.write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
