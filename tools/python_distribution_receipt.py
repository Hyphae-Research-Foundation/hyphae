#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Build and verify exact-source Python publication receipts."""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import os
import re
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any

PROJECT = "hyphae-sdk"
SCHEMA = "hyphae-python-distribution-receipt-v2"
WORKFLOW_REPOSITORY = "Hyphae-Research-Foundation/hyphae"
WORKFLOW_REF = "Hyphae-Research-Foundation/hyphae/.github/workflows/python-publish.yml@refs/heads/main"
PUBLISHER_WORKFLOW = "python-publish.yml"
WORKFLOW_PATH = ".github/workflows/python-publish.yml"
SUPPORTED_PYTHON = ("3.11", "3.14")
SUPPORTED_INSTALLATION_MATRIX = (
    ("3.11", "wheel"),
    ("3.11", "sdist"),
    ("3.14", "wheel"),
    ("3.14", "sdist"),
)
INSTALLATION_EVIDENCE_SCHEMA = "hyphae-python-installation-evidence-v1"
PUBLISH_PREDICATE = "https://docs.pypi.org/attestations/publish/v1"
PYPI_ATTESTATIONS_VERSION = "0.0.30"
CRYPTOGRAPHIC_VERIFIER = {
    "name": "pypi-attestations",
    "version": PYPI_ATTESTATIONS_VERSION,
    "mode": "pep740-local-artifact-and-provenance",
}
MAX_REGISTRY_RESPONSE_BYTES = 4 * 1024 * 1024
MAX_LOCAL_JSON_BYTES = 1024 * 1024
REPOSITORIES = {
    "pypi": {
        "release": "https://pypi.org/pypi/hyphae-sdk",
        "integrity": "https://pypi.org/integrity",
    },
    "testpypi": {
        "release": "https://test.pypi.org/pypi/hyphae-sdk",
        "integrity": "https://test.pypi.org/integrity",
    },
}


class PythonReceiptError(ValueError):
    """A Python distribution receipt or registry response is invalid."""


CryptographicVerifier = Callable[[Path, object, str], dict[str, str]]


def fail(message: str) -> None:
    raise PythonReceiptError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _read_bounded_file(path: Path, label: str) -> bytes:
    with path.open("rb") as stream:
        payload = stream.read(MAX_LOCAL_JSON_BYTES + 1)
    if len(payload) > MAX_LOCAL_JSON_BYTES:
        fail(f"{label} exceeds the bounded JSON input limit")
    return payload


def _reject_duplicate_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, item in pairs:
        if key in value:
            fail(f"JSON contains a duplicate object key: {key}")
        value[key] = item
    return value


def _reject_non_json_constant(value: str) -> object:
    fail(f"JSON contains a non-standard numeric constant: {value}")


def _load_json_bytes(payload: bytes, label: str, *, limit: int) -> object:
    if len(payload) > limit:
        fail(f"{label} exceeds the bounded JSON input limit")
    try:
        return json.loads(
            payload,
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_non_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        fail(f"{label} is not valid UTF-8 JSON: {error}")


def load_local_json(path: Path, label: str) -> object:
    """Load bounded local JSON while rejecting duplicate keys recursively."""
    return _load_json_bytes(
        _read_bounded_file(path, label), label, limit=MAX_LOCAL_JSON_BYTES
    )


def _is_sha(value: object, digits: int) -> bool:
    return (
        isinstance(value, str)
        and re.fullmatch(rf"[0-9a-f]{{{digits}}}", value) is not None
    )


def _positive_integer(value: object) -> bool:
    return not isinstance(value, bool) and isinstance(value, int) and value > 0


def _distribution_inventory(
    directory: Path, version: str, *, allowed_auxiliary: tuple[str, ...] = ()
) -> dict[str, dict[str, object]]:
    files = sorted(
        path
        for path in directory.iterdir()
        if path.is_file()
        and path.name != ".gitignore"
        and path.name not in allowed_auxiliary
    )
    expected = {
        "wheel": f"hyphae_sdk-{version}-py3-none-any.whl",
        "sdist": f"hyphae_sdk-{version}.tar.gz",
    }
    by_name = {path.name: path for path in files}
    if set(by_name) != set(expected.values()):
        fail("receipt input must contain the exact wheel and sdist")
    return {
        kind: {
            "filename": filename,
            "sha256": sha256(by_name[filename]),
            "bytes": by_name[filename].stat().st_size,
        }
        for kind, filename in expected.items()
    }


def _distribution_records(
    distributions: dict[str, dict[str, object]],
) -> list[dict[str, object]]:
    return sorted(distributions.values(), key=lambda entry: str(entry["filename"]))


def _exact_object(value: object, keys: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        fail(f"{label} fields are invalid")
    return value


def _workflow_artifact(value: object, expected_name: str) -> dict[str, object]:
    artifact = _exact_object(value, {"id", "name", "digest"}, "workflow artifact")
    digest = artifact.get("digest")
    if (
        not _positive_integer(artifact.get("id"))
        or artifact.get("name") != expected_name
        or not isinstance(digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None
    ):
        fail("workflow artifact identity is invalid")
    return artifact


def _file_digest(value: object, expected_filename: str, label: str) -> dict[str, str]:
    evidence = _exact_object(value, {"filename", "sha256"}, label)
    if evidence.get("filename") != expected_filename or not _is_sha(
        evidence.get("sha256"), 64
    ):
        fail(f"{label} identity is invalid")
    return evidence


def _authority_run(
    value: object,
    *,
    workflow: str,
    event: str,
    branch: str,
    commit: str,
) -> dict[str, object]:
    run = _exact_object(
        value,
        {
            "id",
            "attempt",
            "event",
            "status",
            "conclusion",
            "head_branch",
            "head_sha",
            "path",
            "repository",
        },
        "publication authority run",
    )
    if (
        run
        != {
            "id": run.get("id"),
            "attempt": run.get("attempt"),
            "event": event,
            "status": "completed",
            "conclusion": "success",
            "head_branch": branch,
            "head_sha": commit,
            "path": workflow,
            "repository": WORKFLOW_REPOSITORY,
        }
        or not _positive_integer(run["id"])
        or not _positive_integer(run["attempt"])
    ):
        fail("publication authority run is not exact and successful")
    return run


def _validated_builder_receipt(
    path: Path,
    *,
    expected_builder: str,
    source: dict[str, object],
    version: str,
    distributions: dict[str, dict[str, object]],
) -> dict[str, object]:
    receipt = _exact_object(
        load_local_json(path, f"independent builder {expected_builder} receipt"),
        {
            "schema",
            "builder",
            "source",
            "version",
            "toolchain",
            "runner",
            "distributions",
        },
        f"independent builder {expected_builder} receipt",
    )
    toolchain = _exact_object(
        receipt.get("toolchain"), {"python", "uv"}, "independent build toolchain"
    )
    runner = _exact_object(
        receipt.get("runner"),
        {"os", "arch", "image_os", "image_version"},
        "independent build runner",
    )
    if (
        receipt.get("schema") != "hyphae-python-independent-build-v1"
        or receipt.get("builder") != expected_builder
        or receipt.get("source") != source
        or receipt.get("version") != version
        or toolchain.get("python") != "3.11.15"
        or not isinstance(toolchain.get("uv"), str)
        or toolchain["uv"].split()[:2] != ["uv", "0.12.3"]
        or not all(isinstance(value, str) and value for value in runner.values())
        or receipt.get("distributions") != _distribution_records(distributions)
    ):
        fail(f"independent builder {expected_builder} authority differs")
    return receipt


def _embedded_independent_build(
    value: object,
    *,
    expected_builder: str,
    source_tag: str,
    distributions: dict[str, dict[str, object]],
) -> dict[str, object]:
    build = _exact_object(
        value,
        {
            "builder",
            "artifact",
            "receipt_sha256",
            "toolchain",
            "runner",
            "distributions",
        },
        "embedded independent build authority",
    )
    toolchain = _exact_object(
        build.get("toolchain"), {"python", "uv"}, "independent build toolchain"
    )
    runner = _exact_object(
        build.get("runner"),
        {"os", "arch", "image_os", "image_version"},
        "independent build runner",
    )
    if (
        build.get("builder") != expected_builder
        or not _is_sha(build.get("receipt_sha256"), 64)
        or toolchain.get("python") != "3.11.15"
        or not isinstance(toolchain.get("uv"), str)
        or toolchain["uv"].split()[:2] != ["uv", "0.12.3"]
        or not all(isinstance(item, str) and item for item in runner.values())
        or build.get("distributions") != _distribution_records(distributions)
    ):
        fail("embedded independent build authority differs")
    _workflow_artifact(
        build["artifact"],
        f"hyphae-python-independent-{expected_builder}-{source_tag}",
    )
    return build


def _release_authority(
    value: object, source: dict[str, object]
) -> tuple[dict[str, object], tuple[int, int]]:
    release = _exact_object(
        value,
        {"run", "artifact", "release_evidence", "sboms", "g8_closure"},
        "Python release authority",
    )
    tag = str(source["tag"])
    commit = str(source["commit"])
    release_run = _authority_run(
        release["run"],
        workflow=".github/workflows/release.yml",
        event="push",
        branch=tag,
        commit=commit,
    )
    _workflow_artifact(release["artifact"], "hyphae-release-candidate")
    _file_digest(
        release["release_evidence"],
        f"hyphae-{tag}.release-evidence.json",
        "release evidence",
    )
    sboms = _exact_object(release["sboms"], {"spdx", "cyclonedx"}, "release SBOMs")
    _file_digest(sboms["spdx"], f"hyphae-{tag}.spdx.json", "SPDX SBOM")
    _file_digest(sboms["cyclonedx"], f"hyphae-{tag}.cdx.json", "CycloneDX SBOM")
    g8 = _exact_object(
        release["g8_closure"], {"run", "artifact", "aggregate"}, "G8 closure"
    )
    g8_run = _authority_run(
        g8["run"],
        workflow=".github/workflows/native-g8-closure.yml",
        event="workflow_dispatch",
        branch="main",
        commit=commit,
    )
    _workflow_artifact(g8["artifact"], f"native-g8-aggregate-{commit}")
    aggregate = _exact_object(
        g8["aggregate"],
        {"filename", "sha256", "claims", "closure_declared"},
        "G8 aggregate",
    )
    if (
        aggregate.get("filename") != "native-g8-aggregate.json"
        or not _is_sha(aggregate.get("sha256"), 64)
        or aggregate.get("claims") != ["G8"]
        or aggregate.get("closure_declared") is not True
    ):
        fail("G8 aggregate authority is open or source-unbound")
    return release, (release_run["id"], g8_run["id"])


def _publication_authority(
    value: object,
    *,
    source: dict[str, object],
    distributions: dict[str, dict[str, object]],
    repository: str,
    reserved_run_ids: set[int],
    builder_receipts: tuple[tuple[Path, dict[str, object]], ...] | None = None,
) -> dict[str, object]:
    authority = _exact_object(
        value,
        {"schema", "source", "independent_builds", "release_authority"},
        "Python publication authority",
    )
    builds = authority.get("independent_builds")
    if (
        authority.get("schema") != "hyphae-python-publication-authority-v1"
        or authority.get("source") != source
        or not isinstance(builds, list)
        or len(builds) != 2
    ):
        fail("Python publication authority identity differs")
    embedded = tuple(
        _embedded_independent_build(
            builds[index],
            expected_builder=builder,
            source_tag=str(source["tag"]),
            distributions=distributions,
        )
        for index, builder in enumerate(("a", "b"))
    )
    if embedded[0]["toolchain"] != embedded[1]["toolchain"]:
        fail("independent builder toolchains differ")
    if builder_receipts is not None:
        for index, (path, receipt) in enumerate(builder_receipts):
            expected = {
                "builder": receipt["builder"],
                "artifact": embedded[index]["artifact"],
                "receipt_sha256": sha256(path),
                "toolchain": receipt["toolchain"],
                "runner": receipt["runner"],
                "distributions": receipt["distributions"],
            }
            if embedded[index] != expected:
                fail("publication authority differs from independent builder receipts")
    release = authority.get("release_authority")
    if repository == "testpypi":
        if release is not None:
            fail("TestPyPI publication cannot claim signed Release/G8 authority")
    else:
        release, release_run_ids = _release_authority(release, source)
        if len(reserved_run_ids | set(release_run_ids)) != len(reserved_run_ids) + 2:
            fail("publication authority workflow runs must be distinct")
    return authority


def _source(version: str, source_tag: str, commit: str, tree: str) -> dict[str, object]:
    if source_tag != f"v{version}":
        fail("Python version and immutable source tag differ")
    if not _is_sha(commit, 40) or not _is_sha(tree, 40):
        fail("source commit and tree must be full lowercase Git object IDs")
    return {"tag": source_tag, "commit": commit, "tree": tree}


def _run(
    repository: str,
    workflow_ref: str,
    workflow_sha: str,
    run_id: int,
    run_attempt: int,
) -> dict[str, object]:
    if repository != WORKFLOW_REPOSITORY or workflow_ref != WORKFLOW_REF:
        fail("publication must use the canonical main-branch workflow identity")
    if not _is_sha(workflow_sha, 40):
        fail("workflow SHA must be one full lowercase Git object ID")
    if not _positive_integer(run_id) or not _positive_integer(run_attempt):
        fail("workflow run ID and attempt must be positive integers")
    return {
        "repository": repository,
        "workflow_ref": workflow_ref,
        "workflow_sha": workflow_sha,
        "id": run_id,
        "attempt": run_attempt,
    }


def _validated_github_run(
    value: object, receipt_run: dict[str, object]
) -> dict[str, object]:
    if not isinstance(value, dict):
        fail("GitHub TestPyPI workflow run metadata is invalid")
    repository = value.get("repository")
    head_repository = value.get("head_repository")
    if (
        value.get("id") != receipt_run["id"]
        or value.get("run_attempt") != receipt_run["attempt"]
        or value.get("event") != "workflow_dispatch"
        or value.get("status") != "completed"
        or value.get("conclusion") != "success"
        or value.get("head_branch") != "main"
        or value.get("head_sha") != receipt_run["workflow_sha"]
        or value.get("path") != WORKFLOW_PATH
        or not isinstance(repository, dict)
        or repository.get("full_name") != WORKFLOW_REPOSITORY
        or not isinstance(head_repository, dict)
        or head_repository.get("full_name") != WORKFLOW_REPOSITORY
    ):
        fail("GitHub TestPyPI workflow run is not the exact successful main authority")
    return {
        "id": value["id"],
        "attempt": value["run_attempt"],
        "event": value["event"],
        "status": value["status"],
        "conclusion": value["conclusion"],
        "head_branch": value["head_branch"],
        "head_sha": value["head_sha"],
        "path": value["path"],
        "repository": repository["full_name"],
    }


def _validated_authority(
    receipt_bytes: bytes,
    run_metadata: object,
    expected_digest: str,
    expected_run_id: int,
    source: dict[str, object],
    distributions: dict[str, dict[str, object]],
    current_run_id: int,
) -> dict[str, object]:
    if (
        not _is_sha(expected_digest, 64)
        or sha256_bytes(receipt_bytes) != expected_digest
    ):
        fail("TestPyPI authority receipt SHA-256 differs from the requested digest")
    receipt = _load_json_bytes(
        receipt_bytes, "TestPyPI authority receipt", limit=MAX_LOCAL_JSON_BYTES
    )
    validate_receipt(receipt, expected_status="published")
    run = receipt["run"]
    if (
        receipt["registry"] != "testpypi"
        or run["id"] != expected_run_id
        or run["id"] == current_run_id
        or receipt["source"] != source
        or receipt["distributions"] != distributions
    ):
        fail(
            "TestPyPI authority does not bind the exact source, run, and distributions"
        )
    observed_run = _validated_github_run(run_metadata, run)
    return {
        "receipt_sha256": expected_digest,
        "run": run,
        "run_metadata": observed_run,
        "source": receipt["source"],
        "distributions": receipt["distributions"],
    }


def build_receipt(
    directory: Path,
    version: str,
    source_tag: str,
    source_commit: str,
    *,
    reproducible_directory: Path,
    independent_build_receipts: tuple[Path, ...],
    publication_authority: Path,
    source_tree: str,
    workflow_repository: str,
    workflow_ref: str,
    workflow_sha: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    repository: str,
    testpypi_receipt: Path | None = None,
    testpypi_run_metadata: Path | None = None,
    testpypi_receipt_sha256: str | None = None,
    testpypi_run_id: int | None = None,
) -> dict[str, Any]:
    """Build a v2 receipt after two byte-identical distribution builds."""
    if re.fullmatch(r"\d+\.\d+\.\d+", version) is None:
        fail("Python version must be strict semver")
    if repository not in REPOSITORIES:
        fail("unknown Python package repository")
    source = _source(version, source_tag, source_commit, source_tree)
    run = _run(
        workflow_repository,
        workflow_ref,
        workflow_sha,
        workflow_run_id,
        workflow_run_attempt,
    )
    distributions = _distribution_inventory(
        directory, version, allowed_auxiliary=("builder-receipt.json",)
    )
    repeated = _distribution_inventory(
        reproducible_directory,
        version,
        allowed_auxiliary=("builder-receipt.json",),
    )
    if distributions != repeated:
        fail(
            "independent Python distribution builds are not byte-for-byte reproducible"
        )
    if len(independent_build_receipts) != 2:
        fail("exactly two independent builder receipts are required")
    builder_receipts = tuple(
        (
            path,
            _validated_builder_receipt(
                path,
                expected_builder=builder,
                source=source,
                version=version,
                distributions=distributions,
            ),
        )
        for path, builder in zip(independent_build_receipts, ("a", "b"))
    )
    prerequisite = (
        testpypi_receipt,
        testpypi_run_metadata,
        testpypi_receipt_sha256,
        testpypi_run_id,
    )
    testpypi_authority: dict[str, object] | None = None
    if repository == "testpypi":
        if any(value is not None for value in prerequisite):
            fail("TestPyPI publication must not accept a TestPyPI prerequisite")
    else:
        if any(value is None for value in prerequisite):
            fail("PyPI publication requires one exact published TestPyPI receipt")
        assert testpypi_receipt is not None
        assert testpypi_run_metadata is not None
        assert testpypi_receipt_sha256 is not None
        assert testpypi_run_id is not None
        testpypi_authority = _validated_authority(
            _read_bounded_file(testpypi_receipt, "TestPyPI authority receipt"),
            load_local_json(
                testpypi_run_metadata, "GitHub TestPyPI workflow run metadata"
            ),
            testpypi_receipt_sha256,
            testpypi_run_id,
            source,
            distributions,
            workflow_run_id,
        )
    reserved_run_ids = {workflow_run_id}
    if testpypi_authority is not None:
        reserved_run_ids.add(testpypi_authority["run"]["id"])
    publication = _publication_authority(
        load_local_json(publication_authority, "Python publication authority"),
        source=source,
        distributions=distributions,
        repository=repository,
        reserved_run_ids=reserved_run_ids,
        builder_receipts=builder_receipts,
    )
    receipt = {
        "schema": SCHEMA,
        "status": "built",
        "project": PROJECT,
        "version": version,
        "source": source,
        "run": run,
        "registry": repository,
        "distributions": distributions,
        "reproducibility": {"builds": 2, "matched": True},
        "testpypi_authority": testpypi_authority,
        "publication_authority": publication,
        "build_receipt_sha256": None,
        "registry_verification": None,
    }
    validate_receipt(receipt, expected_status="built")
    return receipt


def _validate_distribution(value: object, expected_filename: str) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != {"filename", "sha256", "bytes"}:
        fail("Python distribution receipt file entry is invalid")
    if (
        value.get("filename") != expected_filename
        or not _is_sha(value.get("sha256"), 64)
        or not _positive_integer(value.get("bytes"))
    ):
        fail("Python distribution receipt file identity is invalid")
    return value


def _validate_source(value: object, version: str) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != {"tag", "commit", "tree"}:
        fail("Python distribution source binding is invalid")
    if (
        value.get("tag") != f"v{version}"
        or not _is_sha(value.get("commit"), 40)
        or not _is_sha(value.get("tree"), 40)
    ):
        fail("Python distribution source identity is invalid")
    return value


def _validate_run(value: object) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != {
        "repository",
        "workflow_ref",
        "workflow_sha",
        "id",
        "attempt",
    }:
        fail("Python distribution workflow run binding is invalid")
    _run(
        value.get("repository"),
        value.get("workflow_ref"),
        value.get("workflow_sha"),
        value.get("id"),
        value.get("attempt"),
    )
    return value


def _validate_distributions(
    value: object, version: str
) -> dict[str, dict[str, object]]:
    if not isinstance(value, dict) or set(value) != {"wheel", "sdist"}:
        fail("Python distribution inventory is invalid")
    return {
        "wheel": _validate_distribution(
            value["wheel"], f"hyphae_sdk-{version}-py3-none-any.whl"
        ),
        "sdist": _validate_distribution(value["sdist"], f"hyphae_sdk-{version}.tar.gz"),
    }


def validate_receipt(
    receipt: object, *, expected_status: str | None = None
) -> tuple[str, dict[str, dict[str, object]]]:
    """Fail closed on any malformed v2 publication receipt."""
    required = {
        "schema",
        "status",
        "project",
        "version",
        "source",
        "run",
        "registry",
        "distributions",
        "reproducibility",
        "testpypi_authority",
        "publication_authority",
        "build_receipt_sha256",
        "registry_verification",
    }
    if not isinstance(receipt, dict) or set(receipt) != required:
        fail("Python distribution receipt has unknown or missing fields")
    version = receipt.get("version")
    status = receipt.get("status")
    registry = receipt.get("registry")
    if (
        receipt.get("schema") != SCHEMA
        or receipt.get("project") != PROJECT
        or not isinstance(version, str)
        or re.fullmatch(r"\d+\.\d+\.\d+", version) is None
        or status not in {"built", "published"}
        or registry not in REPOSITORIES
        or (expected_status is not None and status != expected_status)
        or receipt.get("reproducibility") != {"builds": 2, "matched": True}
    ):
        fail("Python distribution receipt identity is invalid")
    source = _validate_source(receipt["source"], version)
    run = _validate_run(receipt["run"])
    distributions = _validate_distributions(receipt["distributions"], version)
    authority = receipt["testpypi_authority"]
    if registry == "testpypi":
        if authority is not None:
            fail("TestPyPI receipt cannot contain a TestPyPI prerequisite")
    elif not isinstance(authority, dict) or set(authority) != {
        "receipt_sha256",
        "run",
        "run_metadata",
        "source",
        "distributions",
    }:
        fail("PyPI receipt requires one exact TestPyPI authority")
    else:
        if not _is_sha(authority["receipt_sha256"], 64):
            fail("TestPyPI authority receipt digest is invalid")
        authority_run = _validate_run(authority["run"])
        authority_metadata = authority["run_metadata"]
        authority_source = _validate_source(authority["source"], version)
        authority_distributions = _validate_distributions(
            authority["distributions"], version
        )
        if (
            authority_run["id"] == run["id"]
            or authority_source != source
            or authority_distributions != distributions
        ):
            fail("TestPyPI authority does not bind this exact publication")
        if not isinstance(authority_metadata, dict) or authority_metadata != {
            "id": authority_run["id"],
            "attempt": authority_run["attempt"],
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "success",
            "head_branch": "main",
            "head_sha": authority_run["workflow_sha"],
            "path": WORKFLOW_PATH,
            "repository": WORKFLOW_REPOSITORY,
        }:
            fail("TestPyPI authority workflow metadata is invalid")
    reserved_run_ids = {run["id"]}
    if isinstance(authority, dict):
        reserved_run_ids.add(authority["run"]["id"])
    _publication_authority(
        receipt["publication_authority"],
        source=source,
        distributions=distributions,
        repository=registry,
        reserved_run_ids=reserved_run_ids,
    )
    build_digest = receipt["build_receipt_sha256"]
    verification = receipt["registry_verification"]
    if status == "built":
        if build_digest is not None or verification is not None:
            fail("built receipt cannot claim registry verification")
    else:
        if not _is_sha(build_digest, 64):
            fail("published receipt must bind its exact build receipt")
        _validate_registry_verification(verification, registry, version, distributions)
    return version, distributions


def check_local_distributions(
    receipt: dict[str, Any],
    directory: Path,
    *,
    source_commit: str,
    source_tree: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    repository: str,
) -> None:
    """Rehash local files and identity immediately before privileged upload."""
    version, expected = validate_receipt(receipt, expected_status="built")
    actual = _distribution_inventory(directory, version)
    if actual != expected:
        fail("local publication files differ from the exact build receipt")
    if (
        receipt["source"]["commit"] != source_commit
        or receipt["source"]["tree"] != source_tree
        or receipt["run"]["id"] != workflow_run_id
        or receipt["run"]["attempt"] != workflow_run_attempt
        or receipt["registry"] != repository
    ):
        fail("local publication identity differs from the exact workflow run")


def _registry_inventory(release: object, version: str) -> dict[str, str]:
    if not isinstance(release, dict):
        fail("registry response must be one JSON object")
    info = release.get("info")
    if (
        not isinstance(info, dict)
        or info.get("name") != PROJECT
        or info.get("version") != version
    ):
        fail("registry project/version differs from the receipt")
    urls = release.get("urls")
    if not isinstance(urls, list):
        fail("registry release file inventory is invalid")
    actual: dict[str, str] = {}
    for entry in urls:
        if not isinstance(entry, dict) or not isinstance(entry.get("digests"), dict):
            fail("registry release file entry is invalid")
        filename = entry.get("filename")
        digest = entry["digests"].get("sha256")
        if (
            not isinstance(filename, str)
            or filename in actual
            or not _is_sha(digest, 64)
        ):
            fail("registry release contains an invalid or duplicate filename")
        actual[filename] = digest
    return actual


def _publisher_identity(publisher: object, repository: str) -> dict[str, str]:
    if not isinstance(publisher, dict):
        fail("PyPI provenance publisher is invalid")
    expected = {
        "kind": "GitHub",
        "repository": WORKFLOW_REPOSITORY,
        "workflow": PUBLISHER_WORKFLOW,
        "environment": repository,
    }
    # The 2.1.0 rehearsal files on TestPyPI carry provenance recorded before
    # the Trusted Publisher was environment-bound, so their publisher reports
    # a null environment permanently. Only the rehearsal registry accepts that
    # legacy shape; the production registry requires the exact environment.
    observed_environment = publisher.get("environment")
    environment_matches = observed_environment == repository or (
        repository == "testpypi" and observed_environment is None
    )
    if not environment_matches or any(
        publisher.get(key) != value
        for key, value in expected.items()
        if key != "environment"
    ):
        fail(
            "PyPI provenance Trusted Publisher identity differs from the release workflow"
        )
    return expected


def _cryptographic_verifier_environment() -> dict[str, str]:
    allowed = (
        "APPDATA",
        "HOME",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "LOCALAPPDATA",
        "NO_PROXY",
        "PATH",
        "SSL_CERT_DIR",
        "SSL_CERT_FILE",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "TMPDIR",
        "USERPROFILE",
        "WINDIR",
    )
    return {name: os.environ[name] for name in allowed if name in os.environ}


def verify_distribution_cryptographically(
    artifact: Path, provenance: object, repository: str
) -> dict[str, str]:
    """Verify one local distribution and Integrity API response with PEP 740."""
    if not artifact.is_file() or repository not in REPOSITORIES:
        fail("PEP 740 cryptographic verification input is invalid")
    with tempfile.TemporaryDirectory(prefix="hyphae-pep740-") as directory:
        provenance_path = Path(directory, "provenance.json")
        try:
            provenance_path.write_text(
                json.dumps(provenance, sort_keys=True, separators=(",", ":")),
                encoding="utf-8",
            )
        except (OSError, TypeError, ValueError):
            fail("PEP 740 cryptographic verification input is invalid")
        command = [
            "uvx",
            "--quiet",
            "--isolated",
            "--no-config",
            "--no-env-file",
            "--no-progress",
            "--default-index",
            "https://pypi.org/simple",
            "--from",
            f"pypi-attestations=={PYPI_ATTESTATIONS_VERSION}",
            "pypi-attestations",
            "verify",
            "pypi",
            "--repository",
            f"https://github.com/{WORKFLOW_REPOSITORY}",
            "--provenance-file",
            str(provenance_path),
        ]
        command.append(str(artifact.resolve()))
        try:
            result = subprocess.run(
                command,
                check=False,
                capture_output=True,
                cwd=directory,
                env=_cryptographic_verifier_environment(),
                timeout=180,
            )
        except (OSError, subprocess.TimeoutExpired):
            fail("PEP 740 cryptographic verification failed")
    if result.returncode != 0:
        fail("PEP 740 cryptographic verification failed")
    return dict(CRYPTOGRAPHIC_VERIFIER)


def _statement_subject(attestation: object, filename: str, digest: str) -> None:
    if not isinstance(attestation, dict):
        fail("PyPI provenance attestation is invalid")
    envelope = attestation.get("envelope")
    if (
        attestation.get("version") != 1
        or not isinstance(attestation.get("verification_material"), dict)
        or not isinstance(envelope, dict)
        or not isinstance(envelope.get("signature"), str)
        or not envelope["signature"]
    ):
        fail("PyPI provenance envelope is invalid")
    statement = envelope.get("statement")
    if not isinstance(statement, str):
        fail("PyPI provenance statement is missing")
    try:
        decoded = base64.b64decode(statement, validate=True)
    except binascii.Error as error:
        fail(f"PyPI provenance statement is not canonical base64 JSON: {error}")
    value = _load_json_bytes(
        decoded, "PyPI provenance statement", limit=MAX_LOCAL_JSON_BYTES
    )
    if (
        not isinstance(value, dict)
        or value.get("_type") != "https://in-toto.io/Statement/v1"
        or value.get("predicateType") != PUBLISH_PREDICATE
        or value.get("predicate") not in (None, {})
        or value.get("subject") != [{"name": filename, "digest": {"sha256": digest}}]
    ):
        fail(
            "PyPI provenance statement subject or predicate differs from the distribution"
        )


def _selected_provenance(
    provenance: object, filename: str, digest: str, repository: str
) -> tuple[dict[str, object], dict[str, object]]:
    if not isinstance(provenance, dict) or set(provenance) != {
        "version",
        "attestation_bundles",
    }:
        fail("PyPI Integrity API response is invalid")
    bundles = provenance.get("attestation_bundles")
    if provenance.get("version") != 1 or not isinstance(bundles, list) or not bundles:
        fail("PyPI Integrity API returned no supported provenance bundle")
    for bundle in bundles:
        if not isinstance(bundle, dict) or set(bundle) != {"publisher", "attestations"}:
            fail("PyPI provenance bundle is invalid")
        try:
            publisher = _publisher_identity(bundle["publisher"], repository)
        except PythonReceiptError:
            continue
        attestations = bundle["attestations"]
        if not isinstance(attestations, list) or not attestations:
            fail("PyPI provenance bundle contains no attestation")
        for attestation in attestations:
            try:
                _statement_subject(attestation, filename, digest)
            except PythonReceiptError:
                continue
            return (
                {
                    "integrity_api_version": 1,
                    "publisher": publisher,
                    "predicate_type": PUBLISH_PREDICATE,
                    "subject": {"filename": filename, "sha256": digest},
                },
                {
                    "version": 1,
                    "attestation_bundles": [
                        {
                            "publisher": bundle["publisher"],
                            "attestations": [attestation],
                        }
                    ],
                },
            )
    fail("PyPI Integrity API has no matching Trusted Publisher attestation")


def verify_provenance(
    provenance: object, filename: str, digest: str, repository: str
) -> dict[str, object]:
    """Validate structural publisher identity and signed-statement subject binding."""
    summary, _ = _selected_provenance(provenance, filename, digest, repository)
    return summary


def _canonical_json_bytes(value: object) -> bytes:
    try:
        return (
            json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"
        ).encode()
    except (TypeError, ValueError):
        fail("installation evidence is not canonical JSON")


def _validate_installation_observation(
    value: object,
    *,
    boundary: str,
    kind: str,
    version: str,
    distributions: dict[str, dict[str, object]],
    label: str,
) -> dict[str, object]:
    evidence = _exact_object(
        value,
        {
            "schema",
            "python_boundary",
            "kind",
            "version",
            "distribution_filename",
            "distribution_sha256",
            "implementation",
            "interpreter_version",
            "status",
        },
        label,
    )
    interpreter_version = evidence.get("interpreter_version")
    distribution = distributions[kind]
    if (
        evidence.get("schema") != INSTALLATION_EVIDENCE_SCHEMA
        or evidence.get("python_boundary") != boundary
        or evidence.get("kind") != kind
        or evidence.get("version") != version
        or evidence.get("distribution_filename") != distribution["filename"]
        or evidence.get("distribution_sha256") != distribution["sha256"]
        or evidence.get("implementation") != "CPython"
        or not isinstance(interpreter_version, str)
        or re.fullmatch(rf"{re.escape(boundary)}\.[0-9]+", interpreter_version) is None
        or evidence.get("status") != "passed"
    ):
        fail(f"{label} does not bind the exact distribution")
    return evidence


def _installation_evidence(
    paths: tuple[Path, ...],
    version: str,
    distributions: dict[str, dict[str, object]],
) -> list[dict[str, object]]:
    if len(paths) != len(SUPPORTED_INSTALLATION_MATRIX):
        fail("exactly four installation evidence files are required")
    observed: dict[tuple[str, str], dict[str, object]] = {}
    evidence_digests: dict[tuple[str, str], str] = {}
    for path in paths:
        try:
            payload = _read_bounded_file(path, "installation evidence")
        except OSError:
            fail("installation evidence is unavailable")
        value = _load_json_bytes(
            payload, "installation evidence", limit=MAX_LOCAL_JSON_BYTES
        )
        if payload != _canonical_json_bytes(value):
            fail("installation evidence must use canonical JSON encoding")
        if not isinstance(value, dict):
            fail("installation evidence fields are invalid")
        boundary = value.get("python_boundary")
        kind = value.get("kind")
        key = (boundary, kind)
        if key not in SUPPORTED_INSTALLATION_MATRIX:
            fail("installation evidence has an unknown Python boundary or kind")
        if key in observed:
            fail("installation evidence contains a duplicate Python boundary and kind")
        evidence = _validate_installation_observation(
            value,
            boundary=boundary,
            kind=kind,
            version=version,
            distributions=distributions,
            label="installation evidence",
        )
        observed[key] = evidence
        evidence_digests[key] = sha256_bytes(payload)
    if set(observed) != set(SUPPORTED_INSTALLATION_MATRIX):
        fail("installation evidence is missing a supported Python boundary or kind")
    return [
        {
            "evidence_sha256": evidence_digests[key],
            "observed": observed[key],
        }
        for key in SUPPORTED_INSTALLATION_MATRIX
    ]


def _validate_registry_verification(
    value: object,
    repository: str,
    version: str,
    distributions: dict[str, dict[str, object]],
) -> None:
    if not isinstance(value, dict) or set(value) != {
        "repository",
        "python_versions",
        "installation_evidence",
        "publication_artifact",
        "provenance",
    }:
        fail("registry verification receipt is invalid")
    if value.get("repository") != repository or value.get("python_versions") != list(
        SUPPORTED_PYTHON
    ):
        fail("registry verification boundaries are invalid")
    retained_evidence = value.get("installation_evidence")
    if not isinstance(retained_evidence, list) or len(retained_evidence) != len(
        SUPPORTED_INSTALLATION_MATRIX
    ):
        fail("registry installation evidence inventory is invalid")
    for index, key in enumerate(SUPPORTED_INSTALLATION_MATRIX):
        retained = _exact_object(
            retained_evidence[index],
            {"evidence_sha256", "observed"},
            "retained installation evidence",
        )
        if not _is_sha(retained.get("evidence_sha256"), 64):
            fail("retained installation evidence digest is invalid")
        evidence = _validate_installation_observation(
            retained.get("observed"),
            boundary=key[0],
            kind=key[1],
            version=version,
            distributions=distributions,
            label="retained installation evidence",
        )
        if retained["evidence_sha256"] != sha256_bytes(_canonical_json_bytes(evidence)):
            fail("retained installation evidence digest differs from its observation")
    artifact = value.get("publication_artifact")
    if (
        not isinstance(artifact, dict)
        or set(artifact) != {"id", "sha256"}
        or not _positive_integer(artifact.get("id"))
        or not _is_sha(artifact.get("sha256"), 64)
    ):
        fail("immutable publication artifact identity is invalid")
    provenance = value.get("provenance")
    if not isinstance(provenance, dict) or set(provenance) != {"wheel", "sdist"}:
        fail("registry provenance inventory is invalid")
    for kind in ("wheel", "sdist"):
        expected = {
            "integrity_api_version": 1,
            "cryptographic_verifier": CRYPTOGRAPHIC_VERIFIER,
            "publisher": {
                "kind": "GitHub",
                "repository": WORKFLOW_REPOSITORY,
                "workflow": PUBLISHER_WORKFLOW,
                "environment": repository,
            },
            "predicate_type": PUBLISH_PREDICATE,
            "subject": {
                "filename": distributions[kind]["filename"],
                "sha256": distributions[kind]["sha256"],
            },
        }
        if provenance.get(kind) != expected:
            fail("registry provenance does not bind the exact distribution")


def verify_registry(
    receipt: dict[str, Any],
    build_receipt_bytes: bytes,
    release: dict[str, Any],
    provenances: dict[str, object],
    repository: str,
    installation_evidence_paths: tuple[Path, ...],
    publication_artifact_id: int,
    publication_artifact_sha256: str,
    *,
    distribution_directory: Path | None = None,
    cryptographic_verifier: CryptographicVerifier | None = None,
) -> dict[str, Any]:
    """Verify registry bytes, supported interpreters, and PEP 740 provenance."""
    encoded_receipt = _load_json_bytes(
        build_receipt_bytes, "build receipt bytes", limit=MAX_LOCAL_JSON_BYTES
    )
    if encoded_receipt != receipt:
        fail("build receipt bytes differ from the receipt being verified")
    version, distributions = validate_receipt(receipt, expected_status="built")
    if repository not in REPOSITORIES or receipt["registry"] != repository:
        fail("registry selection differs from the exact build receipt")
    installation_evidence = _installation_evidence(
        installation_evidence_paths, version, distributions
    )
    if not _positive_integer(publication_artifact_id) or not _is_sha(
        publication_artifact_sha256, 64
    ):
        fail("immutable publication artifact identity is invalid")
    expected = {entry["filename"]: entry["sha256"] for entry in distributions.values()}
    if _registry_inventory(release, version) != expected:
        fail("registry filenames or SHA-256 digests differ from built distributions")
    if distribution_directory is None:
        fail("local publication distributions are required for PEP 740 verification")
    try:
        local_distributions = _distribution_inventory(distribution_directory, version)
    except OSError:
        fail("local publication distributions are unavailable")
    if local_distributions != distributions:
        fail("local publication distributions differ from the exact build receipt")
    verifier = cryptographic_verifier or verify_distribution_cryptographically
    provenance: dict[str, object] = {}
    for kind, entry in distributions.items():
        filename = entry["filename"]
        if filename not in provenances:
            fail("PyPI Integrity API provenance is missing for a distribution")
        verified_provenance, selected_provenance = _selected_provenance(
            provenances[filename], filename, entry["sha256"], repository
        )
        artifact = distribution_directory / filename
        verified_provenance["cryptographic_verifier"] = verifier(
            artifact, selected_provenance, repository
        )
        if sha256(artifact) != entry["sha256"]:
            fail("local publication distribution changed during PEP 740 verification")
        provenance[kind] = verified_provenance
    verified = dict(receipt)
    verified["status"] = "published"
    verified["build_receipt_sha256"] = sha256_bytes(build_receipt_bytes)
    verified["registry_verification"] = {
        "repository": repository,
        "python_versions": list(SUPPORTED_PYTHON),
        "installation_evidence": installation_evidence,
        "publication_artifact": {
            "id": publication_artifact_id,
            "sha256": publication_artifact_sha256,
        },
        "provenance": provenance,
    }
    validate_receipt(verified, expected_status="published")
    return verified


def _fetch_json(
    url: str, *, integrity: bool = False, attempts: int = 12
) -> dict[str, Any]:
    for attempt in range(attempts):
        try:
            headers = (
                {"Accept": "application/vnd.pypi.integrity.v1+json"}
                if integrity
                else {}
            )
            request = urllib.request.Request(url, headers=headers)
            with urllib.request.urlopen(request, timeout=20) as response:
                payload = response.read(MAX_REGISTRY_RESPONSE_BYTES + 1)
            if len(payload) > MAX_REGISTRY_RESPONSE_BYTES:
                fail("registry evidence exceeds the bounded response limit")
            value = _load_json_bytes(
                payload, "registry response", limit=MAX_REGISTRY_RESPONSE_BYTES
            )
            if not isinstance(value, dict):
                fail("registry response must be one JSON object")
            return value
        except urllib.error.HTTPError as error:
            if error.code != 404 or attempt + 1 == attempts:
                raise
        if attempt + 1 < attempts:
            time.sleep(5)
    fail("registry evidence did not become visible")


def fetch_release(repository: str, version: str) -> dict[str, Any]:
    return _fetch_json(f"{REPOSITORIES[repository]['release']}/{version}/json")


def fetch_provenance(repository: str, version: str, filename: str) -> dict[str, Any]:
    parts = (PROJECT, version, filename)
    path = "/".join(urllib.parse.quote(part, safe="") for part in parts)
    return _fetch_json(
        f"{REPOSITORIES[repository]['integrity']}/{path}/provenance",
        integrity=True,
    )


def write_json(path: Path, value: dict[str, Any]) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    temporary.replace(path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subcommands = parser.add_subparsers(dest="command", required=True)
    build = subcommands.add_parser("build")
    build.add_argument("--directory", type=Path, required=True)
    build.add_argument("--reproducible-directory", type=Path, required=True)
    build.add_argument(
        "--independent-build-receipt", type=Path, action="append", required=True
    )
    build.add_argument("--publication-authority", type=Path, required=True)
    build.add_argument("--version", required=True)
    build.add_argument("--source-tag", required=True)
    build.add_argument("--source-commit", required=True)
    build.add_argument("--source-tree", required=True)
    build.add_argument("--workflow-repository", required=True)
    build.add_argument("--workflow-ref", required=True)
    build.add_argument("--workflow-sha", required=True)
    build.add_argument("--workflow-run-id", type=int, required=True)
    build.add_argument("--workflow-run-attempt", type=int, required=True)
    build.add_argument("--repository", choices=sorted(REPOSITORIES), required=True)
    build.add_argument("--testpypi-receipt", type=Path)
    build.add_argument("--testpypi-run-metadata", type=Path)
    build.add_argument("--testpypi-receipt-sha256")
    build.add_argument("--testpypi-run-id", type=int)
    build.add_argument("--output", type=Path, required=True)
    local = subcommands.add_parser("check-local")
    local.add_argument("--receipt", type=Path, required=True)
    local.add_argument("--directory", type=Path, required=True)
    local.add_argument("--source-commit", required=True)
    local.add_argument("--source-tree", required=True)
    local.add_argument("--workflow-run-id", type=int, required=True)
    local.add_argument("--workflow-run-attempt", type=int, required=True)
    local.add_argument("--repository", choices=sorted(REPOSITORIES), required=True)
    verify = subcommands.add_parser("verify")
    verify.add_argument("--receipt", type=Path, required=True)
    verify.add_argument("--repository", choices=sorted(REPOSITORIES), required=True)
    verify.add_argument(
        "--installation-evidence", type=Path, action="append", required=True
    )
    verify.add_argument("--publication-artifact-id", type=int, required=True)
    verify.add_argument("--publication-artifact-sha256", required=True)
    verify.add_argument("--distribution-directory", type=Path)
    verify.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "build":
        receipt = build_receipt(
            args.directory,
            args.version,
            args.source_tag,
            args.source_commit,
            reproducible_directory=args.reproducible_directory,
            independent_build_receipts=tuple(args.independent_build_receipt),
            publication_authority=args.publication_authority,
            source_tree=args.source_tree,
            workflow_repository=args.workflow_repository,
            workflow_ref=args.workflow_ref,
            workflow_sha=args.workflow_sha,
            workflow_run_id=args.workflow_run_id,
            workflow_run_attempt=args.workflow_run_attempt,
            repository=args.repository,
            testpypi_receipt=args.testpypi_receipt,
            testpypi_run_metadata=args.testpypi_run_metadata,
            testpypi_receipt_sha256=args.testpypi_receipt_sha256,
            testpypi_run_id=args.testpypi_run_id,
        )
        write_json(args.output, receipt)
    elif args.command == "check-local":
        receipt = load_local_json(args.receipt, "local build receipt")
        check_local_distributions(
            receipt,
            args.directory,
            source_commit=args.source_commit,
            source_tree=args.source_tree,
            workflow_run_id=args.workflow_run_id,
            workflow_run_attempt=args.workflow_run_attempt,
            repository=args.repository,
        )
        print(json.dumps({"status": "passed"}, sort_keys=True))
        return 0
    else:
        receipt_bytes = _read_bounded_file(args.receipt, "local build receipt")
        receipt = _load_json_bytes(
            receipt_bytes, "local build receipt", limit=MAX_LOCAL_JSON_BYTES
        )
        version, distributions = validate_receipt(receipt, expected_status="built")
        release = fetch_release(args.repository, version)
        provenances = {
            entry["filename"]: fetch_provenance(
                args.repository, version, entry["filename"]
            )
            for entry in distributions.values()
        }
        receipt = verify_registry(
            receipt,
            receipt_bytes,
            release,
            provenances,
            args.repository,
            tuple(args.installation_evidence),
            args.publication_artifact_id,
            args.publication_artifact_sha256,
            distribution_directory=(
                args.distribution_directory
                if args.distribution_directory is not None
                else args.receipt.parent / "publish-dist"
            ),
        )
        write_json(args.output, receipt)
    print(json.dumps(receipt, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
