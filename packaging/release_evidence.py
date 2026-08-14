#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Generate and verify post-commit release evidence without self-reference."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from provenance import (
    BUILDER_ID,
    BUILD_TYPE,
    REPOSITORY,
    TARGET_RUNNERS,
    WORKFLOW_PATH,
    commit_file,
    commit_file_digest,
)
from required_checks import REPORT_SUFFIX, load_report


ROOT = Path(__file__).resolve().parents[1]
SCHEMA_NAME = "release-evidence-v1"
SCHEMA_VERSION = 1
SCHEMA_PATH = Path(__file__).with_name("release-evidence-v1.schema.json")
EVIDENCE_SUFFIX = ".release-evidence.json"
ARCHIVE_SUFFIXES = (".tar.gz", ".zip")
TARGET_ARCHIVES = {
    "aarch64-apple-darwin": ".tar.gz",
    "x86_64-apple-darwin": ".tar.gz",
    "x86_64-pc-windows-msvc": ".zip",
    "x86_64-unknown-linux-gnu": ".tar.gz",
}
HEX_OBJECT = re.compile(r"[0-9a-f]{40}")
HEX_DIGEST = re.compile(r"[0-9a-f]{64}")
EVENT_NAME = re.compile(r"[a-z][a-z0-9_]*")
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?")
RUN_URL = re.compile(
    re.escape(REPOSITORY)
    + r"/actions/runs/([1-9][0-9]*)/attempts/([1-9][0-9]*)"
)


@dataclass(frozen=True)
class ReleaseIdentity:
    commit: str
    tree: str
    version: str
    tag: str


def git(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ("git", *arguments),
        cwd=ROOT,
        check=check,
        capture_output=True,
        text=True,
    )


def resolve_commit(commit: str) -> str:
    if HEX_OBJECT.fullmatch(commit) is None:
        raise ValueError("commit must be a lowercase 40-character Git object ID")
    result = git("rev-parse", "--verify", f"{commit}^{{commit}}", check=False)
    if result.returncode != 0 or result.stdout.strip() != commit:
        raise ValueError("commit is not an available canonical Git commit object")
    return commit


def tree_for_commit(commit: str) -> str:
    resolve_commit(commit)
    tree = git("rev-parse", "--verify", f"{commit}^{{tree}}").stdout.strip()
    if HEX_OBJECT.fullmatch(tree) is None:
        raise RuntimeError("Git returned a malformed tree object ID")
    return tree


def current_commit() -> str:
    commit = git("rev-parse", "--verify", "HEAD^{commit}").stdout.strip()
    if HEX_OBJECT.fullmatch(commit) is None:
        raise RuntimeError("Git returned a malformed HEAD commit object ID")
    return commit


def require_tracked_worktree_matches(commit: str) -> None:
    commit = resolve_commit(commit)
    if current_commit() != commit:
        raise ValueError("release commit must equal the checked-out HEAD")
    result = git("diff", "--quiet", commit, "--", check=False)
    if result.returncode == 1:
        raise ValueError("tracked index or worktree differs from the release commit")
    if result.returncode != 0:
        raise RuntimeError("Git could not compare the tracked worktree to the commit")


def source_identity(commit: str) -> ReleaseIdentity:
    commit = resolve_commit(commit)
    cargo = tomllib.loads(commit_file(commit, "Cargo.toml").decode("utf-8"))
    version = str(cargo["workspace"]["package"]["version"])
    if VERSION.fullmatch(version) is None:
        raise ValueError("workspace version at the release commit is malformed")
    return ReleaseIdentity(
        commit=commit,
        tree=tree_for_commit(commit),
        version=version,
        tag=f"v{version}",
    )


def evidence_name(tag: str) -> str:
    return f"hyphae-{tag}{EVIDENCE_SUFFIX}"


def archive_name(version: str, target: str) -> str:
    return f"hyphae-{version}-{target}{TARGET_ARCHIVES[target]}"


def required_checks_name(tag: str) -> str:
    return f"hyphae-{tag}{REPORT_SUFFIX}"


def is_release_tag_ref(tag: str, workflow_ref: str) -> bool:
    return workflow_ref == f"refs/tags/{tag}"


def requires_hosted_checks(tag: str, workflow_ref: str, event: str) -> bool:
    return event == "push" and is_release_tag_ref(tag, workflow_ref)


def expected_primary_names(
    version: str,
    *,
    include_required_checks: bool,
) -> set[str]:
    archives = {
        archive_name(version, target)
        for target in TARGET_ARCHIVES
    }
    expected = (
        archives
        | {f"{archive}.provenance.json" for archive in archives}
        | {
            f"hyphae-v{version}.spdx.json",
            f"hyphae-v{version}.cdx.json",
        }
    )
    if include_required_checks:
        expected.add(required_checks_name(f"v{version}"))
    return expected


def artifact_role(name: str) -> str | None:
    if name.endswith(ARCHIVE_SUFFIXES):
        return "archive"
    if name.endswith(".provenance.json"):
        return "provenance"
    if name.endswith(".spdx.json"):
        return "sbom-spdx"
    if name.endswith(".cdx.json"):
        return "sbom-cyclonedx"
    if name.endswith(REPORT_SUFFIX):
        return "required-checks"
    return None


def primary_artifacts(
    directory: Path,
    version: str,
    *,
    include_required_checks: bool,
) -> list[Path]:
    artifacts = sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and artifact_role(path.name) is not None
    )
    actual = {path.name for path in artifacts}
    expected = expected_primary_names(
        version,
        include_required_checks=include_required_checks,
    )
    if actual != expected:
        missing = sorted(expected - actual)
        unexpected = sorted(actual - expected)
        raise ValueError(
            "release primary artifacts differ from the canonical target set: "
            f"missing={missing!r}, unexpected={unexpected!r}"
        )
    return artifacts


def artifact_record(path: Path) -> dict[str, object]:
    role = artifact_role(path.name)
    if role is None:
        raise ValueError(f"unsupported release evidence artifact: {path.name}")
    return {
        "name": path.name,
        "role": role,
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "size": path.stat().st_size,
    }


def expected_run_url(run_id: str, run_attempt: int) -> str:
    return f"{REPOSITORY}/actions/runs/{run_id}/attempts/{run_attempt}"


def run_url_identity(value: object, label: str) -> tuple[str, int]:
    if not isinstance(value, str):
        raise ValueError(f"{label} must be a string")
    match = RUN_URL.fullmatch(value)
    if match is None:
        raise ValueError(f"{label} is not a canonical Hyphae workflow run URL")
    return match.group(1), int(match.group(2))


def build_release_evidence(
    *,
    directory: Path,
    tag: str,
    commit: str,
    workflow_ref: str,
    event: str,
    run_id: str,
    run_attempt: int,
    tag_object: str | None = None,
    tag_target: str | None = None,
) -> dict[str, object]:
    identity = source_identity(commit)
    if tag != identity.tag:
        raise ValueError("release evidence tag does not match the release commit")
    include_required_checks = requires_hosted_checks(identity.tag, workflow_ref, event)
    document: dict[str, object] = {
        "schema": SCHEMA_NAME,
        "schema_version": SCHEMA_VERSION,
        "repository": REPOSITORY,
        "release": {"tag": identity.tag, "version": identity.version},
        "source": {
            "commit": identity.commit,
            "tree": identity.tree,
            "tag_object": tag_object,
            "tag_target": tag_target,
        },
        "workflow": {
            "path": WORKFLOW_PATH,
            "ref": workflow_ref,
            "event": event,
            "run_id": run_id,
            "run_attempt": run_attempt,
            "url": expected_run_url(run_id, run_attempt),
        },
        "artifacts": [
            artifact_record(path)
            for path in primary_artifacts(
                directory,
                identity.version,
                include_required_checks=include_required_checks,
            )
        ],
    }
    validate_release_evidence(
        document,
        directory=directory,
        expected_commit=identity.commit,
    )
    return document


def require_exact_keys(value: dict[str, Any], expected: set[str], label: str) -> None:
    actual = set(value)
    if actual != expected:
        raise ValueError(
            f"{label} keys differ: missing={sorted(expected - actual)!r}, "
            f"unexpected={sorted(actual - expected)!r}"
        )


def require_object(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{label} must be an object")
    return value


def require_string(value: object, label: str) -> str:
    if not isinstance(value, str):
        raise ValueError(f"{label} must be a string")
    return value


def validate_provenance_predicate(
    document: object,
    *,
    archive: str,
    target: str,
    identity: ReleaseIdentity,
    workflow_ref: str,
    run_id: str,
    maximum_run_attempt: int,
) -> None:
    root = require_object(document, f"{archive} provenance")
    require_exact_keys(
        root,
        {"buildDefinition", "runDetails"},
        f"{archive} provenance",
    )
    definition = require_object(
        root["buildDefinition"],
        f"{archive} provenance buildDefinition",
    )
    require_exact_keys(
        definition,
        {
            "buildType",
            "externalParameters",
            "internalParameters",
            "resolvedDependencies",
        },
        f"{archive} provenance buildDefinition",
    )
    if definition["buildType"] != BUILD_TYPE:
        raise ValueError(f"{archive} provenance build type is not canonical")

    external = require_object(
        definition["externalParameters"],
        f"{archive} provenance externalParameters",
    )
    require_exact_keys(
        external,
        {"profile", "target", "workflow"},
        f"{archive} provenance externalParameters",
    )
    if external["profile"] != "dist" or external["target"] != target:
        raise ValueError(f"{archive} provenance target or profile differs")
    provenance_workflow = require_object(
        external["workflow"],
        f"{archive} provenance workflow",
    )
    expected_workflow = {
        "path": f"/{WORKFLOW_PATH}",
        "ref": workflow_ref,
        "repository": REPOSITORY,
    }
    if provenance_workflow != expected_workflow:
        raise ValueError(f"{archive} provenance workflow differs from release evidence")

    internal = require_object(
        definition["internalParameters"],
        f"{archive} provenance internalParameters",
    )
    require_exact_keys(
        internal,
        {"runner_arch", "runner_os", "rust_toolchain"},
        f"{archive} provenance internalParameters",
    )
    if (
        (internal["runner_os"], internal["runner_arch"])
        != TARGET_RUNNERS[target]
        or internal["rust_toolchain"] != "1.96.0"
    ):
        raise ValueError(f"{archive} provenance runner identity is invalid")

    expected_dependencies = [
        {
            "digest": {"gitCommit": identity.commit},
            "uri": f"git+{REPOSITORY}@{identity.commit}",
        },
        {
            "digest": {
                "sha256": commit_file_digest(identity.commit, "Cargo.lock"),
            },
            "uri": f"{REPOSITORY}/blob/{identity.commit}/Cargo.lock",
        },
        {
            "digest": {
                "sha256": commit_file_digest(identity.commit, WORKFLOW_PATH),
            },
            "uri": f"{REPOSITORY}/blob/{identity.commit}/{WORKFLOW_PATH}",
        },
    ]
    if definition["resolvedDependencies"] != expected_dependencies:
        raise ValueError(
            f"{archive} provenance source or dependency digests differ from the commit"
        )

    run_details = require_object(
        root["runDetails"],
        f"{archive} provenance runDetails",
    )
    require_exact_keys(
        run_details,
        {"builder", "metadata"},
        f"{archive} provenance runDetails",
    )
    if run_details["builder"] != {"id": BUILDER_ID}:
        raise ValueError(f"{archive} provenance builder is not canonical")
    metadata = require_object(
        run_details["metadata"],
        f"{archive} provenance metadata",
    )
    require_exact_keys(
        metadata,
        {"invocationId"},
        f"{archive} provenance metadata",
    )
    provenance_run_id, provenance_run_attempt = run_url_identity(
        metadata["invocationId"],
        f"{archive} provenance invocation",
    )
    if (
        provenance_run_id != run_id
        or provenance_run_attempt > maximum_run_attempt
    ):
        raise ValueError(
            f"{archive} provenance invocation differs from release evidence"
        )


def validate_release_provenance(
    *,
    directory: Path,
    identity: ReleaseIdentity,
    workflow_ref: str,
    run_id: str,
    maximum_run_attempt: int,
) -> None:
    for target in TARGET_ARCHIVES:
        archive = archive_name(identity.version, target)
        predicate = directory / f"{archive}.provenance.json"
        validate_provenance_predicate(
            load_json(predicate, f"{archive} provenance"),
            archive=archive,
            target=target,
            identity=identity,
            workflow_ref=workflow_ref,
            run_id=run_id,
            maximum_run_attempt=maximum_run_attempt,
        )


def validate_release_evidence(
    document: object,
    *,
    directory: Path,
    expected_commit: str,
    expected_tag_object: str | None = None,
    expected_tag_target: str | None = None,
    require_live_tag_binding: bool = False,
) -> None:
    identity = source_identity(expected_commit)
    root = require_object(document, "release evidence")
    require_exact_keys(
        root,
        {
            "schema",
            "schema_version",
            "repository",
            "release",
            "source",
            "workflow",
            "artifacts",
        },
        "release evidence",
    )
    if root["schema"] != SCHEMA_NAME or root["schema_version"] != SCHEMA_VERSION:
        raise ValueError("unsupported release evidence schema")
    if root["repository"] != REPOSITORY:
        raise ValueError("release evidence repository is not canonical")

    release = require_object(root["release"], "release")
    require_exact_keys(release, {"tag", "version"}, "release")
    if (
        release["version"] != identity.version
        or release["tag"] != identity.tag
    ):
        raise ValueError("release evidence version does not match the release commit")

    source = require_object(root["source"], "source")
    require_exact_keys(
        source,
        {"commit", "tree", "tag_object", "tag_target"},
        "source",
    )
    commit = require_string(source["commit"], "source.commit")
    tree = require_string(source["tree"], "source.tree")
    if HEX_OBJECT.fullmatch(commit) is None or HEX_OBJECT.fullmatch(tree) is None:
        raise ValueError("source commit and tree must be lowercase Git object IDs")
    if commit != identity.commit:
        raise ValueError("release evidence commit differs from the expected commit")
    if tree != identity.tree:
        raise ValueError("release evidence tree differs from the commit tree")

    workflow = require_object(root["workflow"], "workflow")
    require_exact_keys(
        workflow,
        {"path", "ref", "event", "run_id", "run_attempt", "url"},
        "workflow",
    )
    if workflow["path"] != WORKFLOW_PATH:
        raise ValueError("release evidence workflow path is not canonical")
    workflow_ref = require_string(workflow["ref"], "workflow.ref")
    event = require_string(workflow["event"], "workflow.event")
    run_id = require_string(workflow["run_id"], "workflow.run_id")
    run_attempt = workflow["run_attempt"]
    if not workflow_ref.startswith("refs/"):
        raise ValueError("workflow.ref must be a full refs/ path")
    if EVENT_NAME.fullmatch(event) is None:
        raise ValueError("workflow.event is malformed")
    if (
        not run_id.isascii()
        or not run_id.isdigit()
        or run_id.startswith("0")
    ):
        raise ValueError("workflow.run_id must be a positive decimal string")
    if isinstance(run_attempt, bool) or not isinstance(run_attempt, int) or run_attempt < 1:
        raise ValueError("workflow.run_attempt must be a positive integer")
    if workflow["url"] != expected_run_url(run_id, run_attempt):
        raise ValueError("workflow.url does not match its run identity")
    tag_ref = is_release_tag_ref(identity.tag, workflow_ref)
    if workflow_ref.startswith("refs/tags/") and not tag_ref:
        raise ValueError("workflow tag ref differs from the release commit version")
    tag_object = source["tag_object"]
    tag_target = source["tag_target"]
    if tag_ref:
        if (
            not isinstance(tag_object, str)
            or HEX_OBJECT.fullmatch(tag_object) is None
            or not isinstance(tag_target, str)
            or HEX_OBJECT.fullmatch(tag_target) is None
        ):
            raise ValueError("tag release evidence requires canonical tag object IDs")
        if tag_target != identity.commit:
            raise ValueError("release tag target differs from the source commit")
    elif tag_object is not None or tag_target is not None:
        raise ValueError("non-tag release evidence must not claim a tag object")
    if (
        require_live_tag_binding
        and tag_ref
        and expected_tag_object is None
        and expected_tag_target is None
    ):
        raise ValueError("tag verification requires the live tag object and target")
    if (expected_tag_object is None) != (expected_tag_target is None):
        raise ValueError("expected tag object and target must be provided together")
    if expected_tag_object is not None and expected_tag_target is not None:
        if (
            HEX_OBJECT.fullmatch(expected_tag_object) is None
            or HEX_OBJECT.fullmatch(expected_tag_target) is None
        ):
            raise ValueError("expected tag object IDs must be canonical")
        if (
            tag_object != expected_tag_object
            or tag_target != expected_tag_target
        ):
            raise ValueError("release evidence differs from the live tag binding")
    include_required_checks = requires_hosted_checks(identity.tag, workflow_ref, event)

    artifacts = root["artifacts"]
    if not isinstance(artifacts, list) or not artifacts:
        raise ValueError("release evidence artifacts must be a nonempty array")
    names: list[str] = []
    records: dict[str, dict[str, Any]] = {}
    for index, item in enumerate(artifacts):
        record = require_object(item, f"artifacts[{index}]")
        require_exact_keys(record, {"name", "role", "sha256", "size"}, f"artifacts[{index}]")
        name = require_string(record["name"], f"artifacts[{index}].name")
        digest = require_string(record["sha256"], f"artifacts[{index}].sha256")
        size = record["size"]
        if not name or Path(name).name != name or "/" in name or "\\" in name:
            raise ValueError("release evidence artifact name is unsafe")
        if name in records:
            raise ValueError("release evidence artifact names must be unique")
        if record["role"] != artifact_role(name):
            raise ValueError(f"release evidence artifact role is invalid for {name}")
        if HEX_DIGEST.fullmatch(digest) is None:
            raise ValueError(f"release evidence artifact digest is malformed for {name}")
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise ValueError(f"release evidence artifact size is invalid for {name}")
        names.append(name)
        records[name] = record
    if names != sorted(names):
        raise ValueError("release evidence artifacts must be sorted by name")

    expected = {
        path.name: path
        for path in primary_artifacts(
            directory,
            identity.version,
            include_required_checks=include_required_checks,
        )
    }
    if set(records) != set(expected):
        raise ValueError("release evidence artifact set differs from release payloads")
    for name, path in expected.items():
        actual = artifact_record(path)
        if records[name] != actual:
            raise ValueError(f"release evidence artifact mismatch for {name}")
    validate_release_provenance(
        directory=directory,
        identity=identity,
        workflow_ref=workflow_ref,
        run_id=run_id,
        maximum_run_attempt=run_attempt,
    )
    if include_required_checks:
        load_report(
            directory / required_checks_name(identity.tag),
            expected_commit=identity.commit,
        )


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_json(path: Path, label: str) -> object:
    if not path.is_file():
        raise ValueError(f"{label} file is missing: {path.name}")
    return json.loads(
        path.read_text("utf-8"),
        object_pairs_hook=reject_duplicate_keys,
    )


def load_release_evidence(path: Path) -> object:
    return load_json(path, "release evidence")


def validate_release_evidence_file(
    path: Path,
    *,
    directory: Path,
    expected_commit: str,
    expected_tag_object: str | None = None,
    expected_tag_target: str | None = None,
    require_live_tag_binding: bool = False,
) -> None:
    validate_release_evidence(
        load_release_evidence(path),
        directory=directory,
        expected_commit=expected_commit,
        expected_tag_object=expected_tag_object,
        expected_tag_target=expected_tag_target,
        require_live_tag_binding=require_live_tag_binding,
    )


def write_release_evidence(path: Path, document: object) -> None:
    with path.open("x", encoding="utf-8", newline="\n") as output:
        json.dump(document, output, indent=2, sort_keys=True)
        output.write("\n")


def generate_release_evidence(
    *,
    directory: Path,
    output: Path,
    tag: str,
    commit: str,
    workflow_ref: str,
    event: str,
    run_id: str,
    run_attempt: int,
    tag_object: str | None,
    tag_target: str | None,
) -> None:
    directory = directory.resolve(strict=True)
    if output.parent.resolve() != directory:
        raise ValueError("release evidence output must be inside the release directory")
    if output.name != evidence_name(tag):
        raise ValueError("release evidence filename does not match its tag")
    commit = resolve_commit(commit)
    require_tracked_worktree_matches(commit)
    document = build_release_evidence(
        directory=directory,
        tag=tag,
        commit=commit,
        workflow_ref=workflow_ref,
        event=event,
        run_id=run_id,
        run_attempt=run_attempt,
        tag_object=tag_object,
        tag_target=tag_target,
    )
    write_release_evidence(output, document)
    validate_release_evidence_file(output, directory=directory, expected_commit=commit)


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    generate = subparsers.add_parser("generate")
    generate.add_argument("--directory", required=True, type=Path)
    generate.add_argument("--output", required=True, type=Path)
    generate.add_argument("--tag", required=True)
    generate.add_argument("--commit", required=True)
    generate.add_argument("--workflow-ref", required=True)
    generate.add_argument("--event", required=True)
    generate.add_argument("--run-id", required=True)
    generate.add_argument("--run-attempt", required=True, type=int)
    generate.add_argument("--tag-object")
    generate.add_argument("--tag-target")

    verify = subparsers.add_parser("verify")
    verify.add_argument("--directory", required=True, type=Path)
    verify.add_argument("--manifest", required=True, type=Path)
    verify.add_argument("--commit", required=True)
    verify.add_argument("--tag-object")
    verify.add_argument("--tag-target")

    arguments = parser.parse_args()
    if arguments.command == "generate":
        generate_release_evidence(
            directory=arguments.directory,
            output=arguments.output,
            tag=arguments.tag,
            commit=arguments.commit,
            workflow_ref=arguments.workflow_ref,
            event=arguments.event,
            run_id=arguments.run_id,
            run_attempt=arguments.run_attempt,
            tag_object=arguments.tag_object,
            tag_target=arguments.tag_target,
        )
    else:
        validate_release_evidence_file(
            arguments.manifest,
            directory=arguments.directory,
            expected_commit=arguments.commit,
            expected_tag_object=arguments.tag_object,
            expected_tag_target=arguments.tag_target,
            require_live_tag_binding=True,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
