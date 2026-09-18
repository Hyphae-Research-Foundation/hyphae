#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Validate a PostgreSQL Application Core v1 oracle receipt against its profile."""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tools.check_postgresql_application_core import (
    CASES_DIRECTORY,
    GateFailure,
    exact_fields,
    mapping,
    resolve_contained_file,
    validate_profile,
)


DEFAULT_PROFILE = ROOT / "conformance/postgresql/application-core-v1.json"
RECEIPT_SCHEMA = "hyphae-postgresql-application-core-receipt-v2"
RECEIPT_EVIDENCE_CLASS = "operator-observation-supporting-only"
ADMISSION_SCHEMA = "hyphae-postgresql-operator-admission-v1"
ADMISSION_KIND = "operator-attested-non-claim-authoritative"
ADMISSION_STATEMENT = (
    "I initiated this external PostgreSQL oracle observation and acknowledge that its receipt "
    "is supporting evidence only and cannot authorize a Hyphae compatibility claim."
)
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
OCI_DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
GIT_OBJECT = re.compile(r"[0-9a-f]{40}\Z")
MAX_OCI_DOCUMENT_BYTES = 1024 * 1024
INDEX_MEDIA_TYPES = {
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.index.v1+json",
}
MANIFEST_MEDIA_TYPES = {
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
}
CONFIG_MEDIA_TYPES = {
    "application/vnd.docker.container.image.v1+json",
    "application/vnd.oci.image.config.v1+json",
}
CODE_AUTHORITY_PATHS = {
    "runner": "conformance/postgresql/run.py",
    "profile_checker": "tools/check_postgresql_application_core.py",
    "receipt_validator": "tools/check_postgresql_application_core_receipt.py",
}


def canonical_sha256(value: object) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def build_execution_admission(operator: str, accepted: bool) -> dict[str, object]:
    if (
        accepted is not True
        or not isinstance(operator, str)
        or not operator.strip()
        or operator != operator.strip()
        or len(operator) > 200
        or any(character.isspace() and character != " " for character in operator)
    ):
        raise GateFailure("an explicit bounded operator non-claim attestation is required")
    admission: dict[str, object] = {
        "schema": ADMISSION_SCHEMA,
        "kind": ADMISSION_KIND,
        "operator": operator,
        "statement": ADMISSION_STATEMENT,
        "claim_authority": False,
    }
    admission["attestation_sha256"] = canonical_sha256(admission)
    return admission


def validate_execution_admission(raw_admission: object) -> dict[str, object]:
    admission = mapping(raw_admission, "execution admission")
    exact_fields(
        admission,
        {
            "schema",
            "kind",
            "operator",
            "statement",
            "claim_authority",
            "attestation_sha256",
        },
        "execution admission",
    )
    expected = build_execution_admission(admission.get("operator"), True)
    if admission != expected:
        raise GateFailure("execution admission is malformed or tampered")
    return admission


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def code_authorities(root: Path) -> dict[str, dict[str, str]]:
    return {
        name: {
            "path": relative,
            "sha256": sha256_file(
                resolve_contained_file(root, relative, Path("."), f"{name} authority")
            ),
        }
        for name, relative in CODE_AUTHORITY_PATHS.items()
    }


def git_output(root: Path, *arguments: str, environment: dict[str, str] | None = None) -> str:
    completed = subprocess.run(
        ["git", *arguments],
        cwd=root,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        raise GateFailure(f"git source identity failed: {completed.stderr.strip()}")
    return completed.stdout.strip()


def repository_source_identity(root: Path) -> dict[str, str]:
    commit = git_output(root, "rev-parse", "HEAD^{commit}")
    if GIT_OBJECT.fullmatch(commit) is None:
        raise GateFailure("repository source commit is not canonical")
    status = git_output(root, "status", "--porcelain=v1", "--untracked-files=all")
    if not status:
        tree = git_output(root, "rev-parse", "HEAD^{tree}")
        mode = "clean"
    else:
        with tempfile.TemporaryDirectory(prefix="hyphae-postgresql-source-") as directory:
            environment = {**os.environ, "GIT_INDEX_FILE": str(Path(directory) / "index")}
            git_output(root, "read-tree", "HEAD", environment=environment)
            git_output(root, "add", "--all", environment=environment)
            tree = git_output(root, "write-tree", environment=environment)
        mode = "integration"
    if GIT_OBJECT.fullmatch(tree) is None:
        raise GateFailure("repository source tree is not canonical")
    return {"commit": commit, "tree": tree, "mode": mode}


def authenticated_oci_document(encoded: object, digest: object, size: object, label: str) -> dict[str, Any]:
    if (
        not isinstance(encoded, str)
        or not isinstance(digest, str)
        or OCI_DIGEST.fullmatch(digest) is None
        or type(size) is not int
        or size <= 0
        or size > MAX_OCI_DOCUMENT_BYTES
    ):
        raise GateFailure(f"{label} OCI document metadata is malformed")
    try:
        raw = base64.b64decode(encoded, validate=True)
    except (ValueError, binascii.Error) as error:
        raise GateFailure(f"{label} OCI document encoding is invalid") from error
    if len(raw) != size or hashlib.sha256(raw).hexdigest() != digest.removeprefix("sha256:"):
        raise GateFailure(f"{label} OCI document bytes do not match its digest and size")
    try:
        document = json.loads(raw)
    except json.JSONDecodeError as error:
        raise GateFailure(f"{label} OCI document is not JSON") from error
    return mapping(document, f"{label} OCI document")


def validate_oci_container(
    raw_container: object, expected_container: dict[str, Any]
) -> tuple[str, str]:
    container = mapping(raw_container, "receipt container")
    exact_fields(
        container,
        {
            "reference",
            "registry",
            "repository",
            "index",
            "resolved_manifest",
            "image_digest",
            "platform",
        },
        "receipt container",
    )
    if (
        container.get("reference") != expected_container["reference"]
        or container.get("registry") != expected_container["registry"]
        or container.get("repository") != expected_container["repository"]
    ):
        raise GateFailure("receipt container authority differs from the pinned profile")

    index = mapping(container["index"], "receipt OCI index")
    exact_fields(index, {"digest", "media_type", "size", "document_base64"}, "receipt OCI index")
    if index.get("digest") != expected_container["manifest_digest"]:
        raise GateFailure("receipt OCI index digest differs from the pinned profile")
    index_document = authenticated_oci_document(
        index.get("document_base64"), index.get("digest"), index.get("size"), "index"
    )
    if (
        index_document.get("schemaVersion") != 2
        or index_document.get("mediaType") not in INDEX_MEDIA_TYPES
        or index.get("media_type") != index_document.get("mediaType")
        or not isinstance(index_document.get("manifests"), list)
    ):
        raise GateFailure("receipt OCI index metadata is invalid")

    resolved = mapping(container["resolved_manifest"], "receipt resolved manifest")
    exact_fields(
        resolved,
        {"digest", "media_type", "size", "document_base64", "config"},
        "receipt resolved manifest",
    )
    resolved_digest = resolved.get("digest")
    if (
        not isinstance(resolved_digest, str)
        or OCI_DIGEST.fullmatch(resolved_digest) is None
        or resolved_digest == index["digest"]
        or resolved.get("media_type") not in MANIFEST_MEDIA_TYPES
        or type(resolved.get("size")) is not int
    ):
        raise GateFailure("receipt resolved manifest identity is malformed")

    platform = mapping(container["platform"], "receipt resolved platform")
    if set(platform) not in ({"os", "architecture"}, {"os", "architecture", "variant"}) or any(
        not isinstance(value, str) or not value for value in platform.values()
    ):
        raise GateFailure("receipt resolved platform is malformed")
    members = [
        descriptor
        for descriptor in index_document["manifests"]
        if isinstance(descriptor, dict) and descriptor.get("digest") == resolved_digest
    ]
    if len(members) != 1:
        raise GateFailure("resolved child manifest is not a unique member of the pinned OCI index")
    member = members[0]
    if (
        member.get("mediaType") != resolved["media_type"]
        or member.get("size") != resolved["size"]
        or member.get("platform") != platform
    ):
        raise GateFailure("resolved child descriptor differs from its pinned index member")

    manifest_document = authenticated_oci_document(
        resolved.get("document_base64"), resolved_digest, resolved.get("size"), "child manifest"
    )
    if (
        manifest_document.get("schemaVersion") != 2
        or manifest_document.get("mediaType") != resolved["media_type"]
    ):
        raise GateFailure("resolved child manifest metadata is invalid")
    config = mapping(resolved["config"], "receipt image config descriptor")
    exact_fields(config, {"digest", "media_type", "size"}, "receipt image config descriptor")
    manifest_config = mapping(manifest_document.get("config"), "child manifest config descriptor")
    if (
        config.get("digest") != manifest_config.get("digest")
        or config.get("media_type") != manifest_config.get("mediaType")
        or config.get("size") != manifest_config.get("size")
        or config.get("media_type") not in CONFIG_MEDIA_TYPES
        or not isinstance(config.get("digest"), str)
        or OCI_DIGEST.fullmatch(config["digest"]) is None
        or type(config.get("size")) is not int
        or config["size"] <= 0
        or config["size"] > MAX_OCI_DOCUMENT_BYTES
        or container.get("image_digest") != config.get("digest")
    ):
        raise GateFailure("child manifest config is not bound to the recorded local image identity")
    return resolved_digest, config["digest"]


def expected_observation(postgresql: dict[str, Any]) -> dict[str, object]:
    configuration = mapping(postgresql["configuration"], "PostgreSQL configuration")
    return {
        "server_version_num": postgresql["server_version_num"],
        "server_encoding": configuration["server_encoding"],
        "client_encoding": configuration["client_encoding"],
        "lc_collate": configuration["lc_collate"],
        "lc_ctype": configuration["lc_ctype"],
        "timezone": configuration["timezone"],
        "settings": configuration["settings"],
    }


def validate_receipt(
    root: Path, profile_path: Path, raw_receipt: object
) -> dict[str, object]:
    profile_bytes = profile_path.read_bytes()
    profile = mapping(json.loads(profile_bytes), "profile")
    audit = validate_profile(root, profile)
    receipt = mapping(raw_receipt, "receipt")
    exact_fields(
        receipt,
        {
            "schema",
            "status",
            "profile_sha256",
            "postgresql_version",
            "evidence_class",
            "claim_authority",
            "claims",
            "closure_declared",
            "execution_admission",
            "source",
            "code_authorities",
            "host",
            "container",
            "configuration",
            "observed_configuration",
            "case_count",
            "passed_count",
            "profile_claim_status",
            "cleanup",
            "results",
        },
        "receipt",
    )
    expected_profile_sha256 = hashlib.sha256(profile_bytes).hexdigest()
    if (
        receipt.get("schema") != RECEIPT_SCHEMA
        or receipt.get("profile_sha256") != expected_profile_sha256
        or receipt.get("postgresql_version") != profile["postgresql"]["version"]
        or receipt.get("profile_claim_status") != audit["parity_status"]
        or receipt.get("cleanup") != "container-removed"
        or receipt.get("evidence_class") != RECEIPT_EVIDENCE_CLASS
        or receipt.get("claim_authority") is not False
        or receipt.get("claims") != []
        or receipt.get("closure_declared") is not False
    ):
        raise GateFailure("receipt identity, profile binding, parity state, or cleanup is invalid")
    validate_execution_admission(receipt["execution_admission"])

    source = mapping(receipt["source"], "receipt source")
    exact_fields(source, {"commit", "tree", "mode"}, "receipt source")
    if source != repository_source_identity(root):
        raise GateFailure("receipt repository source identity differs from the current source")
    if receipt.get("code_authorities") != code_authorities(root):
        raise GateFailure("receipt runner or checker bytes differ from current authorities")

    host = mapping(receipt["host"], "receipt host")
    exact_fields(host, {"os", "architecture"}, "receipt host")
    if any(not isinstance(host.get(field), str) or not host[field] for field in host):
        raise GateFailure("receipt host operating system and architecture must be nonempty")

    resolved_manifest, image_digest = validate_oci_container(
        receipt["container"], profile["postgresql"]["container"]
    )

    configuration = profile["postgresql"]["configuration"]
    if receipt.get("configuration") != configuration:
        raise GateFailure("receipt configuration differs from the pinned profile")
    if receipt.get("observed_configuration") != expected_observation(profile["postgresql"]):
        raise GateFailure("receipt observed PostgreSQL settings differ from the pinned profile")

    results = receipt.get("results")
    if not isinstance(results, list):
        raise GateFailure("receipt results must be an array")
    expected_cases = profile["cases"]
    if len(results) != len(expected_cases):
        raise GateFailure("receipt result inventory differs from the profile")
    passed_count = 0
    for expected_case, raw_result in zip(expected_cases, results, strict=True):
        result = mapping(raw_result, f"receipt result {expected_case['id']}")
        exact_fields(
            result,
            {
                "id",
                "status",
                "exit_code",
                "oracle_sha256",
                "stdout",
                "stdout_sha256",
                "diagnostic",
                "diagnostic_sha256",
            },
            f"receipt result {expected_case['id']}",
        )
        oracle_path = resolve_contained_file(
            root,
            expected_case["oracle"],
            CASES_DIRECTORY,
            f"case {expected_case['id']} receipt oracle",
        )
        oracle_sha256 = sha256_file(oracle_path)
        stdout = result.get("stdout")
        stdout_sha256 = result.get("stdout_sha256")
        exit_code = result.get("exit_code")
        diagnostic = result.get("diagnostic")
        diagnostic_sha256 = result.get("diagnostic_sha256")
        if (
            result.get("id") != expected_case["id"]
            or result.get("oracle_sha256") != expected_case["oracle_sha256"]
            or result.get("oracle_sha256") != oracle_sha256
            or type(exit_code) is not int
            or not isinstance(stdout, str)
            or not isinstance(stdout_sha256, str)
            or SHA256.fullmatch(stdout_sha256) is None
            or hashlib.sha256(stdout.encode()).hexdigest() != stdout_sha256
            or not isinstance(diagnostic, str)
            or not isinstance(diagnostic_sha256, str)
            or SHA256.fullmatch(diagnostic_sha256) is None
            or hashlib.sha256(diagnostic.encode()).hexdigest() != diagnostic_sha256
        ):
            raise GateFailure(f"receipt result {expected_case['id']} is malformed or tampered")
        lines = [line.strip() for line in stdout.splitlines() if line.strip()]
        expected_pass = exit_code == 0 and lines[-1:] == [f"ok:{expected_case['id']}"]
        status = "observed-passed" if expected_pass else "observed-failed"
        if result.get("status") != status or (expected_pass and diagnostic):
            raise GateFailure(f"receipt result {expected_case['id']} status is inconsistent")
        passed_count += int(expected_pass)

    expected_status = "observed-passed" if passed_count == len(results) else "observed-failed"
    if (
        type(receipt.get("case_count")) is not int
        or receipt["case_count"] != len(results)
        or type(receipt.get("passed_count")) is not int
        or receipt["passed_count"] != passed_count
        or receipt.get("status") != expected_status
    ):
        raise GateFailure("receipt aggregate counts or status are inconsistent")
    return {
        "schema": "hyphae-postgresql-application-core-receipt-audit-v2",
        "status": "validated-non-claim-authoritative",
        "observation_status": expected_status,
        "evidence_class": RECEIPT_EVIDENCE_CLASS,
        "claim_authority": False,
        "claims": [],
        "closure_declared": False,
        "case_count": len(results),
        "passed_count": passed_count,
        "profile_sha256": expected_profile_sha256,
        "source_tree": source["tree"],
        "resolved_manifest_digest": resolved_manifest,
        "image_digest": image_digest,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, default=DEFAULT_PROFILE)
    parser.add_argument("--receipt", type=Path, required=True)
    args = parser.parse_args()
    profile_path = args.profile if args.profile.is_absolute() else ROOT / args.profile
    try:
        audit = validate_receipt(
            ROOT,
            profile_path,
            json.loads(args.receipt.read_text(encoding="utf-8")),
        )
    except (
        OSError,
        subprocess.SubprocessError,
        tomllib.TOMLDecodeError,
        json.JSONDecodeError,
        GateFailure,
    ) as error:
        print(f"PostgreSQL Application Core v1 receipt failed: {error}", file=sys.stderr)
        return 2
    print(json.dumps(audit, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
