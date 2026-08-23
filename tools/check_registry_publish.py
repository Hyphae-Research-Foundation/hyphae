#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Gate registry publication on immutable GitHub and release evidence."""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable


ROOT = Path(__file__).resolve().parents[1]
POLICY_PATH = Path("config/registry-publish-authority.json")
EXPECTED_AUTHORITY = {
    "version": "2.0.1",
    "tag": "v2.0.1",
    "source_ref_kind": "annotated-tag",
    "require_exact_clean_source": True,
}
EXPECTED_NPM_PACKAGES = (
    ("sdks/typescript", "@hyphae_/hyphae"),
    ("integrations/javascript", "@hyphae_/hyphae-integrations"),
)
HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
POSITIVE = re.compile(r"[1-9][0-9]*\Z")
GITHUB_ACTIONS_APP = {"id": 15368, "slug": "github-actions"}
EXPECTED_POLICY_KEYS = {
    "$comment",
    "schema",
    "repository",
    "branch",
    "version",
    "tag",
    "tag_kind",
    "tag_signature",
    "environment",
    "required_checks",
    "required_artifacts",
    "control_files",
}
EXPECTED_CHECKS = (
    ("Quality", ".github/workflows/ci.yml", "push", "main"),
    ("Test (Linux stable)", ".github/workflows/ci.yml", "push", "main"),
    ("Test (Linux MSRV)", ".github/workflows/ci.yml", "push", "main"),
    ("Test (macOS stable)", ".github/workflows/ci.yml", "push", "main"),
    ("Test (Windows stable)", ".github/workflows/ci.yml", "push", "main"),
    ("Public client conformance", ".github/workflows/ci.yml", "push", "main"),
    ("Optional framework integrations", ".github/workflows/ci.yml", "push", "main"),
    ("Release readiness", ".github/workflows/ci.yml", "push", "main"),
    ("Security hard-kill aggregate", ".github/workflows/ci.yml", "push", "main"),
    ("MCP real hosts", ".github/workflows/ci.yml", "push", "main"),
    ("Dependency and license policy", ".github/workflows/security.yml", "push", "main"),
    ("Package x86_64-unknown-linux-gnu", ".github/workflows/release.yml", "push", "v2.0.1"),
    ("Package x86_64-apple-darwin", ".github/workflows/release.yml", "push", "v2.0.1"),
    ("Package aarch64-apple-darwin", ".github/workflows/release.yml", "push", "v2.0.1"),
    ("Package x86_64-pc-windows-msvc", ".github/workflows/release.yml", "push", "v2.0.1"),
    ("Assemble and verify release candidate", ".github/workflows/release.yml", "push", "v2.0.1"),
    ("Publish GitHub release", ".github/workflows/release.yml", "push", "v2.0.1"),
    ("Validate all exact-SHA G8 receipts", ".github/workflows/native-g8-closure.yml", "workflow_dispatch", "main"),
)
EXPECTED_ARTIFACTS = (
    ("release-candidate", ".github/workflows/release.yml", "hyphae-release-candidate"),
    ("signed-release-receipt", ".github/workflows/release.yml", "native-g8-signed-release-{commit}"),
    ("g8-aggregate", ".github/workflows/native-g8-closure.yml", "native-g8-aggregate-{commit}"),
    ("mcp-real-hosts", ".github/workflows/ci.yml", "mcp-real-hosts-{commit}"),
    ("security-hard-kill", ".github/workflows/ci.yml", "security-hard-kill-{commit}"),
)
EXPECTED_RELEASE_RUN_JOBS = frozenset(
    {
        "Package x86_64-unknown-linux-gnu",
        "Package x86_64-apple-darwin",
        "Package aarch64-apple-darwin",
        "Package x86_64-pc-windows-msvc",
        "Assemble and verify release candidate",
        "Publish GitHub release",
    }
)
EXPECTED_CONTROL_FILES = (
    ".github/workflows/registry-publish.yml",
    "config/crates-io-release.json",
    "config/npm-release.json",
    "config/registry-publish-authority.json",
    "packaging/g8_release_verification.py",
    "packaging/finalize_release.py",
    "packaging/provenance.py",
    "packaging/release_evidence.py",
    "packaging/release-evidence-v1.schema.json",
    "packaging/required_checks.py",
    "packaging/required-checks-report-v1.schema.json",
    "tools/check_crate_packages.py",
    "tools/check_native_g8_receipts.py",
    "tools/check_npm_packages.py",
    "tools/check_registry_publish.py",
    "tools/check_mcp_host_receipts.py",
    "tools/check_security_crash_matrix.py",
    "tools/check_relicensing_transition.py",
    "tools/check_relicensing_preflight.py",
    "tools/check_license_policy.py",
    "tools/generate_third_party_licenses.py",
    "tools/produce_native_g8_receipt.py",
    "tools/verify_crate_packages.py",
)


class GateFailure(ValueError):
    pass


RegistryQuery = Callable[[dict[str, Any]], dict[str, Any] | None]
RegistryPublish = Callable[[dict[str, Any]], None]
StateWriter = Callable[[dict[str, Any]], None]


def _reconcile_record(intent: dict[str, Any]) -> dict[str, Any]:
    return {
        "ecosystem": intent["ecosystem"],
        "name": intent["name"],
        "version": intent["version"],
        "layer": intent["layer"],
        "source_commit": intent["source_commit"],
        "source_tree": intent["source_tree"],
        "intended_sha256": hashlib.sha256(intent["bytes"]).hexdigest(),
    }


def _matching_registry_record(
    intent: dict[str, Any], remote: dict[str, Any]
) -> dict[str, Any]:
    record = _reconcile_record(intent)
    registry_bytes = remote.get("bytes")
    metadata = remote.get("metadata")
    provenance = remote.get("provenance")
    if not isinstance(registry_bytes, bytes) or not isinstance(metadata, bytes):
        raise GateFailure(f"{intent['name']}: registry artifact or metadata is malformed")
    record.update(
        {
            "registry_sha256": hashlib.sha256(registry_bytes).hexdigest(),
            "metadata_sha256": hashlib.sha256(metadata).hexdigest(),
            "provenance_sha256": (
                hashlib.sha256(provenance).hexdigest()
                if isinstance(provenance, bytes)
                else None
            ),
            "layer_visible": remote.get("layer_visible", True) is True,
        }
    )
    if registry_bytes != intent["bytes"]:
        raise GateFailure(
            f"{intent['name']}@{intent['version']}: registry artifact differs from "
            "intended packaged bytes"
        )
    if remote.get("source_commit") != intent["source_commit"]:
        raise GateFailure(
            f"{intent['name']}@{intent['version']}: registry source provenance differs"
        )
    if intent["ecosystem"] == "npm" and (
        not isinstance(provenance, bytes) or remote.get("provenance_verified") is not True
    ):
        raise GateFailure(
            f"{intent['name']}@{intent['version']}: npm provenance is absent or unverified"
        )
    return record


def reconcile_registry_artifact(
    intent: dict[str, Any],
    *,
    query: RegistryQuery,
    publish: RegistryPublish,
    state: dict[str, Any],
    persist: StateWriter,
    attempts: int,
    interval_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
) -> dict[str, Any]:
    """Reconcile one immutable version before or after an ambiguous upload."""
    if attempts <= 0:
        raise GateFailure("registry polling attempts must be positive")
    artifacts = state.setdefault("artifacts", {})
    key = intent["name"]
    base = _reconcile_record(intent)
    existing = artifacts.get(key)
    if isinstance(existing, dict) and existing.get("intended_sha256") not in {
        None,
        base["intended_sha256"],
    }:
        raise GateFailure(f"{key}: receipt intent differs from this rerun")

    remote = query(intent)
    if remote is not None:
        if remote.get("incomplete") is not None:
            artifacts[key] = {**base, "status": "mismatch"}
            persist(state)
            raise GateFailure(
                f"{key}@{intent['version']}: present registry artifact lacks "
                f"{remote['incomplete']}"
            )
        try:
            record = _matching_registry_record(intent, remote)
        except GateFailure:
            artifacts[key] = {**base, "status": "mismatch"}
            persist(state)
            raise
        record.update({"status": "complete", "outcome": "already-complete"})
        artifacts[key] = record
        persist(state)
        return record

    artifacts[key] = {**base, "status": "uploading"}
    persist(state)
    upload_completed = False
    try:
        publish(intent)
        upload_completed = True
    except subprocess.SubprocessError:
        pass

    for attempt in range(1, attempts + 1):
        remote = query(intent)
        if remote is not None:
            if remote.get("incomplete") is not None:
                if attempt < attempts:
                    sleep(interval_seconds)
                continue
            try:
                record = _matching_registry_record(intent, remote)
            except GateFailure:
                artifacts[key] = {**base, "status": "mismatch", "poll_attempts": attempt}
                persist(state)
                raise
            record.update(
                {
                    "status": "complete",
                    "outcome": "published" if upload_completed else "reconciled-ambiguous",
                    "poll_attempts": attempt,
                }
            )
            artifacts[key] = record
            persist(state)
            return record
        if attempt < attempts:
            sleep(interval_seconds)

    artifacts[key] = {
        **base,
        "status": "ambiguous",
        "poll_attempts": attempts,
    }
    persist(state)
    raise GateFailure(
        f"{key}@{intent['version']}: upload is ambiguous after registry polling timeout"
    )


def reconcile_registry_layer(
    intents: list[dict[str, Any]],
    *,
    query: RegistryQuery,
    publish: RegistryPublish,
    state: dict[str, Any],
    persist: StateWriter,
    attempts: int,
    interval_seconds: float,
    sleep: Callable[[float], None] = time.sleep,
) -> list[dict[str, Any]]:
    results = []
    for intent in intents:
        results.append(
            reconcile_registry_artifact(
                intent,
                query=query,
                publish=publish,
                state=state,
                persist=persist,
                attempts=attempts,
                interval_seconds=interval_seconds,
                sleep=sleep,
            )
        )
    if any(record.get("status") != "complete" for record in results):
        raise GateFailure("registry layer is not completely visible")
    for attempt in range(1, attempts + 1):
        pending = [record for record in results if record.get("layer_visible") is not True]
        if not pending:
            return results
        for record in pending:
            intent = next(item for item in intents if item["name"] == record["name"])
            remote = query(intent)
            if remote is None:
                continue
            refreshed = _matching_registry_record(intent, remote)
            refreshed.update(
                {
                    "status": "complete",
                    "outcome": record["outcome"],
                    "poll_attempts": record.get("poll_attempts", 0),
                }
            )
            state["artifacts"][record["name"]] = refreshed
            results[results.index(record)] = refreshed
        persist(state)
        if attempt < attempts:
            sleep(interval_seconds)
    pending_names = [record["name"] for record in results if not record["layer_visible"]]
    for name in pending_names:
        state["artifacts"][name]["status"] = "propagation-timeout"
    persist(state)
    raise GateFailure(f"registry layer propagation timed out: {pending_names}")
    return results


def _git(root: Path, *arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(root), *arguments],
        check=check,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=30,
    )


def _load_json(path: Path, label: str) -> dict[str, Any]:
    def reject_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise GateFailure(f"{label} contains duplicate key {key!r}")
            result[key] = value
        return result

    if not path.is_file() or path.is_symlink():
        raise GateFailure(f"{label} is missing or is not a regular file: {path}")
    value = json.loads(
        path.read_text(encoding="utf-8"), object_pairs_hook=reject_duplicates
    )
    if not isinstance(value, dict):
        raise GateFailure(f"{label} must be an object")
    return value


def _policy(root: Path) -> dict[str, Any]:
    value = _load_json(root / POLICY_PATH, "registry authority policy")
    if set(value) != EXPECTED_POLICY_KEYS:
        raise GateFailure("registry authority policy fields differ")
    expected_checks = [
        {"name": name, "workflow": workflow, "event": event, "head_branch": branch}
        for name, workflow, event, branch in EXPECTED_CHECKS
    ]
    expected_artifacts = [
        {"id": identifier, "workflow": workflow, "name": name}
        for identifier, workflow, name in EXPECTED_ARTIFACTS
    ]
    if (
        value["schema"] != "hyphae-registry-publish-authority-v1"
        or value["repository"] != "celiumsai/hyphae"
        or value["branch"] != "main"
        or value["version"] != "2.0.1"
        or value["tag"] != "v2.0.1"
        or value["tag_kind"] != "annotated"
        or value["tag_signature"]
        != {
            "required": False,
            "policy": "annotated-tag-with-sigstore-signed-release",
        }
        or value["environment"] != "registry-production"
        or value["required_checks"] != expected_checks
        or value["required_artifacts"] != expected_artifacts
        or value["control_files"] != list(EXPECTED_CONTROL_FILES)
    ):
        raise GateFailure("registry authority policy differs from the pinned 2.0.1 authority")
    return value


def _source_version(root: Path, ecosystem: str) -> tuple[str | None, list[str]]:
    failures: list[str] = []
    config_path = root / "config" / (
        "crates-io-release.json" if ecosystem == "crates-io" else "npm-release.json"
    )
    try:
        config = _load_json(config_path, f"{ecosystem} publication policy")
    except (OSError, UnicodeError, json.JSONDecodeError, GateFailure) as error:
        return None, [str(error)]
    authority = config.get("apache_publication_authority")
    if authority != EXPECTED_AUTHORITY:
        failures.append(f"{config_path}: Apache publication authority differs")
    version = config.get("version")
    if ecosystem == "crates-io":
        try:
            manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
            workspace = manifest["workspace"]["package"]
        except (OSError, UnicodeError, tomllib.TOMLDecodeError, KeyError, TypeError) as error:
            failures.append(f"Cargo.toml: cannot resolve workspace package: {error}")
        else:
            if version != workspace.get("version"):
                failures.append("crates.io config version differs from workspace version")
            if workspace.get("license") != "Apache-2.0":
                failures.append("Cargo.toml: workspace license is not Apache-2.0")
    else:
        packages = config.get("packages")
        expected = [{"path": path, "name": name} for path, name in EXPECTED_NPM_PACKAGES]
        if packages != expected:
            failures.append("npm publication package inventory differs")
        for path, name in EXPECTED_NPM_PACKAGES:
            try:
                package = _load_json(root / path / "package.json", f"{path}/package.json")
            except (OSError, UnicodeError, json.JSONDecodeError, GateFailure) as error:
                failures.append(str(error))
                continue
            if package.get("name") != name:
                failures.append(f"{path}/package.json: package name differs")
            if package.get("version") != version:
                failures.append(f"{path}/package.json: version differs from npm release config")
            if package.get("license") != "Apache-2.0":
                failures.append(f"{path}/package.json: license is not Apache-2.0")
    return version if isinstance(version, str) else None, failures


def validate_publish_authority(
    ecosystem: str, root: Path = ROOT, *, dry_run: bool = False
) -> list[str]:
    failures: list[str] = []
    try:
        _policy(root)
    except (OSError, UnicodeError, json.JSONDecodeError, GateFailure) as error:
        failures.append(str(error))
    version, source_failures = _source_version(root, ecosystem)
    failures.extend(source_failures)
    if dry_run:
        return failures
    if version != EXPECTED_AUTHORITY["version"]:
        failures.append(
            f"{ecosystem}: publish is blocked until exact version "
            f"{EXPECTED_AUTHORITY['version']} (current {version!r})"
        )
    status = _git(root, "status", "--porcelain=v1").stdout
    if status:
        failures.append(f"{ecosystem}: publish source worktree is not clean")
    tag = EXPECTED_AUTHORITY["tag"]
    tag_type = _git(root, "cat-file", "-t", f"refs/tags/{tag}", check=False)
    if tag_type.returncode != 0:
        failures.append(f"{ecosystem}: required source tag {tag} is unavailable")
    elif tag_type.stdout.strip() != "tag":
        failures.append(f"{ecosystem}: required source tag {tag} is not annotated")
    else:
        head = _git(root, "rev-parse", "HEAD").stdout.strip()
        target = _git(root, "rev-parse", f"refs/tags/{tag}^{{commit}}").stdout.strip()
        if head != target:
            failures.append(f"{ecosystem}: HEAD is not the exact {tag} source commit")
    return failures


def _job(workflow: str, name: str) -> str:
    match = re.search(rf"(?ms)^  {re.escape(name)}:\n.*?(?=^  [a-z0-9-]+:\n|\Z)", workflow)
    if match is None:
        raise GateFailure(f"registry workflow is missing the {name} job")
    return match.group(0)


def validate_publish_workflow(root: Path = ROOT) -> list[str]:
    path = root / ".github/workflows/registry-publish.yml"
    try:
        workflow = path.read_text(encoding="utf-8")
        live = _job(workflow, "live")
        dry_run = _job(workflow, "dry-run")
    except (OSError, UnicodeError, GateFailure) as error:
        return [f"{path}: {error}"]
    required_live = (
        "if: github.event_name == 'workflow_dispatch' && !inputs.dry_run",
        "environment: registry-production",
        "actions: read",
        "checks: read",
        "id-token: write",
        "Reject a non-main live dispatch before checkout",
        "refs/heads/main",
        "test \"${{ github.ref }}\" = refs/heads/main",
        "github.workflow_ref",
        "ref: ${{ github.workflow_sha }}",
        "path: control",
        "ref: refs/tags/${{ inputs.source_tag }}",
        "path: source",
        "--live-resolve",
        "--live-evidence",
        "--live-recheck",
        "--publication-state \"$PUBLICATION_STATE\"",
        "native-g8-signed-release-${{ steps.authority.outputs.source_commit }}",
        "native-g8-aggregate-${{ steps.authority.outputs.source_commit }}",
        "security-hard-kill-${{ steps.authority.outputs.source_commit }}",
        "mcp-real-hosts-${{ steps.authority.outputs.source_commit }}",
        "python \"$CONTROL_ROOT/tools/check_registry_publish.py\"",
        "--authority-receipt \"$AUTHORITY_RECEIPT\"",
        "--evidence-receipt \"$EVIDENCE_RECEIPT\"",
        "cargo publish --locked -p \"$package\"",
        "previous_layer=-1",
        "layer=\"${package%%:*}\"",
        "--wait-layer \"$previous_layer\"",
        "npm publish ./sdks/typescript --provenance --access public",
        "npm publish ./integrations/javascript --provenance --access public",
        "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
        "registry-publication-${{ inputs.ecosystem }}-${{ steps.authority.outputs.source_commit }}",
        "Restore a prior reconciliation receipt on workflow rerun",
        "run-id: ${{ github.run_id }}",
    )
    required_dry = (
        "if: github.event_name == 'pull_request' || inputs.dry_run",
        "--dry-run",
        "python3 tools/verify_crate_packages.py",
        "npm pack ./sdks/typescript --dry-run",
        "npm pack ./integrations/javascript --dry-run",
    )
    failures = [
        f"{path}: required live publication control is missing: {fragment}"
        for fragment in required_live
        if fragment not in live
    ]
    failures.extend(
        f"{path}: required dry-run control is missing: {fragment}"
        for fragment in required_dry
        if fragment not in dry_run
    )
    if "cargo publish" in dry_run or "--provenance --access public" in dry_run:
        failures.append(f"{path}: unprivileged dry-run job contains a live publish command")
    if workflow.count("environment: registry-production") != 1:
        failures.append(f"{path}: exactly one live job must use the protected environment")
    if live.count("--live-recheck") < 4:
        failures.append(f"{path}: every live package boundary must recheck external authority")
    if live.count("--authority-receipt \"$AUTHORITY_RECEIPT\"") < 6:
        failures.append(f"{path}: every live publish boundary must carry authority evidence")
    if live.count("--publication-state \"$PUBLICATION_STATE\"") < 3:
        failures.append(f"{path}: every live publish boundary must persist reconciliation state")
    if (
        live.count("if (( previous_layer >= 0 && layer != previous_layer )); then") != 1
        or live.count("--wait-layer \"$previous_layer\"") != 2
    ):
        failures.append(f"{path}: crates.io layers must wait between complete layers")
    return failures


def _request_json(url: str, token: str, label: str) -> object:
    if not token:
        raise GateFailure("GITHUB_TOKEN is required for live authority")
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "hyphae-registry-publication",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        value = json.load(response)
    if value is None:
        raise GateFailure(f"GitHub returned no {label}")
    return value


def _pages(url: str, key: str | None, token: str, label: str) -> list[object]:
    values: list[object] = []
    separator = "&" if "?" in url else "?"
    for page in range(1, 101):
        payload = _request_json(
            f"{url}{separator}per_page=100&page={page}", token, label
        )
        page_values = payload.get(key) if key is not None and isinstance(payload, dict) else payload
        if not isinstance(page_values, list):
            raise GateFailure(f"GitHub {label} response is not an array")
        values.extend(page_values)
        if len(page_values) < 100:
            return values
    raise GateFailure(f"GitHub {label} pagination exceeded 10,000 entries")


def _api(repository: str, path: str) -> str:
    return f"https://api.github.com/repos/{repository}/{path.lstrip('/')}"


def _workflow_run(repository: str, run_id: int, token: str) -> dict[str, Any]:
    value = _request_json(_api(repository, f"actions/runs/{run_id}"), token, "workflow run")
    if not isinstance(value, dict):
        raise GateFailure("GitHub workflow run response must be an object")
    return value


def _run_for_check(
    check: dict[str, Any], expected: dict[str, str], repository: str, commit: str, token: str
) -> tuple[dict[str, Any], dict[str, Any]]:
    check_id = check.get("id")
    details = check.get("details_url")
    app = check.get("app")
    if (
        check.get("name") != expected["name"]
        or check.get("head_sha") != commit
        or check.get("status") != "completed"
        or check.get("conclusion") != "success"
        or not isinstance(check_id, int)
        or isinstance(check_id, bool)
        or not isinstance(details, str)
        or not isinstance(app, dict)
        or {"id": app.get("id"), "slug": app.get("slug")} != GITHUB_ACTIONS_APP
    ):
        raise GateFailure(f"required check is not a successful GitHub Actions job: {expected['name']}")
    match = re.fullmatch(
        rf"https://github\.com/{re.escape(repository)}/actions/runs/([1-9][0-9]*)/job/{check_id}",
        details,
    )
    if match is None or check.get("html_url") != details:
        raise GateFailure(f"required check URL is not canonical: {expected['name']}")
    run = _workflow_run(repository, int(match.group(1)), token)
    if (
        run.get("path") != expected["workflow"]
        or run.get("head_sha") != commit
        or run.get("head_branch") != expected["head_branch"]
        or run.get("event") != expected["event"]
        or run.get("status") != "completed"
        or run.get("conclusion") != "success"
        or run.get("repository", {}).get("full_name") != repository
    ):
        raise GateFailure(f"required check workflow identity differs: {expected['name']}")
    job = _request_json(_api(repository, f"actions/jobs/{check_id}"), token, "job")
    if not isinstance(job, dict) or (
        job.get("id") != check_id
        or job.get("run_id") != run.get("id")
        or job.get("run_attempt") != run.get("run_attempt")
        or job.get("name") != expected["name"]
        or job.get("head_sha") != commit
        or job.get("status") != "completed"
        or job.get("conclusion") != "success"
    ):
        raise GateFailure(f"required job metadata differs: {expected['name']}")
    return run, job


def _select_check_run(
    candidates: list[dict[str, Any]], expected_name: str
) -> dict[str, Any]:
    if not candidates:
        raise GateFailure(f"required check has no exact-SHA run: {expected_name}")
    completions = [check.get("completed_at") for check in candidates]
    if any(not isinstance(value, str) for value in completions):
        raise GateFailure(f"required check completion is malformed: {expected_name}")
    latest_completion = max(completions)
    latest = [
        check for check in candidates if check.get("completed_at") == latest_completion
    ]
    if len(latest) != 1:
        raise GateFailure(
            f"required check has an ambiguous latest completion: {expected_name}"
        )
    return latest[0]


def fetch_required_checks(
    repository: str, commit: str, token: str, excluded_run_id: str, policy: dict[str, Any]
) -> tuple[list[dict[str, Any]], dict[str, dict[str, Any]]]:
    checks = _pages(
        _api(repository, f"commits/{commit}/check-runs?filter=all"),
        "check_runs",
        token,
        "check runs",
    )
    selected: list[dict[str, Any]] = []
    runs_by_path: dict[str, dict[str, Any]] = {}
    excluded_fragment = f"/actions/runs/{excluded_run_id}/"
    release_run: dict[str, Any] | None = None
    release_expectations = [
        expected
        for expected in policy["required_checks"]
        if expected["name"] in EXPECTED_RELEASE_RUN_JOBS
    ]
    for publish_expected in release_expectations:
        if publish_expected["name"] == "Publish GitHub release":
            candidates = [
                check
                for check in checks
                if isinstance(check, dict)
                and check.get("name") == publish_expected["name"]
                and check.get("head_sha") == commit
                and excluded_fragment not in str(check.get("details_url", ""))
            ]
            selected_publish = _select_check_run(candidates, publish_expected["name"])
            release_run, _job_metadata = _run_for_check(
                selected_publish, publish_expected, repository, commit, token
            )
            break
    if release_run is None:
        raise GateFailure("canonical successful Release workflow run is unavailable")
    for expected in policy["required_checks"]:
        candidates = [
            check
            for check in checks
            if isinstance(check, dict)
            and check.get("name") == expected["name"]
            and check.get("head_sha") == commit
            and excluded_fragment not in str(check.get("details_url", ""))
        ]
        if expected["name"] in EXPECTED_RELEASE_RUN_JOBS:
            release_run_id = release_run["id"]
            candidates = [
                check
                for check in candidates
                if f"/actions/runs/{release_run_id}/" in str(check.get("details_url", ""))
            ]
            if len(candidates) != 1:
                raise GateFailure(
                    f"required Release job is not unique in canonical run: {expected['name']}"
                )
            selected_check = candidates[0]
        else:
            selected_check = _select_check_run(candidates, expected["name"])
        run, _job_metadata = _run_for_check(
            selected_check, expected, repository, commit, token
        )
        existing = runs_by_path.get(expected["workflow"])
        if existing is not None and existing.get("id") != run.get("id"):
            raise GateFailure(f"required workflow jobs span multiple runs: {expected['workflow']}")
        runs_by_path[expected["workflow"]] = run
        selected.append(
            {
                "name": expected["name"],
                "check_run_id": selected_check["id"],
                "workflow_run_id": run["id"],
                "workflow_run_attempt": run["run_attempt"],
                "workflow": expected["workflow"],
                "event": expected["event"],
                "head_branch": expected["head_branch"],
                "head_sha": commit,
            }
        )
    return selected, runs_by_path


def _artifact_records(repository: str, run: dict[str, Any], token: str) -> list[dict[str, Any]]:
    records = _pages(
        _api(repository, f"actions/runs/{run['id']}/artifacts"),
        "artifacts",
        token,
        "workflow artifacts",
    )
    return [record for record in records if isinstance(record, dict)]


def _artifact_authorities(
    repository: str,
    commit: str,
    token: str,
    policy: dict[str, Any],
    runs_by_path: dict[str, dict[str, Any]],
) -> list[dict[str, Any]]:
    records_by_run: dict[int, list[dict[str, Any]]] = {}
    result: list[dict[str, Any]] = []
    for expected in policy["required_artifacts"]:
        run = runs_by_path.get(expected["workflow"])
        if run is None:
            raise GateFailure(f"required artifact workflow is unavailable: {expected['id']}")
        records = records_by_run.get(run["id"])
        if records is None:
            records = _artifact_records(repository, run, token)
            records_by_run[run["id"]] = records
        name = expected["name"].format(commit=commit)
        matches = [record for record in records if record.get("name") == name]
        if len(matches) != 1:
            raise GateFailure(f"required workflow artifact must be unique: {name}")
        record = matches[0]
        digest = record.get("digest")
        if (
            record.get("expired") is not False
            or not isinstance(record.get("id"), int)
            or not isinstance(digest, str)
            or not digest.startswith("sha256:")
            or HEX64.fullmatch(digest.removeprefix("sha256:")) is None
        ):
            raise GateFailure(f"required workflow artifact is expired or malformed: {name}")
        result.append(
            {
                "id": expected["id"],
                "artifact_id": record["id"],
                "name": name,
                "service_digest": digest,
                "workflow": expected["workflow"],
                "workflow_run_id": run["id"],
                "workflow_run_attempt": run["run_attempt"],
            }
        )
    return result


def _file_at_commit(root: Path, commit: str, relative: str) -> bytes:
    result = subprocess.run(
        ["git", "-C", str(root), "show", f"{commit}:{relative}"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=30,
    )
    if result.returncode != 0:
        raise GateFailure(f"trusted control file is unavailable at main: {relative}")
    return result.stdout


def _control_digests(control_root: Path, source_root: Path, workflow_sha: str, policy: dict[str, Any]) -> list[dict[str, str]]:
    records: list[dict[str, str]] = []
    for relative in policy["control_files"]:
        trusted = _file_at_commit(control_root, workflow_sha, relative)
        source_path = source_root / relative
        if not source_path.is_file() or source_path.is_symlink():
            raise GateFailure(f"tag target control file is missing: {relative}")
        source = source_path.read_bytes()
        if trusted != source:
            raise GateFailure(f"tag-controlled publication file differs from trusted main: {relative}")
        records.append(
            {
                "path": relative,
                "sha256": hashlib.sha256(trusted).hexdigest(),
            }
        )
    return records


def _control_digests_match(
    control_root: Path,
    source_root: Path,
    workflow_sha: str,
    policy: dict[str, Any],
    records: object,
) -> None:
    if not isinstance(records, list) or len(records) != len(policy["control_files"]):
        raise GateFailure("trusted control-file digest coverage differs")
    for relative, row in zip(policy["control_files"], records, strict=True):
        if (
            not isinstance(row, dict)
            or set(row) != {"path", "sha256"}
            or row.get("path") != relative
            or not isinstance(row.get("sha256"), str)
            or HEX64.fullmatch(row["sha256"]) is None
        ):
            raise GateFailure("trusted control-file digest is malformed")
        trusted = _file_at_commit(control_root, workflow_sha, relative)
        if hashlib.sha256(trusted).hexdigest() != row["sha256"]:
            raise GateFailure(f"trusted main control file changed: {relative}")
        source_path = source_root / relative
        if not source_path.is_file() or source_path.is_symlink():
            raise GateFailure(f"tag target control file is missing: {relative}")
        if source_path.read_bytes() != trusted:
            raise GateFailure(f"tag target control file changed: {relative}")


def _tag_signature(root: Path, tag: str, required: bool) -> dict[str, object]:
    if not required:
        return {"required": False, "verified": False}
    verification = _git(root, "tag", "-v", tag, check=False)
    if verification.returncode != 0:
        raise GateFailure("release policy requires a cryptographically verified tag signature")
    return {"required": True, "verified": True}


def _write_new_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("x", encoding="utf-8", newline="\n") as output:
        json.dump(value, output, indent=2, sort_keys=True)
        output.write("\n")


def _write_outputs(path: Path, values: dict[str, str]) -> None:
    with path.open("a", encoding="utf-8", newline="\n") as output:
        for name, value in values.items():
            output.write(f"{name}={value}\n")


def _url_bytes(url: str, label: str, *, accept: str | None = "application/json") -> bytes:
    headers = {"User-Agent": "hyphae-registry-reconcile"}
    if accept is not None:
        headers["Accept"] = accept
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            raise FileNotFoundError(label) from error
        raise


def _archive_content_sha256(encoded: bytes, mode: str, prefix: str) -> str:
    digest = hashlib.sha256()
    try:
        with tarfile.open(fileobj=io.BytesIO(encoded), mode=mode) as archive:
            members = [member for member in archive.getmembers() if member.isfile()]
            for member in sorted(members, key=lambda item: item.name):
                relative = member.name.removeprefix(prefix)
                source = archive.extractfile(member)
                if source is None:
                    raise GateFailure("registry archive contains an unreadable file")
                content = source.read()
                name = relative.encode("utf-8")
                digest.update(len(name).to_bytes(8, "big"))
                digest.update(name)
                digest.update(len(content).to_bytes(8, "big"))
                digest.update(content)
    except tarfile.TarError as error:
        raise GateFailure("registry archive is malformed") from error
    return digest.hexdigest()


def _crate_vcs_commit(encoded: bytes, name: str, version: str) -> str:
    member = f"{name}-{version}/.cargo_vcs_info.json"
    try:
        with tarfile.open(fileobj=io.BytesIO(encoded), mode="r:gz") as archive:
            source = archive.extractfile(member)
            if source is None:
                raise GateFailure(f"{name}@{version}: crate lacks Cargo VCS metadata")
            value = json.load(source)
    except (tarfile.TarError, KeyError, json.JSONDecodeError) as error:
        raise GateFailure(f"{name}@{version}: crate VCS metadata is malformed") from error
    commit = value.get("git", {}).get("sha1") if isinstance(value, dict) else None
    if not isinstance(commit, str) or HEX40.fullmatch(commit) is None:
        raise GateFailure(f"{name}@{version}: crate VCS commit is malformed")
    # Cargo omits the dirty marker entirely for a clean packaging tree and
    # writes true for a dirty one; only an explicit clean or absent marker
    # passes.
    if value.get("git", {}).get("dirty") not in (None, False):
        raise GateFailure(f"{name}@{version}: crate VCS metadata reports a dirty source")
    return commit


def _crates_io_query(intent: dict[str, Any]) -> dict[str, Any] | None:
    name = urllib.parse.quote(intent["name"], safe="")
    version = urllib.parse.quote(intent["version"], safe="")
    try:
        metadata = _url_bytes(
            f"https://crates.io/api/v1/crates/{name}/{version}",
            f"{intent['name']}@{intent['version']}",
        )
    except FileNotFoundError:
        return None
    value = json.loads(metadata)
    row = value.get("version") if isinstance(value, dict) else None
    if not isinstance(row, dict) or row.get("crate") != intent["name"] or row.get("num") != intent["version"]:
        raise GateFailure(f"{intent['name']}@{intent['version']}: crates.io metadata differs")
    # The download endpoint answers a JSON accept with a URL document instead
    # of the crate bytes, so this request must accept the archive itself.
    encoded = _url_bytes(
        f"https://crates.io/api/v1/crates/{name}/{version}/download",
        f"{intent['name']}@{intent['version']} crate",
        accept=None,
    )
    checksum = hashlib.sha256(encoded).hexdigest()
    if row.get("checksum") != checksum:
        raise GateFailure(f"{intent['name']}@{intent['version']}: crates.io checksum differs")
    index_name = intent["name"].lower()
    if len(index_name) == 1:
        index_path = f"1/{index_name}"
    elif len(index_name) == 2:
        index_path = f"2/{index_name}"
    elif len(index_name) == 3:
        index_path = f"3/{index_name[0]}/{index_name}"
    else:
        index_path = f"{index_name[:2]}/{index_name[2:4]}/{index_name}"
    try:
        index = _url_bytes(
            f"https://index.crates.io/{index_path}",
            f"{intent['name']} sparse-index entry",
        )
    except FileNotFoundError:
        return None
    index_rows = []
    for line in index.splitlines():
        try:
            candidate = json.loads(line)
        except json.JSONDecodeError as error:
            raise GateFailure(f"{intent['name']}: crates.io sparse index is malformed") from error
        if isinstance(candidate, dict) and candidate.get("vers") == intent["version"]:
            index_rows.append(candidate)
    layer_visible = len(index_rows) == 1 and index_rows[0].get("cksum") == checksum
    return {
        "bytes": encoded,
        "content_sha256": _archive_content_sha256(
            encoded, "r:gz", f"{intent['name']}-{intent['version']}/"
        ),
        "metadata": metadata,
        "provenance": None,
        "source_commit": _crate_vcs_commit(encoded, intent["name"], intent["version"]),
        "provenance_verified": True,
        "layer_visible": layer_visible,
    }


def _npm_provenance_commit(
    attestations: bytes, name: str, version: str, encoded: bytes
) -> str:
    value = json.loads(attestations)
    rows = value.get("attestations") if isinstance(value, dict) else None
    if not isinstance(rows, list):
        raise GateFailure(f"{name}@{version}: npm attestation response is malformed")
    digest = hashlib.sha512(encoded).hexdigest()
    commits: set[str] = set()
    for row in rows:
        if not isinstance(row, dict) or row.get("predicateType") != "https://slsa.dev/provenance/v1":
            continue
        envelope = row.get("bundle", {}).get("dsseEnvelope", {})
        payload = envelope.get("payload") if isinstance(envelope, dict) else None
        if not isinstance(payload, str):
            continue
        try:
            statement = json.loads(base64.b64decode(payload, validate=True))
        except (ValueError, json.JSONDecodeError):
            continue
        subjects = statement.get("subject") if isinstance(statement, dict) else None
        if not isinstance(subjects, list) or not any(
            isinstance(subject, dict)
            and subject.get("digest", {}).get("sha512") == digest
            for subject in subjects
        ):
            continue
        dependencies = (
            statement.get("predicate", {})
            .get("buildDefinition", {})
            .get("resolvedDependencies", [])
        )
        for dependency in dependencies:
            commit = dependency.get("digest", {}).get("gitCommit") if isinstance(dependency, dict) else None
            if isinstance(commit, str) and HEX40.fullmatch(commit) is not None:
                commits.add(commit)
    if len(commits) != 1:
        raise GateFailure(f"{name}@{version}: npm provenance has no unique source commit")
    return next(iter(commits))


def _verify_npm_signatures(name: str, version: str) -> None:
    with tempfile.TemporaryDirectory(prefix="hyphae-npm-verify-") as directory:
        root = Path(directory)
        (root / "package.json").write_text(
            json.dumps(
                {
                    "name": "hyphae-registry-verification",
                    "version": "0.0.0",
                    "private": True,
                    "dependencies": {name: version},
                }
            ),
            encoding="utf-8",
        )
        subprocess.run(
            ["npm", "install", "--ignore-scripts", "--no-audit", "--no-fund"],
            cwd=root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=300,
        )
        subprocess.run(
            ["npm", "audit", "signatures", "--json", "--include-attestations"],
            cwd=root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=300,
        )


def _npm_query(intent: dict[str, Any]) -> dict[str, Any] | None:
    escaped = urllib.parse.quote(intent["name"], safe="")
    version = urllib.parse.quote(intent["version"], safe="")
    try:
        metadata = _url_bytes(
            f"https://registry.npmjs.org/{escaped}/{version}",
            f"{intent['name']}@{intent['version']}",
        )
    except FileNotFoundError:
        return None
    value = json.loads(metadata)
    if not isinstance(value, dict) or value.get("name") != intent["name"] or value.get("version") != intent["version"]:
        raise GateFailure(f"{intent['name']}@{intent['version']}: npm metadata differs")
    dist = value.get("dist")
    if not isinstance(dist, dict) or not isinstance(dist.get("tarball"), str):
        raise GateFailure(f"{intent['name']}@{intent['version']}: npm distribution metadata is malformed")
    encoded = _url_bytes(dist["tarball"], f"{intent['name']}@{intent['version']} tarball")
    integrity = dist.get("integrity")
    expected_integrity = "sha512-" + base64.b64encode(hashlib.sha512(encoded).digest()).decode("ascii")
    if integrity != expected_integrity:
        raise GateFailure(f"{intent['name']}@{intent['version']}: npm integrity differs")
    try:
        attestations = _url_bytes(
            f"https://registry.npmjs.org/-/npm/v1/attestations/{escaped}@{version}",
            f"{intent['name']}@{intent['version']} provenance",
        )
    except FileNotFoundError:
        return {
            "bytes": encoded,
            "content_sha256": _archive_content_sha256(encoded, "r:gz", "package/"),
            "metadata": metadata,
            "provenance": None,
            "source_commit": None,
            "provenance_verified": False,
            "incomplete": "verified npm provenance",
        }
    source_commit = _npm_provenance_commit(
        attestations, intent["name"], intent["version"], encoded
    )
    _verify_npm_signatures(intent["name"], intent["version"])
    return {
        "bytes": encoded,
        "content_sha256": _archive_content_sha256(encoded, "r:gz", "package/"),
        "metadata": metadata,
        "provenance": attestations,
        "source_commit": source_commit,
        "provenance_verified": True,
    }


def _package_intent(
    ecosystem: str,
    command: list[str],
    root: Path,
    source: dict[str, Any],
) -> tuple[dict[str, Any], Path]:
    temporary = tempfile.TemporaryDirectory(prefix="hyphae-registry-package-")
    destination = Path(temporary.name)
    if ecosystem == "crates-io":
        name = command[-1]
        version = _load_json(root / "config/crates-io-release.json", "crates.io release config")["version"]
        release = _load_json(root / "config/crates-io-release.json", "crates.io release config")
        layer = next(
            index
            for index, packages in enumerate(release["layers"])
            if name in packages
        )
        subprocess.run(
            [
                "cargo", "package", "--locked", "--no-verify", "-p", name,
                "--target-dir", str(destination / "target"),
            ],
            cwd=root,
            check=True,
            timeout=900,
        )
        artifact = destination / "target" / "package" / f"{name}-{version}.crate"
    else:
        project = command[2].removeprefix("./")
        package = _load_json(root / project / "package.json", f"{project}/package.json")
        name = package["name"]
        version = package["version"]
        completed = subprocess.run(
            [
                "npm",
                "pack",
                f"./{project}",
                "--json",
                "--pack-destination",
                str(destination),
            ],
            cwd=root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=900,
        )
        packed = json.loads(completed.stdout)
        if not isinstance(packed, list) or len(packed) != 1 or not isinstance(packed[0].get("filename"), str):
            raise GateFailure(f"{name}: npm pack did not return one exact tarball")
        artifact = destination / packed[0]["filename"]
        layer = next(
            index
            for index, (path, _package_name) in enumerate(EXPECTED_NPM_PACKAGES)
            if path == project
        )
    if not artifact.is_file() or artifact.is_symlink():
        raise GateFailure(f"{name}@{version}: intended registry package was not produced")
    intent = {
        "ecosystem": ecosystem,
        "name": name,
        "version": version,
        "bytes": artifact.read_bytes(),
        "source_commit": source["commit"],
        "source_tree": source["tree"],
        "layer": layer,
        "artifact": artifact,
        "temporary": temporary,
    }
    intent["content_sha256"] = _archive_content_sha256(
        intent["bytes"],
        "r:gz",
        f"{name}-{version}/" if ecosystem == "crates-io" else "package/",
    )
    if ecosystem == "crates-io" and _crate_vcs_commit(intent["bytes"], name, version) != source["commit"]:
        raise GateFailure(f"{name}@{version}: intended crate VCS commit differs")
    return intent, artifact


def _publication_inventory(ecosystem: str, root: Path) -> list[str]:
    if ecosystem == "crates-io":
        release = _load_json(root / "config/crates-io-release.json", "crates.io release config")
        return [package for layer in release["layers"] for package in layer]
    return [name for _path, name in EXPECTED_NPM_PACKAGES]


def _load_publication_state(
    path: Path,
    ecosystem: str,
    root: Path,
    authority: dict[str, Any],
) -> dict[str, Any]:
    expected = {
        "schema": "hyphae-registry-publication-state-v1",
        "ecosystem": ecosystem,
        "version": "2.0.1",
        "source": authority["source"],
        "inventory": _publication_inventory(ecosystem, root),
    }
    if path.exists():
        value = _load_json(path, "registry publication state")
        if any(value.get(key) != expected_value for key, expected_value in expected.items()):
            raise GateFailure("registry publication state authority differs")
        if not isinstance(value.get("artifacts"), dict):
            raise GateFailure("registry publication state artifacts are malformed")
        return value
    return {**expected, "status": "in-progress", "artifacts": {}}


def _publication_state_writer(path: Path) -> StateWriter:
    def write(value: dict[str, Any]) -> None:
        complete = set(value["inventory"]) == {
            name
            for name, record in value["artifacts"].items()
            if isinstance(record, dict) and record.get("status") == "complete"
        }
        value["status"] = "complete" if complete else "in-progress"
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(path.suffix + ".tmp")
        temporary.write_text(
            json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        temporary.replace(path)

    return write


def reconcile_publish_command(
    ecosystem: str,
    command: list[str],
    root: Path,
    authority: dict[str, Any],
    state_path: Path,
) -> dict[str, Any]:
    intent, artifact = _package_intent(ecosystem, command, root, authority["source"])
    state = _load_publication_state(state_path, ecosystem, root, authority)
    persist = _publication_state_writer(state_path)
    persist(state)

    def publish(_intent: dict[str, Any]) -> None:
        publish_command = command
        if ecosystem == "npm":
            publish_command = [
                "npm", "publish", str(artifact), "--provenance", "--access", "public"
            ]
        subprocess.run(publish_command, cwd=root, check=True, timeout=900)

    return reconcile_registry_artifact(
        intent,
        query=_crates_io_query if ecosystem == "crates-io" else _npm_query,
        publish=publish,
        state=state,
        persist=persist,
        attempts=60,
        interval_seconds=10,
    )


def wait_for_completed_layer(
    ecosystem: str,
    layer: int,
    root: Path,
    authority: dict[str, Any],
    state_path: Path,
) -> None:
    if ecosystem != "crates-io":
        raise GateFailure("only crates.io has topological publication layers")
    release = _load_json(root / "config/crates-io-release.json", "crates.io release config")
    packages = release["layers"][layer]
    state = _load_publication_state(state_path, ecosystem, root, authority)
    persist = _publication_state_writer(state_path)
    intents = []
    temporaries = []
    for package in packages:
        intent, _artifact = _package_intent(
            ecosystem,
            ["cargo", "publish", "--locked", "-p", package],
            root,
            authority["source"],
        )
        temporaries.append(intent["temporary"])
        intents.append(intent)
    recorded = state.get("artifacts", {})
    if any(
        not isinstance(recorded.get(intent["name"]), dict)
        or recorded[intent["name"]].get("status") != "complete"
        for intent in intents
    ):
        raise GateFailure(f"crates.io layer {layer} publication receipt is incomplete")
    reconcile_registry_layer(
        intents,
        query=_crates_io_query,
        publish=lambda _intent: None,
        state=state,
        persist=persist,
        attempts=60,
        interval_seconds=10,
    )


def resolve_live_authority(
    *,
    ecosystem: str,
    root: Path,
    control_root: Path,
    source_tag: str,
    repository: str,
    workflow_sha: str,
    workflow_run_id: str,
    token: str,
) -> dict[str, Any]:
    policy = _policy(control_root)
    if (
        source_tag != policy["tag"]
        or repository != policy["repository"]
        or HEX40.fullmatch(workflow_sha) is None
        or POSITIVE.fullmatch(workflow_run_id) is None
    ):
        raise GateFailure("live publication invocation identity differs")
    source_policy = _policy(root)
    if source_policy != policy:
        raise GateFailure("source registry authority policy differs from trusted main")
    _git(
        root,
        "fetch",
        "--force",
        "--no-tags",
        "origin",
        f"+refs/tags/{source_tag}:refs/hyphae/registry-tag",
    )
    if _git(root, "cat-file", "-t", "refs/hyphae/registry-tag").stdout.strip() != "tag":
        raise GateFailure(f"remote {source_tag} is not an annotated tag object")
    tag_object = _git(root, "rev-parse", "refs/hyphae/registry-tag").stdout.strip()
    source_commit = _git(
        root, "rev-parse", "refs/hyphae/registry-tag^{commit}"
    ).stdout.strip()
    if (
        _git(root, "rev-parse", f"refs/tags/{source_tag}").stdout.strip()
        != tag_object
        or _git(root, "rev-parse", f"refs/tags/{source_tag}^{{commit}}").stdout.strip()
        != source_commit
    ):
        raise GateFailure("checked-out tag differs from the exact remote tag")
    failures = validate_publish_authority(ecosystem, root)
    if failures:
        raise GateFailure("; ".join(failures))
    source_tree = _git(root, "rev-parse", f"{source_commit}^{{tree}}").stdout.strip()
    _git(root, "fetch", "--force", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
    origin_main = _git(root, "rev-parse", "refs/remotes/origin/main").stdout.strip()
    if source_commit != origin_main:
        raise GateFailure("v2.0.1 target is not the exact origin/main commit")
    checks, runs_by_path = fetch_required_checks(
        repository, source_commit, token, workflow_run_id, policy
    )
    artifacts = _artifact_authorities(
        repository, source_commit, token, policy, runs_by_path
    )
    return {
        "schema": "hyphae-registry-publish-github-authority-v1",
        "repository": repository,
        "ecosystem": ecosystem,
        "source": {
            "tag": source_tag,
            "tag_object": tag_object,
            "commit": source_commit,
            "tree": source_tree,
            "origin_main": origin_main,
        },
        "control": {
            "workflow_sha": workflow_sha,
            "workflow_run_id": workflow_run_id,
            "workflow_ref": (
                "celiumsai/hyphae/.github/workflows/registry-publish.yml@refs/heads/main"
            ),
            "files": _control_digests(control_root, root, workflow_sha, policy),
        },
        "tag_signature": _tag_signature(
            root, source_tag, bool(policy["tag_signature"]["required"])
        ),
        "checks": checks,
        "artifacts": artifacts,
    }


def validate_authority_receipt(
    value: dict[str, Any], ecosystem: str, root: Path, policy: dict[str, Any]
) -> dict[str, Any]:
    if set(value) != {
        "schema", "repository", "ecosystem", "source", "control",
        "tag_signature", "checks", "artifacts",
    }:
        raise GateFailure("GitHub authority receipt fields differ")
    source = value.get("source")
    control = value.get("control")
    if not isinstance(source, dict) or set(source) != {
        "tag", "tag_object", "commit", "tree", "origin_main"
    }:
        raise GateFailure("GitHub authority source fields differ")
    if not isinstance(control, dict) or set(control) != {
        "workflow_sha", "workflow_run_id", "workflow_ref", "files"
    }:
        raise GateFailure("GitHub authority control fields differ")
    commit = source.get("commit")
    if (
        value.get("schema") != "hyphae-registry-publish-github-authority-v1"
        or value.get("repository") != policy["repository"]
        or value.get("ecosystem") != ecosystem
        or source.get("tag") != policy["tag"]
        or not isinstance(source.get("tag_object"), str)
        or HEX40.fullmatch(source["tag_object"]) is None
        or not isinstance(commit, str)
        or HEX40.fullmatch(commit) is None
        or not isinstance(source.get("tree"), str)
        or HEX40.fullmatch(source["tree"]) is None
        or source.get("origin_main") != commit
        or control.get("workflow_ref")
        != "celiumsai/hyphae/.github/workflows/registry-publish.yml@refs/heads/main"
        or not isinstance(control.get("workflow_sha"), str)
        or HEX40.fullmatch(control["workflow_sha"]) is None
        or not isinstance(control.get("workflow_run_id"), str)
        or POSITIVE.fullmatch(control["workflow_run_id"]) is None
    ):
        raise GateFailure("GitHub authority receipt identity differs")
    if _git(root, "rev-parse", f"refs/tags/{policy['tag']}").stdout.strip() != source["tag_object"]:
        raise GateFailure("live tag object differs from GitHub authority receipt")
    if _git(root, "rev-parse", f"refs/tags/{policy['tag']}^{{commit}}").stdout.strip() != commit:
        raise GateFailure("live tag target differs from GitHub authority receipt")
    if _git(root, "rev-parse", f"{commit}^{{tree}}").stdout.strip() != source["tree"]:
        raise GateFailure("source tree differs from GitHub authority receipt")
    if value.get("tag_signature") != (
        {"required": True, "verified": True}
        if policy["tag_signature"]["required"]
        else {"required": False, "verified": False}
    ):
        raise GateFailure("tag signature authority differs")
    expected_checks = policy["required_checks"]
    checks = value.get("checks")
    if not isinstance(checks, list) or len(checks) != len(expected_checks):
        raise GateFailure("GitHub required-check receipt coverage differs")
    for expected, check in zip(expected_checks, checks, strict=True):
        if not isinstance(check, dict) or (
            check.get("name") != expected["name"]
            or check.get("workflow") != expected["workflow"]
            or check.get("event") != expected["event"]
            or check.get("head_branch") != expected["head_branch"]
            or check.get("head_sha") != commit
            or not isinstance(check.get("check_run_id"), int)
            or not isinstance(check.get("workflow_run_id"), int)
            or not isinstance(check.get("workflow_run_attempt"), int)
        ):
            raise GateFailure(f"GitHub required-check receipt differs: {expected['name']}")
    artifacts = value.get("artifacts")
    expected_artifacts = policy["required_artifacts"]
    if not isinstance(artifacts, list) or len(artifacts) != len(expected_artifacts):
        raise GateFailure("GitHub artifact receipt coverage differs")
    check_runs = {
        check["workflow"]: (
            check["workflow_run_id"], check["workflow_run_attempt"]
        )
        for check in checks
    }
    for expected, artifact in zip(expected_artifacts, artifacts, strict=True):
        expected_name = expected["name"].format(commit=commit)
        expected_run = check_runs.get(expected["workflow"])
        if not isinstance(artifact, dict) or (
            artifact.get("id") != expected["id"]
            or artifact.get("name") != expected_name
            or artifact.get("workflow") != expected["workflow"]
            or not isinstance(artifact.get("artifact_id"), int)
            or not isinstance(artifact.get("workflow_run_id"), int)
            or not isinstance(artifact.get("workflow_run_attempt"), int)
            or expected_run
            != (artifact.get("workflow_run_id"), artifact.get("workflow_run_attempt"))
            or not isinstance(artifact.get("service_digest"), str)
            or not artifact["service_digest"].startswith("sha256:")
            or HEX64.fullmatch(artifact["service_digest"].removeprefix("sha256:")) is None
        ):
            raise GateFailure(f"GitHub artifact receipt differs: {expected['id']}")
    files = control.get("files")
    if not isinstance(files, list) or [row.get("path") for row in files if isinstance(row, dict)] != policy["control_files"]:
        raise GateFailure("trusted control-file digest coverage differs")
    for row in files:
        if set(row) != {"path", "sha256"} or HEX64.fullmatch(row["sha256"]) is None:
            raise GateFailure("trusted control-file digest is malformed")
    return value


def _artifact_digest(path: Path) -> str:
    if not path.is_file() or path.is_symlink():
        raise GateFailure(f"required evidence file is missing: {path}")
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _verify_release_and_g8(
    root: Path, control_root: Path, evidence_root: Path, authority: dict[str, Any]
) -> dict[str, Any]:
    source = authority["source"]
    commit = source["commit"]
    tag = source["tag"]
    release = evidence_root / "release-candidate"
    signed = evidence_root / "signed-release"
    g8 = evidence_root / "g8"
    release_manifest = release / f"hyphae-{tag}.release-evidence.json"
    required_checks = release / f"hyphae-{tag}.required-checks.json"
    signed_raw = signed / "native-g8-signed-release.json"
    signed_receipt = signed / "native-g8-signed-release-receipt.json"
    g8_aggregate = g8 / "native-g8-aggregate.json"
    environment = os.environ.copy()
    environment["HYPHAE_RELEASE_SOURCE_ROOT"] = str(root)
    subprocess.run(
        [
            sys.executable,
            str(control_root / "packaging/release_evidence.py"),
            "verify",
            "--directory",
            str(release),
            "--manifest",
            str(release_manifest),
            "--commit",
            commit,
            "--tag-object",
            source["tag_object"],
            "--tag-target",
            commit,
        ],
        cwd=control_root,
        env=environment,
        check=True,
        timeout=300,
    )
    release_check = next(
        check
        for check in authority["checks"]
        if check["name"] == "Publish GitHub release"
    )
    certificate_identity = (
        f"https://github.com/{authority['repository']}/.github/workflows/release.yml@"
        f"refs/tags/{tag}"
    )
    subprocess.run(
        [
            sys.executable,
            str(control_root / "packaging/g8_release_verification.py"),
            "--directory",
            str(release),
            "--commit",
            commit,
            "--tag",
            tag,
            "--tag-object",
            source["tag_object"],
            "--tag-target",
            commit,
            "--certificate-identity",
            certificate_identity,
            "--output",
            str(evidence_root / "reverified-signed-release.json"),
        ],
        cwd=control_root,
        env=environment,
        check=True,
        timeout=900,
    )
    from tools.check_native_g8_receipts import validate_aggregate
    from tools.produce_native_g8_receipt import validate_signed_release

    signed_payload = _load_json(signed_raw, "signed release result")
    validate_signed_release(signed_payload, commit)
    signed_receipt_payload = _load_json(signed_receipt, "signed release G8 receipt")
    if (
        signed_receipt_payload.get("source_commit") != commit
        or signed_receipt_payload.get("requirement") != "sbom-signatures-provenance"
        or signed_receipt_payload.get("status") != "passed"
        or signed_receipt_payload.get("artifacts")
        != [{"name": signed_raw.name, "sha256": _artifact_digest(signed_raw)}]
    ):
        raise GateFailure("signed release G8 receipt is not source and digest bound")
    aggregate = _load_json(g8_aggregate, "G8 aggregate")
    validate_aggregate(aggregate, commit)
    sys.path.insert(0, str(control_root / "packaging"))
    from required_checks import load_report

    load_report(required_checks, expected_commit=commit)
    release_document = _load_json(release_manifest, "release evidence")
    if (
        release_document.get("workflow", {}).get("run_id")
        != str(release_check["workflow_run_id"])
        or release_document.get("workflow", {}).get("run_attempt")
        != release_check["workflow_run_attempt"]
        or release_document.get("workflow", {}).get("event") != "push"
        or release_document.get("workflow", {}).get("ref") != f"refs/tags/{tag}"
    ):
        raise GateFailure("release evidence differs from the selected Release authority")
    return {
        "release_evidence": {
            "path": release_manifest.name,
            "sha256": _artifact_digest(release_manifest),
        },
        "required_checks": {
            "path": required_checks.name,
            "sha256": _artifact_digest(required_checks),
        },
        "signed_release": {
            "path": signed_raw.name,
            "sha256": _artifact_digest(signed_raw),
        },
        "signed_release_receipt": {
            "path": signed_receipt.name,
            "sha256": _artifact_digest(signed_receipt),
        },
        "g8_aggregate": {
            "path": g8_aggregate.name,
            "sha256": _artifact_digest(g8_aggregate),
        },
    }


def _directory_digest(path: Path) -> str:
    if not path.is_dir() or path.is_symlink():
        raise GateFailure(f"required evidence directory is missing: {path}")
    children = list(path.iterdir())
    if any(child.is_symlink() or not child.is_file() for child in children):
        raise GateFailure(f"required evidence directory is not flat and regular: {path}")
    files = sorted(children)
    if not files:
        raise GateFailure(f"required evidence directory is empty: {path}")
    digest = hashlib.sha256()
    for file in files:
        name = file.name.encode("utf-8")
        content = file.read_bytes()
        digest.update(len(name).to_bytes(8, "big"))
        digest.update(name)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


def _verify_external_ci_artifacts(
    root: Path, control_root: Path, evidence_root: Path, commit: str
) -> dict[str, dict[str, str]]:
    sys.path.insert(0, str(control_root))
    from tools.check_mcp_host_receipts import validate as validate_mcp_receipts
    from tools.check_security_crash_matrix import (
        validate as validate_security_corpus,
        validate_receipts as validate_security_receipts,
    )

    security = evidence_root / "security-hard-kill"
    security_receipts = [
        security / f"security-process-crash-matrix-{shard}.json" for shard in range(4)
    ]
    corpus = _load_json(
        root / "conformance/v2/security-crash-cases.json", "security crash corpus"
    )
    registry = _load_json(
        root / "contracts/native-access-control-v1.json", "access-control registry"
    )
    validate_security_corpus(
        corpus,
        registry,
        root / "crates/hyphae-native-product/src/operation.rs",
        root,
    )
    validate_security_receipts(
        [_load_json(receipt, "security hard-kill receipt") for receipt in security_receipts],
        corpus,
        commit,
    )

    mcp = evidence_root / "mcp-real-hosts"
    validate_mcp_receipts(mcp, root=root, expected_commit=commit)
    return {
        "security_hard_kill": {
            "path": "security-hard-kill",
            "sha256": _directory_digest(security),
        },
        "mcp_real_hosts": {
            "path": "mcp-real-hosts",
            "sha256": _directory_digest(mcp),
        },
    }


def _validate_transition(root: Path) -> dict[str, Any]:
    from tools.check_relicensing_transition import transition_for_committed_tree

    transition = transition_for_committed_tree(root)
    subprocess.run(
        [sys.executable, "tools/check_relicensing_transition.py"],
        cwd=root,
        check=True,
        timeout=300,
    )
    return transition


def validate_live_evidence(
    ecosystem: str,
    root: Path,
    control_root: Path,
    evidence_root: Path,
    authority_path: Path,
) -> dict[str, Any]:
    policy = _policy(control_root)
    authority = validate_authority_receipt(
        _load_json(authority_path, "GitHub authority receipt"), ecosystem, root, policy
    )
    _control_digests_match(
        control_root,
        root,
        authority["control"]["workflow_sha"],
        policy,
        authority["control"]["files"],
    )
    transition = _validate_transition(root)
    evidence = _verify_release_and_g8(root, control_root, evidence_root, authority)
    external_ci = _verify_external_ci_artifacts(
        root, control_root, evidence_root, authority["source"]["commit"]
    )
    inventory, failures = _source_version(root, ecosystem)
    if failures or inventory != policy["version"]:
        raise GateFailure("; ".join(failures) or "registry package inventory version differs")
    return {
        "schema": "hyphae-registry-publish-evidence-v1",
        "ecosystem": ecosystem,
        "source": authority["source"],
        "control": authority["control"],
        "transition": transition,
        "release": evidence,
        "external_ci": external_ci,
        "package_inventory": {
            "version": inventory,
            "config": (
                "config/crates-io-release.json"
                if ecosystem == "crates-io"
                else "config/npm-release.json"
            ),
        },
    }


def validate_evidence_receipt(value: dict[str, Any], ecosystem: str, authority: dict[str, Any]) -> None:
    if set(value) != {
        "schema", "ecosystem", "source", "control", "transition", "release", "external_ci",
        "package_inventory"
    }:
        raise GateFailure("publication evidence receipt fields differ")
    if (
        value["schema"] != "hyphae-registry-publish-evidence-v1"
        or value["ecosystem"] != ecosystem
        or value["source"] != authority["source"]
        or value["control"] != authority["control"]
        or value.get("transition", {}).get("target_release") != "1.2.0"
        or value.get("transition", {}).get("tree") != authority["source"]["tree"]
        or value.get("package_inventory", {}).get("version") != "2.0.1"
    ):
        raise GateFailure("publication evidence receipt identity differs")
    release = value.get("release")
    if not isinstance(release, dict) or set(release) != {
        "release_evidence", "required_checks", "signed_release",
        "signed_release_receipt", "g8_aggregate",
    }:
        raise GateFailure("publication release evidence coverage differs")
    for label, record in release.items():
        if (
            not isinstance(record, dict)
            or set(record) != {"path", "sha256"}
            or not isinstance(record["path"], str)
            or Path(record["path"]).name != record["path"]
            or HEX64.fullmatch(record["sha256"]) is None
        ):
            raise GateFailure(f"publication evidence digest is malformed: {label}")
    external_ci = value.get("external_ci")
    if not isinstance(external_ci, dict) or set(external_ci) != {
        "security_hard_kill", "mcp_real_hosts"
    }:
        raise GateFailure("publication external CI evidence coverage differs")
    for label, record in external_ci.items():
        expected_path = {
            "security_hard_kill": "security-hard-kill",
            "mcp_real_hosts": "mcp-real-hosts",
        }[label]
        if (
            not isinstance(record, dict)
            or set(record) != {"path", "sha256"}
            or record["path"] != expected_path
            or HEX64.fullmatch(record["sha256"]) is None
        ):
            raise GateFailure(f"publication external CI evidence is malformed: {label}")


def recheck_live_authority(
    *,
    ecosystem: str,
    root: Path,
    control_root: Path,
    source_tag: str,
    repository: str,
    workflow_sha: str,
    workflow_run_id: str,
    token: str,
    authority_path: Path,
    evidence_root: Path,
    evidence_path: Path,
) -> None:
    policy = _policy(control_root)
    authority = validate_authority_receipt(
        _load_json(authority_path, "GitHub authority receipt"), ecosystem, root, policy
    )
    evidence = _load_json(evidence_path, "publication evidence receipt")
    validate_evidence_receipt(evidence, ecosystem, authority)
    fresh = resolve_live_authority(
        ecosystem=ecosystem,
        root=root,
        control_root=control_root,
        source_tag=source_tag,
        repository=repository,
        workflow_sha=workflow_sha,
        workflow_run_id=workflow_run_id,
        token=token,
    )
    if fresh != authority:
        raise GateFailure("GitHub authority changed after evidence verification")
    _control_digests_match(
        control_root,
        root,
        workflow_sha,
        policy,
        authority["control"]["files"],
    )
    refreshed = validate_live_evidence(
        ecosystem, root, control_root, evidence_root, authority_path
    )
    if refreshed != evidence:
        raise GateFailure("release evidence changed after verification")


def validate_publish_command(
    ecosystem: str,
    command: list[str],
    root: Path = ROOT,
    *,
    authority_receipt: Path | None = None,
    evidence_root: Path | None = None,
    evidence_receipt: Path | None = None,
) -> list[str]:
    failures: list[str] = []
    try:
        policy = _policy(ROOT)
        if authority_receipt is None or evidence_receipt is None or evidence_root is None:
            raise GateFailure("live command requires external authority and evidence receipts")
        authority = validate_authority_receipt(
            _load_json(authority_receipt, "GitHub authority receipt"), ecosystem, root, policy
        )
        _control_digests_match(
            ROOT,
            root,
            authority["control"]["workflow_sha"],
            policy,
            authority["control"]["files"],
        )
        evidence = _load_json(evidence_receipt, "publication evidence receipt")
        validate_evidence_receipt(evidence, ecosystem, authority)
        for label, record in evidence["release"].items():
            directory = {
                "release_evidence": "release-candidate",
                "required_checks": "release-candidate",
                "signed_release": "signed-release",
                "signed_release_receipt": "signed-release",
                "g8_aggregate": "g8",
            }[label]
            if _artifact_digest(evidence_root / directory / record["path"]) != record["sha256"]:
                raise GateFailure(f"live evidence bytes changed: {label}")
        for label, record in evidence["external_ci"].items():
            if _directory_digest(evidence_root / record["path"]) != record["sha256"]:
                raise GateFailure(f"live external CI evidence bytes changed: {label}")
    except (OSError, UnicodeError, json.JSONDecodeError, GateFailure) as error:
        failures.append(str(error))
    expected_prefix = (
        ["cargo", "publish", "--locked", "-p"]
        if ecosystem == "crates-io"
        else ["npm", "publish"]
    )
    if command[: len(expected_prefix)] != expected_prefix:
        failures.append(f"{ecosystem}: publish command prefix differs")
    if ecosystem == "npm" and command[3:] != ["--provenance", "--access", "public"]:
        failures.append("npm: publish command must require provenance and public access")
    if ecosystem == "crates-io" and len(command) == 5:
        try:
            release = _load_json(
                root / "config/crates-io-release.json", "crates.io release config"
            )
            allowed = {package for layer in release["layers"] for package in layer}
        except (OSError, UnicodeError, json.JSONDecodeError, KeyError, TypeError, GateFailure):
            allowed = set()
        if command[-1] not in allowed:
            failures.append("crates-io: package is outside the exact release closure")
    elif ecosystem == "crates-io":
        failures.append("crates-io: publish command shape differs")
    if ecosystem == "npm" and len(command) == 6:
        if command[2] not in {f"./{path}" for path, _name in EXPECTED_NPM_PACKAGES}:
            failures.append("npm: package path is outside the exact release inventory")
    elif ecosystem == "npm":
        failures.append("npm: publish command shape differs")
    if "--dry-run" in command:
        failures.append(f"{ecosystem}: live command cannot be a dry-run")
    return failures


def guarded_publish_command(command: list[str]) -> list[str]:
    """Remove argparse's one option separator without rewriting the command."""
    if command[:1] == ["--"]:
        return command[1:]
    return command


def parse_arguments(
    arguments: list[str] | None = None,
) -> tuple[argparse.ArgumentParser, argparse.Namespace]:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ecosystem", choices=("crates-io", "npm"), required=True)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--live-resolve", action="store_true")
    parser.add_argument("--live-evidence", action="store_true")
    parser.add_argument("--live-recheck", action="store_true")
    parser.add_argument("--source-tag")
    parser.add_argument("--repository")
    parser.add_argument("--workflow-sha")
    parser.add_argument("--workflow-run-id")
    parser.add_argument("--authority-receipt", type=Path)
    parser.add_argument("--evidence-root", type=Path)
    parser.add_argument("--evidence-receipt", type=Path)
    parser.add_argument("--publication-state", type=Path)
    parser.add_argument("--wait-layer", type=int)
    parser.add_argument("--github-output", type=Path)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    parsed = parser.parse_args(arguments)
    parsed.command = guarded_publish_command(parsed.command)
    return parser, parsed


def main() -> int:
    parser, arguments = parse_arguments()
    modes = sum(
        (
            arguments.dry_run,
            arguments.live_resolve,
            arguments.live_evidence,
            arguments.live_recheck,
            arguments.wait_layer is not None,
        )
    )
    try:
        failures = validate_publish_workflow(ROOT)
        if failures:
            raise GateFailure("; ".join(failures))
        if arguments.command:
            if modes:
                parser.error("guarded commands cannot be combined with another mode")
            failures = validate_publish_command(
                arguments.ecosystem,
                arguments.command,
                arguments.root,
                authority_receipt=arguments.authority_receipt,
                evidence_root=arguments.evidence_root,
                evidence_receipt=arguments.evidence_receipt,
            )
            if failures:
                raise GateFailure("; ".join(failures))
            if arguments.publication_state is None:
                parser.error("guarded live commands require --publication-state")
            policy = _policy(ROOT)
            authority = validate_authority_receipt(
                _load_json(arguments.authority_receipt, "GitHub authority receipt"),
                arguments.ecosystem,
                arguments.root,
                policy,
            )
            result = reconcile_publish_command(
                arguments.ecosystem,
                arguments.command,
                arguments.root,
                authority,
                arguments.publication_state,
            )
            print(
                f"{arguments.ecosystem} registry artifact {result['name']} "
                f"{result['outcome']}"
            )
            return 0
        if arguments.wait_layer is not None:
            if arguments.ecosystem != "crates-io" or arguments.publication_state is None:
                parser.error("--wait-layer requires crates-io and --publication-state")
            policy = _policy(ROOT)
            authority = validate_authority_receipt(
                _load_json(arguments.authority_receipt, "GitHub authority receipt"),
                arguments.ecosystem,
                arguments.root,
                policy,
            )
            wait_for_completed_layer(
                arguments.ecosystem,
                arguments.wait_layer,
                arguments.root,
                authority,
                arguments.publication_state,
            )
            print(f"crates.io layer {arguments.wait_layer} propagation passed")
            return 0
        if modes > 1:
            parser.error("publication modes are mutually exclusive")
        if arguments.dry_run:
            failures = validate_publish_authority(
                arguments.ecosystem, arguments.root, dry_run=True
            )
            if failures:
                raise GateFailure("; ".join(failures))
            print(f"{arguments.ecosystem} dry-run authority passed")
            return 0
        required_live = (
            arguments.source_tag,
            arguments.repository,
            arguments.workflow_sha,
            arguments.workflow_run_id,
            arguments.authority_receipt,
        )
        if not all(required_live):
            parser.error("live modes require tag, repository, workflow, run, and receipt inputs")
        token = os.environ.get("GITHUB_TOKEN", "")
        control_root = ROOT
        if arguments.live_resolve:
            authority = resolve_live_authority(
                ecosystem=arguments.ecosystem,
                root=arguments.root,
                control_root=control_root,
                source_tag=str(arguments.source_tag),
                repository=str(arguments.repository),
                workflow_sha=str(arguments.workflow_sha),
                workflow_run_id=str(arguments.workflow_run_id),
                token=token,
            )
            _write_new_json(arguments.authority_receipt, authority)
            if arguments.github_output is not None:
                release_run = next(
                    row["workflow_run_id"]
                    for row in authority["checks"]
                    if row["name"] == "Publish GitHub release"
                )
                g8_run = next(
                    row["workflow_run_id"]
                    for row in authority["checks"]
                    if row["name"] == "Validate all exact-SHA G8 receipts"
                )
                ci_run = next(
                    row["workflow_run_id"]
                    for row in authority["checks"]
                    if row["name"] == "MCP real hosts"
                )
                _write_outputs(
                    arguments.github_output,
                    {
                        "source_commit": authority["source"]["commit"],
                        "release_run_id": str(release_run),
                        "g8_run_id": str(g8_run),
                        "ci_run_id": str(ci_run),
                    },
                )
        elif arguments.live_evidence:
            if arguments.evidence_root is None or arguments.evidence_receipt is None:
                parser.error("--live-evidence requires evidence root and receipt")
            evidence = validate_live_evidence(
                arguments.ecosystem,
                arguments.root,
                control_root,
                arguments.evidence_root,
                arguments.authority_receipt,
            )
            _write_new_json(arguments.evidence_receipt, evidence)
        elif arguments.live_recheck:
            if arguments.evidence_root is None or arguments.evidence_receipt is None:
                parser.error("--live-recheck requires evidence root and receipt")
            recheck_live_authority(
                ecosystem=arguments.ecosystem,
                root=arguments.root,
                control_root=control_root,
                source_tag=str(arguments.source_tag),
                repository=str(arguments.repository),
                workflow_sha=str(arguments.workflow_sha),
                workflow_run_id=str(arguments.workflow_run_id),
                token=token,
                authority_path=arguments.authority_receipt,
                evidence_root=arguments.evidence_root,
                evidence_path=arguments.evidence_receipt,
            )
        else:
            parser.error("select --dry-run or an explicit live mode")
    except (
        OSError,
        UnicodeError,
        json.JSONDecodeError,
        subprocess.SubprocessError,
        urllib.error.URLError,
        GateFailure,
        ValueError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"{arguments.ecosystem} live authority passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
