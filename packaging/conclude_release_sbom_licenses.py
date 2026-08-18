#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Conclude first-party release SBOM licenses from tracked package manifests."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any


ROOT = Path(
    os.environ.get(
        "HYPHAE_RELEASE_SOURCE_ROOT",
        Path(__file__).resolve().parents[1],
    )
).resolve()
SOFTWARE_LICENSE = "Apache-2.0"
SYFT_VERSION = "1.46.0"
IGNORED_DIRECTORIES = frozenset({"build", "dist", "node_modules", "target"})


@dataclass(frozen=True)
class PrivateNpmTool:
    name: str
    manifest: str
    lock: str


PRIVATE_NPM_TOOLS = (
    PrivateNpmTool(
        name="hyphae-mcp-conformance-hosts",
        manifest="conformance/mcp/hosts/package.json",
        lock="conformance/mcp/hosts/package-lock.json",
    ),
    PrivateNpmTool(
        name="framework-host-smoke",
        manifest="integrations/host-smoke/package.json",
        lock="integrations/host-smoke/package-lock.json",
    ),
    PrivateNpmTool(
        name="hyphae-premium-site",
        manifest="website/package.json",
        lock="website/package-lock.json",
    ),
)
PRIVATE_NPM_TOOLS_BY_MANIFEST = {tool.manifest: tool for tool in PRIVATE_NPM_TOOLS}
PRIVATE_NPM_TOOLS_BY_NAME = {tool.name: tool for tool in PRIVATE_NPM_TOOLS}
PRIVATE_NPM_TOOLS_BY_LOCK = {tool.lock: tool for tool in PRIVATE_NPM_TOOLS}


@dataclass(frozen=True)
class PackageAuthority:
    package_type: str
    name: str
    version: str
    purl: str
    manifest: Path


@dataclass(frozen=True)
class ArtifactIdentity:
    package_type: str
    name: str
    version: str
    purl: str
    location: str


def private_npm_tool_for_manifest(
    manifest: Path, document: dict[str, Any], root: Path
) -> PrivateNpmTool | None:
    relative = manifest.resolve().relative_to(root.resolve()).as_posix()
    tool = PRIVATE_NPM_TOOLS_BY_MANIFEST.get(relative)
    name = document.get("name")
    if tool is None:
        expected = PRIVATE_NPM_TOOLS_BY_NAME.get(name)
        if expected is not None:
            raise RuntimeError(
                f"{manifest}: private npm tool {name} must use {expected.manifest}"
            )
        return None
    if name != tool.name or document.get("private") is not True:
        raise RuntimeError(
            f"{manifest}: excluded private npm tool must be {tool.name} "
            "with private=true"
        )
    return tool


def is_hyphae_component(name: object) -> bool:
    return isinstance(name, str) and (
        name == "hyphae"
        or name.startswith("hyphae-")
        or name == "@celiums/hyphae"
        or name.startswith("@celiums/hyphae-")
    )


def require_string(value: object, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise RuntimeError(f"{label} must be a non-empty string")
    return value


def package_purl(package_type: str, name: str, version: str) -> str:
    encoded_name = name.replace("@", "%40") if package_type == "npm" else name
    ecosystem = {"rust-crate": "cargo", "npm": "npm", "python": "pypi"}.get(
        package_type
    )
    if ecosystem is None:
        raise RuntimeError(f"unsupported first-party package type: {package_type}")
    return f"pkg:{ecosystem}/{encoded_name}@{version}"


def unversioned_npm_purl(name: str) -> str:
    return f"pkg:npm/{name.replace('@', '%40')}"


def inherited_workspace_value(
    value: object,
    workspace_package: dict[str, Any],
    key: str,
    manifest: Path,
) -> str:
    if isinstance(value, str) and value:
        return value
    if isinstance(value, dict) and value == {"workspace": True}:
        inherited = workspace_package.get(key)
        if isinstance(inherited, str) and inherited:
            return inherited
    raise RuntimeError(f"{manifest}: package {key} has no exact authority")


def discover_package_authorities(root: Path) -> dict[tuple[str, str, str], PackageAuthority]:
    root_manifest = root / "Cargo.toml"
    root_toml = tomllib.loads(root_manifest.read_text(encoding="utf-8"))
    workspace_package = root_toml.get("workspace", {}).get("package", {})
    if not isinstance(workspace_package, dict):
        raise RuntimeError("Cargo.toml: workspace.package must be an object")

    authorities: dict[tuple[str, str, str], PackageAuthority] = {}

    def add(
        package_type: str,
        name: object,
        version: object,
        license_value: object,
        path: Path,
    ) -> None:
        package_name = require_string(name, f"{path}: package name")
        if not is_hyphae_component(package_name):
            return
        package_version = require_string(version, f"{path}: package version")
        if license_value != SOFTWARE_LICENSE:
            raise RuntimeError(
                f"{path}: {package_name} license must be {SOFTWARE_LICENSE}"
            )
        authority = PackageAuthority(
            package_type=package_type,
            name=package_name,
            version=package_version,
            purl=package_purl(package_type, package_name, package_version),
            manifest=path,
        )
        key = (package_type, package_name, package_version)
        previous = authorities.get(key)
        if previous is not None and previous.manifest != path:
            raise RuntimeError(
                f"ambiguous first-party package authority for {package_name}@{package_version}"
            )
        authorities[key] = authority

    for manifest in sorted(root.rglob("Cargo.toml")):
        if IGNORED_DIRECTORIES.intersection(manifest.parts):
            continue
        document = tomllib.loads(manifest.read_text(encoding="utf-8"))
        package = document.get("package")
        if not isinstance(package, dict):
            continue
        name = package.get("name")
        if not is_hyphae_component(name):
            continue
        version = inherited_workspace_value(
            package.get("version"), workspace_package, "version", manifest
        )
        license_value = inherited_workspace_value(
            package.get("license"), workspace_package, "license", manifest
        )
        add("rust-crate", name, version, license_value, manifest)

    for manifest in sorted(root.rglob("package.json")):
        if IGNORED_DIRECTORIES.intersection(manifest.parts):
            continue
        document = json.loads(manifest.read_text(encoding="utf-8"))
        if not isinstance(document, dict):
            raise RuntimeError(f"{manifest}: package manifest must be an object")
        private_tool = private_npm_tool_for_manifest(manifest, document, root)
        if private_tool is not None:
            version = require_string(
                document.get("version"), f"{manifest}: package version"
            )
            lock = root / private_tool.lock
            if not lock.is_file():
                raise RuntimeError(f"{manifest}: private npm tool lock is missing")
            validate_private_npm_tool_lock(lock, private_tool, version)
            continue
        add(
            "npm",
            document.get("name"),
            document.get("version"),
            document.get("license"),
            manifest,
        )

    for manifest in sorted(root.rglob("pyproject.toml")):
        if IGNORED_DIRECTORIES.intersection(manifest.parts):
            continue
        document = tomllib.loads(manifest.read_text(encoding="utf-8"))
        project = document.get("project")
        if not isinstance(project, dict):
            continue
        add(
            "python",
            project.get("name"),
            project.get("version"),
            project.get("license"),
            manifest,
        )

    if not authorities:
        raise RuntimeError("no first-party package authorities were discovered")
    return authorities


def artifact_location(artifact: dict[str, Any], root: Path) -> tuple[str, Path]:
    locations = artifact.get("locations")
    if not isinstance(locations, list) or len(locations) != 1:
        raise RuntimeError(f"{artifact.get('name')}: expected one package location")
    location = locations[0]
    if not isinstance(location, dict):
        raise RuntimeError(f"{artifact.get('name')}: package location must be an object")
    raw_path = require_string(location.get("path"), f"{artifact.get('name')}: location")
    pure = PurePosixPath(raw_path)
    if not pure.is_absolute() or ".." in pure.parts:
        raise RuntimeError(f"{artifact.get('name')}: package location is not canonical")
    resolved = (root / Path(*pure.parts[1:])).resolve()
    try:
        resolved.relative_to(root.resolve())
    except ValueError as error:
        raise RuntimeError(
            f"{artifact.get('name')}: package location escapes repository"
        ) from error
    if not resolved.is_file():
        raise RuntimeError(f"{artifact.get('name')}: package location does not exist")
    return raw_path, resolved


def private_npm_tool_for_artifact(
    artifact: dict[str, Any], location: Path, root: Path
) -> PrivateNpmTool | None:
    name = artifact.get("name")
    tool = PRIVATE_NPM_TOOLS_BY_NAME.get(name)
    if tool is None:
        return None
    expected_location = (root / tool.lock).resolve()
    if (
        artifact.get("type") != "npm"
        or artifact.get("foundBy") != "javascript-lock-cataloger"
        or location.resolve() != expected_location
    ):
        raise RuntimeError(f"{name}: private npm tool artifact evidence does not match")
    manifest = (root / tool.manifest).resolve()
    if not manifest.is_file():
        raise RuntimeError(f"{name}: private npm tool manifest is missing")
    document = json.loads(manifest.read_text(encoding="utf-8"))
    if not isinstance(document, dict):
        raise RuntimeError(f"{manifest}: package manifest must be an object")
    if private_npm_tool_for_manifest(manifest, document, root) != tool:
        raise RuntimeError(f"{name}: private npm tool manifest does not match")
    version = require_string(document.get("version"), f"{manifest}: package version")
    validate_private_npm_tool_lock(location, tool, version)
    if (
        artifact.get("version") != version
        or artifact.get("purl") != package_purl("npm", tool.name, version)
    ):
        raise RuntimeError(f"{name}: private npm tool inventory does not match")
    return tool


def authority_for_exact_artifact(
    artifact: dict[str, Any],
    authorities: dict[tuple[str, str, str], PackageAuthority],
) -> PackageAuthority:
    package_type = require_string(artifact.get("type"), "artifact type")
    name = require_string(artifact.get("name"), "artifact name")
    version = require_string(artifact.get("version"), f"{name}: version")
    authority = authorities.get((package_type, name, version))
    if authority is None:
        raise RuntimeError(f"{name}@{version}: no exact first-party package authority")
    if artifact.get("purl") != authority.purl:
        raise RuntimeError(f"{name}@{version}: package URL does not match authority")
    return authority


def validate_rust_artifact(artifact: dict[str, Any], location: Path) -> None:
    if location.name != "Cargo.lock" or artifact.get("foundBy") != "rust-cargo-lock-cataloger":
        raise RuntimeError(f"{artifact.get('name')}: Rust artifact lacks Cargo.lock evidence")
    metadata = artifact.get("metadata")
    if (
        not isinstance(metadata, dict)
        or metadata.get("source") != ""
        or metadata.get("checksum") != ""
    ):
        raise RuntimeError(f"{artifact.get('name')}: Rust artifact is not a local path package")
    lock = tomllib.loads(location.read_text(encoding="utf-8"))
    packages = lock.get("package")
    if not isinstance(packages, list) or not any(
        isinstance(package, dict)
        and package.get("name") == artifact.get("name")
        and package.get("version") == artifact.get("version")
        and not package.get("source")
        for package in packages
    ):
        raise RuntimeError(f"{artifact.get('name')}: Cargo.lock evidence does not match")


def npm_lock_packages(location: Path) -> dict[str, Any]:
    document = json.loads(location.read_text(encoding="utf-8"))
    packages = document.get("packages") if isinstance(document, dict) else None
    if not isinstance(packages, dict):
        raise RuntimeError(f"{location}: package-lock packages must be an object")
    return packages


def validate_private_npm_tool_lock(
    location: Path, tool: PrivateNpmTool, version: str
) -> dict[str, Any]:
    document = json.loads(location.read_text(encoding="utf-8"))
    packages = document.get("packages") if isinstance(document, dict) else None
    root_package = packages.get("") if isinstance(packages, dict) else None
    if (
        not isinstance(packages, dict)
        or not isinstance(root_package, dict)
        or document.get("name") != tool.name
        or document.get("version") != version
        or root_package.get("name") != tool.name
        or root_package.get("version") != version
    ):
        raise RuntimeError(f"{location}: private npm tool lock identity does not match")
    return packages


def exact_authority(
    authorities: dict[tuple[str, str, str], PackageAuthority],
    package_type: str,
    name: object,
    version: object,
    label: str,
) -> PackageAuthority:
    package_name = require_string(name, f"{label}: package name")
    package_version = require_string(version, f"{label}: package version")
    authority = authorities.get((package_type, package_name, package_version))
    if authority is None:
        raise RuntimeError(
            f"{label}: no exact first-party authority for "
            f"{package_name}@{package_version}"
        )
    return authority


def validate_exact_npm_artifact(
    artifact: dict[str, Any], location: Path, authority: PackageAuthority
) -> None:
    if (
        location.name != "package-lock.json"
        or artifact.get("foundBy") != "javascript-lock-cataloger"
        or location.parent.resolve() != authority.manifest.parent.resolve()
    ):
        raise RuntimeError(f"{authority.name}: npm artifact lacks package-lock evidence")
    packages = npm_lock_packages(location)
    package = packages.get("")
    if (
        not isinstance(package, dict)
        or package.get("name") != authority.name
        or package.get("version") != authority.version
        or package.get("license") != SOFTWARE_LICENSE
    ):
        raise RuntimeError(f"{authority.name}: package-lock authority does not match")


def authority_for_linked_npm(
    artifact: dict[str, Any],
    root: Path,
    location: Path,
    authorities: dict[tuple[str, str, str], PackageAuthority],
) -> PackageAuthority:
    name = require_string(artifact.get("name"), "linked npm artifact name")
    if (
        artifact.get("foundBy") != "javascript-lock-cataloger"
        or location.name != "package-lock.json"
    ):
        raise RuntimeError(f"{name}: linked npm identity is not canonical")
    metadata = artifact.get("metadata")
    resolved_value = metadata.get("resolved") if isinstance(metadata, dict) else None
    resolved = require_string(resolved_value, f"{name}: linked npm target")
    packages = npm_lock_packages(location)
    link_key = f"node_modules/{name}"
    link = packages.get(link_key)
    if (
        not isinstance(link, dict)
        or link.get("link") is not True
        or link.get("resolved") != resolved
    ):
        raise RuntimeError(f"{name}: linked npm lock entry does not match")
    target = (location.parent / resolved).resolve()
    try:
        target.relative_to(root.resolve())
    except ValueError as error:
        raise RuntimeError(f"{name}: linked npm target escapes repository") from error
    target_key = Path(os.path.relpath(target, location.parent.resolve())).as_posix()
    target_entry = packages.get(target_key)
    target_manifest = target / "package.json"
    if not isinstance(target_entry, dict) or not target_manifest.is_file():
        raise RuntimeError(f"{name}: linked npm target authority is missing")
    manifest = json.loads(target_manifest.read_text(encoding="utf-8"))
    if not isinstance(manifest, dict):
        raise RuntimeError(f"{name}: linked npm target manifest must be an object")
    version = target_entry.get("version")
    authority = authorities.get(("npm", name, version))
    expected = {
        "name": authority.name if authority else None,
        "version": authority.version if authority else None,
        "license": SOFTWARE_LICENSE,
    }
    for key, value in expected.items():
        if target_entry.get(key) != value or manifest.get(key) != value:
            raise RuntimeError(f"{name}: linked npm target {key} does not match authority")
    root_package = packages.get("")
    peer_dependencies = (
        root_package.get("peerDependencies") if isinstance(root_package, dict) else None
    )
    if not isinstance(peer_dependencies, dict) or peer_dependencies.get(name) != version:
        raise RuntimeError(f"{name}: linked npm peer dependency is not exact")
    identity = (artifact.get("version"), artifact.get("purl"))
    if identity not in {
        ("UNKNOWN", unversioned_npm_purl(name)),
        (authority.version, authority.purl),
    }:
        raise RuntimeError(f"{name}: linked npm identity is not canonical")
    return authority


def canonicalize_linked_npm(
    artifact: dict[str, Any], authority: PackageAuthority
) -> None:
    artifact["version"] = authority.version
    artifact["purl"] = authority.purl
    cpes = artifact.get("cpes")
    if not isinstance(cpes, list):
        raise RuntimeError(f"{authority.name}: linked npm CPEs must be a list")
    for cpe_value in cpes:
        if not isinstance(cpe_value, dict) or cpe_value.get("source") != "syft-generated":
            raise RuntimeError(f"{authority.name}: linked npm CPE is not Syft-generated")
        cpe = cpe_value.get("cpe")
        if not isinstance(cpe, str):
            raise RuntimeError(f"{authority.name}: linked npm CPE must be a string")
        fields = cpe.split(":")
        if len(fields) != 13 or fields[:3] != ["cpe", "2.3", "a"]:
            raise RuntimeError(f"{authority.name}: linked npm CPE is not canonical")
        fields[5] = authority.version
        cpe_value["cpe"] = ":".join(fields)


def lock_location(root: Path, path: Path) -> str:
    return f"/{path.resolve().relative_to(root.resolve()).as_posix()}"


def expected_cargo_identities(
    root: Path,
    authorities: dict[tuple[str, str, str], PackageAuthority],
) -> list[ArtifactIdentity]:
    identities: list[ArtifactIdentity] = []
    for path in sorted(root.rglob("Cargo.lock")):
        if IGNORED_DIRECTORIES.intersection(path.parts):
            continue
        document = tomllib.loads(path.read_text(encoding="utf-8"))
        packages = document.get("package")
        if not isinstance(packages, list):
            raise RuntimeError(f"{path}: Cargo.lock packages must be a list")
        for package in packages:
            if not isinstance(package, dict) or not is_hyphae_component(
                package.get("name")
            ):
                continue
            if package.get("source"):
                continue
            authority = exact_authority(
                authorities,
                "rust-crate",
                package.get("name"),
                package.get("version"),
                str(path),
            )
            identities.append(
                ArtifactIdentity(
                    package_type="rust-crate",
                    name=authority.name,
                    version=authority.version,
                    purl=authority.purl,
                    location=lock_location(root, path),
                )
            )
    return identities


def package_name_from_lock_key(key: str, package: dict[str, Any]) -> object:
    if isinstance(package.get("name"), str):
        return package["name"]
    marker = "node_modules/"
    if marker not in key:
        return None
    return key.rsplit(marker, maxsplit=1)[1]


def linked_npm_authority_from_lock(
    name: str,
    package: dict[str, Any],
    packages: dict[str, Any],
    location: Path,
    root: Path,
    authorities: dict[tuple[str, str, str], PackageAuthority],
) -> PackageAuthority:
    resolved = require_string(package.get("resolved"), f"{name}: linked npm target")
    target = (location.parent / resolved).resolve()
    try:
        target.relative_to(root.resolve())
    except ValueError as error:
        raise RuntimeError(f"{name}: linked npm target escapes repository") from error
    target_key = Path(os.path.relpath(target, location.parent.resolve())).as_posix()
    target_entry = packages.get(target_key)
    target_manifest = target / "package.json"
    if not isinstance(target_entry, dict) or not target_manifest.is_file():
        raise RuntimeError(f"{name}: linked npm target authority is missing")
    manifest = json.loads(target_manifest.read_text(encoding="utf-8"))
    authority = exact_authority(
        authorities, "npm", name, target_entry.get("version"), str(location)
    )
    for key, value in (
        ("name", authority.name),
        ("version", authority.version),
        ("license", SOFTWARE_LICENSE),
    ):
        if target_entry.get(key) != value or not isinstance(
            manifest, dict
        ) or manifest.get(key) != value:
            raise RuntimeError(f"{name}: linked npm target {key} does not match authority")
    root_package = packages.get("")
    peer_dependencies = (
        root_package.get("peerDependencies") if isinstance(root_package, dict) else None
    )
    if (
        not isinstance(peer_dependencies, dict)
        or peer_dependencies.get(name) != authority.version
    ):
        raise RuntimeError(f"{name}: linked npm peer dependency is not exact")
    return authority


def expected_npm_identities(
    root: Path,
    authorities: dict[tuple[str, str, str], PackageAuthority],
) -> list[ArtifactIdentity]:
    identities: list[ArtifactIdentity] = []
    for path in sorted(root.rglob("package-lock.json")):
        if IGNORED_DIRECTORIES.intersection(path.parts):
            continue
        manifest = path.with_name("package.json")
        relative_lock = path.resolve().relative_to(root.resolve()).as_posix()
        tool = PRIVATE_NPM_TOOLS_BY_LOCK.get(relative_lock)
        if tool is not None:
            if not manifest.is_file():
                raise RuntimeError(f"{path}: private npm tool manifest is missing")
            document = json.loads(manifest.read_text(encoding="utf-8"))
            if not isinstance(document, dict):
                raise RuntimeError(f"{manifest}: package manifest must be an object")
            if private_npm_tool_for_manifest(manifest, document, root) != tool:
                raise RuntimeError(f"{path}: private npm tool inventory does not match")
            version = require_string(
                document.get("version"), f"{manifest}: package version"
            )
            validate_private_npm_tool_lock(path, tool, version)
            continue
        packages = npm_lock_packages(path)
        for key, package in packages.items():
            if key != "" and "node_modules/" not in key:
                continue
            if not isinstance(package, dict):
                raise RuntimeError(f"{path}: npm package entry must be an object")
            name = package_name_from_lock_key(key, package)
            if not is_hyphae_component(name):
                continue
            if package.get("link") is True:
                authority = linked_npm_authority_from_lock(
                    name, package, packages, path, root, authorities
                )
            else:
                authority = exact_authority(
                    authorities, "npm", name, package.get("version"), str(path)
                )
                if package.get("license") != SOFTWARE_LICENSE:
                    raise RuntimeError(
                        f"{path}: {authority.name} lock license must be {SOFTWARE_LICENSE}"
                    )
            identities.append(
                ArtifactIdentity(
                    package_type="npm",
                    name=authority.name,
                    version=authority.version,
                    purl=authority.purl,
                    location=lock_location(root, path),
                )
            )
    return identities


def expected_artifact_identities(
    root: Path,
    authorities: dict[tuple[str, str, str], PackageAuthority],
) -> Counter[ArtifactIdentity]:
    return Counter(
        [
            *expected_cargo_identities(root, authorities),
            *expected_npm_identities(root, authorities),
            *(
                ArtifactIdentity(
                    package_type=authority.package_type,
                    name=authority.name,
                    version=authority.version,
                    purl=authority.purl,
                    location=lock_location(root, authority.manifest),
                )
                for authority in authorities.values()
                if authority.package_type == "python"
            ),
        ]
    )


def python_manifest_artifact(
    authority: PackageAuthority, root: Path
) -> dict[str, Any]:
    document = tomllib.loads(authority.manifest.read_text(encoding="utf-8"))
    project = document.get("project")
    dependencies = project.get("dependencies") if isinstance(project, dict) else None
    if dependencies != []:
        raise RuntimeError(
            f"{authority.manifest}: Python SBOM supplement requires no dependencies"
        )
    location = lock_location(root, authority.manifest)
    identifier = hashlib.sha256(
        f"hyphae-manifest:python:{authority.name}@{authority.version}:{location}".encode(
            "utf-8"
        )
    ).hexdigest()[:16]
    return {
        "id": identifier,
        "name": authority.name,
        "version": authority.version,
        "type": "python",
        "foundBy": "hyphae-manifest-cataloger",
        "locations": [
            {
                "path": location,
                "accessPath": location,
                "annotations": {"evidence": "primary"},
            }
        ],
        "licenses": [],
        "language": "python",
        "cpes": [],
        "purl": authority.purl,
    }


def supplement_python_artifacts(
    document: dict[str, Any],
    authorities: dict[tuple[str, str, str], PackageAuthority],
    root: Path,
) -> None:
    artifacts = document["artifacts"]
    observed = {
        (artifact.get("type"), artifact.get("name"), artifact.get("version"))
        for artifact in artifacts
        if isinstance(artifact, dict)
    }
    for authority in sorted(authorities.values(), key=lambda value: value.purl):
        identity = (authority.package_type, authority.name, authority.version)
        if authority.package_type == "python" and identity not in observed:
            artifacts.append(python_manifest_artifact(authority, root))


def validate_python_artifact(
    artifact: dict[str, Any], location: Path, authority: PackageAuthority
) -> None:
    if location.resolve() != authority.manifest.resolve() or artifact.get(
        "foundBy"
    ) not in {"python-package-cataloger", "hyphae-manifest-cataloger"}:
        raise RuntimeError(
            f"{authority.name}: Python artifact lacks exact pyproject authority"
        )


def validate_existing_licenses(artifact: dict[str, Any]) -> None:
    licenses = artifact.get("licenses")
    if not isinstance(licenses, list):
        raise RuntimeError(f"{artifact.get('name')}: licenses must be a list")
    for license_value in licenses:
        if not isinstance(license_value, dict):
            raise RuntimeError(f"{artifact.get('name')}: license must be an object")
        if license_value.get("value") != SOFTWARE_LICENSE or license_value.get(
            "spdxExpression"
        ) != SOFTWARE_LICENSE:
            raise RuntimeError(f"{artifact.get('name')}: conflicting observed license")
        if license_value.get("type") not in {"declared", "concluded"}:
            raise RuntimeError(f"{artifact.get('name')}: unsupported observed license type")


def conclude_artifact_license(
    artifact: dict[str, Any], authority: PackageAuthority, root: Path
) -> None:
    validate_existing_licenses(artifact)
    relative = authority.manifest.resolve().relative_to(root.resolve()).as_posix()
    evidence = {
        "path": f"/{relative}",
        "accessPath": f"/{relative}",
        "annotations": {"evidence": "primary"},
    }
    artifact["licenses"] = [
        {
            "value": SOFTWARE_LICENSE,
            "spdxExpression": SOFTWARE_LICENSE,
            "type": license_type,
            "urls": [],
            "locations": [evidence],
        }
        for license_type in ("declared", "concluded")
    ]


def conclude_document(document: object, root: Path) -> int:
    if not isinstance(document, dict):
        raise RuntimeError("Syft JSON must be an object")
    descriptor = document.get("descriptor")
    if (
        not isinstance(descriptor, dict)
        or descriptor.get("name") != "syft"
        or descriptor.get("version") != SYFT_VERSION
    ):
        raise RuntimeError(f"Syft JSON descriptor must identify syft {SYFT_VERSION}")
    if not isinstance(document.get("artifacts"), list):
        raise RuntimeError("Syft JSON artifacts must be a list")
    authorities = discover_package_authorities(root)
    supplement_python_artifacts(document, authorities, root)
    expected = expected_artifact_identities(root, authorities)
    observed: Counter[ArtifactIdentity] = Counter()
    retained_artifacts: list[dict[str, Any]] = []
    excluded_artifact_ids: set[str] = set()
    concluded = 0
    for artifact in document["artifacts"]:
        if not isinstance(artifact, dict):
            raise RuntimeError("Syft artifact must be an object")
        if artifact.get("name") in PRIVATE_NPM_TOOLS_BY_NAME:
            _, location = artifact_location(artifact, root)
            if private_npm_tool_for_artifact(artifact, location, root) is not None:
                artifact_id = require_string(
                    artifact.get("id"), f"{artifact.get('name')}: artifact ID"
                )
                if artifact_id in excluded_artifact_ids:
                    raise RuntimeError(
                        f"{artifact.get('name')}: duplicate private artifact ID"
                    )
                excluded_artifact_ids.add(artifact_id)
                continue
        if not is_hyphae_component(artifact.get("name")):
            retained_artifacts.append(artifact)
            continue
        raw_location, location = artifact_location(artifact, root)
        metadata = artifact.get("metadata")
        is_npm_link = (
            artifact.get("type") == "npm"
            and isinstance(metadata, dict)
            and isinstance(metadata.get("resolved"), str)
            and bool(metadata["resolved"])
        )
        if is_npm_link:
            authority = authority_for_linked_npm(
                artifact, root, location, authorities
            )
            canonicalize_linked_npm(artifact, authority)
        else:
            authority = authority_for_exact_artifact(artifact, authorities)
            if authority.package_type == "rust-crate":
                validate_rust_artifact(artifact, location)
            elif authority.package_type == "npm":
                validate_exact_npm_artifact(artifact, location, authority)
            elif authority.package_type == "python":
                validate_python_artifact(artifact, location, authority)
            else:
                raise RuntimeError(
                    f"{authority.name}: unsupported observed first-party package type"
                )
        conclude_artifact_license(artifact, authority, root)
        observed[
            ArtifactIdentity(
                package_type=authority.package_type,
                name=authority.name,
                version=authority.version,
                purl=authority.purl,
                location=raw_location,
            )
        ] += 1
        retained_artifacts.append(artifact)
        concluded += 1
    if observed != expected:
        missing = sorted((expected - observed).elements(), key=repr)
        unexpected = sorted((observed - expected).elements(), key=repr)
        raise RuntimeError(
            "Syft first-party artifact inventory differs from lock authority: "
            f"missing={missing!r}, unexpected={unexpected!r}"
        )
    if excluded_artifact_ids:
        if any(
            artifact.get("id") in excluded_artifact_ids
            for artifact in retained_artifacts
        ):
            raise RuntimeError("private npm tool artifact ID is not unique")
        relationships = document.get("artifactRelationships")
        if not isinstance(relationships, list) or not all(
            isinstance(relationship, dict) for relationship in relationships
        ):
            raise RuntimeError("Syft artifact relationships must be a list")
        document["artifactRelationships"] = [
            relationship
            for relationship in relationships
            if relationship.get("parent") not in excluded_artifact_ids
            and relationship.get("child") not in excluded_artifact_ids
        ]
        document["artifacts"] = retained_artifacts
    return concluded


def write_json_atomically(path: Path, document: object) -> None:
    encoded = (
        json.dumps(document, separators=(",", ":"), ensure_ascii=False) + "\n"
    ).encode("utf-8")
    descriptor = tempfile.NamedTemporaryFile(
        mode="wb",
        prefix=f".{path.name}.",
        suffix=".tmp",
        dir=path.parent,
        delete=False,
    )
    temporary = Path(descriptor.name)
    try:
        with descriptor:
            descriptor.write(encoded)
            descriptor.flush()
            os.fsync(descriptor.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def conclude_file(path: Path, root: Path) -> int:
    document = json.loads(path.read_text(encoding="utf-8"))
    count = conclude_document(document, root.resolve())
    write_json_atomically(path, document)
    return count


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--root", type=Path, default=ROOT)
    arguments = parser.parse_args()
    try:
        count = conclude_file(arguments.input, arguments.root)
    except (
        OSError,
        UnicodeError,
        json.JSONDecodeError,
        tomllib.TOMLDecodeError,
        RuntimeError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(
        f"concluded {count} manifest-backed Hyphae artifact licenses as "
        f"{SOFTWARE_LICENSE}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
