#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Reverify the complete signed release layout and emit G8 evidence."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

from conclude_release_sbom_licenses import (
    discover_package_authorities,
    expected_artifact_identities,
)


PACKAGING = Path(__file__).resolve().parent
ROOT = Path(
    os.environ.get(
        "HYPHAE_RELEASE_SOURCE_ROOT",
        PACKAGING.parent,
    )
).resolve()
RELEASE_TAG = re.compile(
    r"(?:release-)?v([0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.]+)?|[0-9]+\.[0-9]+\.[0-9]+-final)\Z"
)
SOFTWARE_LICENSE = "Apache-2.0"


def is_hyphae_component(name: object) -> bool:
    return isinstance(name, str) and (
        name == "hyphae"
        or name.startswith("hyphae-")
        or name == "@hyphae_/hyphae"
        or name.startswith("@hyphae_/hyphae-")
    )


def verify_spdx_hyphae_licenses(document: object) -> list[str]:
    if not isinstance(document, dict) or not isinstance(document.get("packages"), list):
        raise RuntimeError("SPDX SBOM packages must be a list")
    verified: set[str] = set()
    for package in document["packages"]:
        if not isinstance(package, dict) or not isinstance(package.get("name"), str):
            raise RuntimeError("SPDX SBOM package must be a named object")
        name = package["name"]
        if not is_hyphae_component(name):
            continue
        for field in ("licenseDeclared", "licenseConcluded"):
            if package.get(field) != SOFTWARE_LICENSE:
                raise RuntimeError(
                    f"SPDX Hyphae component {name} {field} must be {SOFTWARE_LICENSE}"
                )
        verified.add(name)
    if not verified:
        raise RuntimeError("SPDX SBOM contains no Hyphae component")
    return sorted(verified)


def spdx_hyphae_identities(document: object) -> list[tuple[str, str, str]]:
    if not isinstance(document, dict) or not isinstance(document.get("packages"), list):
        raise RuntimeError("SPDX SBOM packages must be a list")
    identities: list[tuple[str, str, str]] = []
    for package in document["packages"]:
        if not isinstance(package, dict) or not is_hyphae_component(package.get("name")):
            continue
        name = package["name"]
        version = package.get("versionInfo")
        references = package.get("externalRefs")
        if not isinstance(version, str) or not isinstance(references, list):
            raise RuntimeError(f"SPDX Hyphae component {name} identity is incomplete")
        purls = [
            reference.get("referenceLocator")
            for reference in references
            if isinstance(reference, dict) and reference.get("referenceType") == "purl"
        ]
        if len(purls) != 1 or not isinstance(purls[0], str):
            raise RuntimeError(f"SPDX Hyphae component {name} must have one package URL")
        identities.append((name, version, purls[0]))
    return identities


def cyclonedx_license_identifiers(component: dict) -> list[str] | None:
    licenses = component.get("licenses")
    if not isinstance(licenses, list) or not licenses:
        return None
    identifiers: list[str] = []
    for choice in licenses:
        if not isinstance(choice, dict):
            return None
        expression = choice.get("expression")
        if isinstance(expression, str):
            identifiers.append(expression)
            continue
        license_value = choice.get("license")
        if not isinstance(license_value, dict) or not isinstance(
            license_value.get("id"), str
        ):
            return None
        identifiers.append(license_value["id"])
    return identifiers


def cyclonedx_components(document: dict) -> list[dict]:
    roots = document.get("components", [])
    if not isinstance(roots, list):
        raise RuntimeError("CycloneDX SBOM components must be a list")
    metadata = document.get("metadata")
    if metadata is not None:
        if not isinstance(metadata, dict):
            raise RuntimeError("CycloneDX SBOM metadata must be an object")
        component = metadata.get("component")
        if component is not None:
            roots = [component, *roots]
    pending = list(roots)
    flattened: list[dict] = []
    while pending:
        component = pending.pop()
        if not isinstance(component, dict):
            raise RuntimeError("CycloneDX SBOM component must be an object")
        children = component.get("components", [])
        if not isinstance(children, list):
            raise RuntimeError("CycloneDX nested components must be a list")
        pending.extend(children)
        flattened.append(component)
    return flattened


def verify_cyclonedx_hyphae_licenses(document: object) -> list[str]:
    if not isinstance(document, dict):
        raise RuntimeError("CycloneDX SBOM must be an object")
    verified: set[str] = set()
    for component in cyclonedx_components(document):
        name = component.get("name")
        if not is_hyphae_component(name):
            continue
        identifiers = cyclonedx_license_identifiers(component)
        if identifiers is None or set(identifiers) != {SOFTWARE_LICENSE}:
            raise RuntimeError(
                f"CycloneDX Hyphae component {name} license must be {SOFTWARE_LICENSE}"
            )
        verified.add(name)
    if not verified:
        raise RuntimeError("CycloneDX SBOM contains no Hyphae component")
    return sorted(verified)


def cyclonedx_hyphae_identities(document: object) -> list[tuple[str, str, str]]:
    if not isinstance(document, dict):
        raise RuntimeError("CycloneDX SBOM must be an object")
    identities: list[tuple[str, str, str]] = []
    for component in cyclonedx_components(document):
        if not is_hyphae_component(component.get("name")):
            continue
        name = component["name"]
        version = component.get("version")
        purl = component.get("purl")
        if not isinstance(version, str) or not isinstance(purl, str):
            raise RuntimeError(
                f"CycloneDX Hyphae component {name} identity is incomplete"
            )
        identities.append((name, version, purl))
    return identities


def expected_hyphae_identities(root: Path = ROOT) -> Counter[tuple[str, str, str]]:
    authorities = discover_package_authorities(root)
    return Counter(
        (identity.name, identity.version, identity.purl)
        for identity in expected_artifact_identities(root, authorities).elements()
    )


def read_json_document(path: Path, label: str) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"{label} is not valid JSON: {error}") from error


def expected_archives(tag: str) -> set[str]:
    match = RELEASE_TAG.fullmatch(tag)
    if match is None:
        raise ValueError("release tag must be canonical")
    version = match.group(1)
    if tag.startswith("release-"):
        version = version.removesuffix("-final").removesuffix("-crates")
    if tag.startswith("release-") and version.endswith("-final"):
        version = version.removesuffix("-final")
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
        sys.executable, PACKAGING / "release_evidence.py", "verify",
        "--directory", directory, "--manifest", manifest, "--commit", commit,
    ]
    if tag_object is not None and tag_target is not None:
        evidence_arguments.extend(
            ("--tag-object", tag_object, "--tag-target", tag_target)
        )
    run(*evidence_arguments)
    run(
        sys.executable, PACKAGING / "finalize_release.py",
        "--directory", directory, "--tag", tag, "--verify",
    )
    archives = sorted([*directory.glob("*.tar.gz"), *directory.glob("*.zip")])
    if {archive.name for archive in archives} != expected_archives(tag):
        raise RuntimeError("G8 release must contain the four canonical target archives")
    spdx = directory / f"hyphae-{tag}.spdx.json"
    cyclonedx = directory / f"hyphae-{tag}.cdx.json"
    checksums = directory / "SHA256SUMS"
    if not all(path.is_file() for path in (spdx, cyclonedx, checksums, manifest)):
        raise RuntimeError("G8 release metadata set is incomplete")
    spdx_document = read_json_document(spdx, "SPDX SBOM")
    cyclonedx_document = read_json_document(cyclonedx, "CycloneDX SBOM")
    spdx_components = verify_spdx_hyphae_licenses(spdx_document)
    cyclonedx_component_names = verify_cyclonedx_hyphae_licenses(
        cyclonedx_document
    )
    spdx_identities = Counter(spdx_hyphae_identities(spdx_document))
    cyclonedx_identities = Counter(cyclonedx_hyphae_identities(cyclonedx_document))
    if spdx_identities != cyclonedx_identities:
        raise RuntimeError("SPDX and CycloneDX Hyphae component identities differ")
    expected_identities = expected_hyphae_identities()
    if spdx_identities != expected_identities:
        raise RuntimeError("release SBOM omits or adds first-party package identities")
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
    return {
        "schema": "hyphae-native-g8-signed-release-v1",
        "status": "passed",
        "source_commit": commit,
        "tag": tag,
        "archive_count": len(archives),
        "signature_verifications": signatures,
        "attestation_verifications": attestations,
        "software_license": SOFTWARE_LICENSE,
        "license_authority": "tracked-package-manifests-and-local-locks-v1",
        "first_party_artifact_count": sum(expected_identities.values()),
        "first_party_identity_count": len(expected_identities),
        "spdx_hyphae_components": spdx_components,
        "cyclonedx_hyphae_components": cyclonedx_component_names,
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
