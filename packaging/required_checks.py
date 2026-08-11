#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fetch and validate the hosted checks bound to a release commit."""

from __future__ import annotations

import argparse
import json
import os
import re
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REPOSITORY_SLUG = "celiumsai/hyphae"
REPORT_SCHEMA = "required-checks-report-v1"
REPORT_SCHEMA_VERSION = 1
REPORT_SUFFIX = ".required-checks.json"
REPORT_SCHEMA_PATH = Path(__file__).with_name(
    "required-checks-report-v1.schema.json"
)
GITHUB_ACTIONS_APP_ID = 15368
GITHUB_ACTIONS_APP_SLUG = "github-actions"
CANONICAL_BASE_REF = "main"
HEX_COMMIT = re.compile(r"[0-9a-f]{40}")
POSITIVE_DECIMAL = re.compile(r"[1-9][0-9]*")
HEAD_REF = re.compile(r"[0-9A-Za-z._/-]+")
GITHUB_TIMESTAMP = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z")
REQUIRED_CHECKS = (
    ("Assemble and verify release candidate", ".github/workflows/release.yml"),
    ("Bounded parser fuzzing", ".github/workflows/fuzz.yml"),
    ("Dependency and license policy", ".github/workflows/security.yml"),
    ("Load and kill/restart soak", ".github/workflows/stress.yml"),
    ("Optional framework integrations", ".github/workflows/ci.yml"),
    ("Package aarch64-apple-darwin", ".github/workflows/release.yml"),
    ("Package x86_64-apple-darwin", ".github/workflows/release.yml"),
    ("Package x86_64-pc-windows-msvc", ".github/workflows/release.yml"),
    ("Package x86_64-unknown-linux-gnu", ".github/workflows/release.yml"),
    ("Public client conformance", ".github/workflows/ci.yml"),
    ("Quality", ".github/workflows/ci.yml"),
    ("Release readiness", ".github/workflows/ci.yml"),
    ("Review dependency changes", ".github/workflows/dependency-review.yml"),
    ("Test (Linux MSRV)", ".github/workflows/ci.yml"),
    ("Test (Linux stable)", ".github/workflows/ci.yml"),
    ("Test (Windows stable)", ".github/workflows/ci.yml"),
    ("Test (macOS stable)", ".github/workflows/ci.yml"),
    ("Validate all exact-SHA G8 receipts", ".github/workflows/native-g8-closure.yml"),
)
REQUIRED_CHECK_NAMES = tuple(name for name, _ in REQUIRED_CHECKS)
REQUIRED_CHECK_WORKFLOWS = dict(REQUIRED_CHECKS)
REQUIRED_CHECK_EVENTS = {
    name: "workflow_dispatch" if name == "Validate all exact-SHA G8 receipts" else "pull_request"
    for name in REQUIRED_CHECK_NAMES
}
INTEGRATION_GUARD_STEP = "Verify the pull-request integration tree"
INTEGRATION_GUARD_CHECKS = frozenset(
    {
        "Assemble and verify release candidate",
        "Bounded parser fuzzing",
        "Dependency and license policy",
        "Load and kill/restart soak",
        "Quality",
        "Review dependency changes",
    }
)
PROHIBITED_BASE_EVENTS = frozenset(
    {
        "automatic_base_change_succeeded",
        "base_ref_changed",
    }
)


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


def check_run_url(
    repository: str,
    workflow_run_id: int,
    check_run_id: int,
) -> str:
    return (
        f"https://github.com/{repository}/actions/runs/"
        f"{workflow_run_id}/job/{check_run_id}"
    )


def check_run_url_identity(
    value: object,
    *,
    repository: str,
    check_run_id: int,
) -> tuple[int, str]:
    if not isinstance(value, str):
        raise ValueError("check run URL must be a string")
    pattern = re.compile(
        rf"https://github\.com/{re.escape(repository)}/actions/runs/"
        rf"([1-9][0-9]*)/job/([1-9][0-9]*)"
    )
    match = pattern.fullmatch(value)
    if match is None or int(match.group(2)) != check_run_id:
        raise ValueError("check run URL is not canonical or is bound to another check")
    workflow_run_id = int(match.group(1))
    return workflow_run_id, value


def parse_github_timestamp(value: object, label: str) -> tuple[str, datetime]:
    if not isinstance(value, str) or GITHUB_TIMESTAMP.fullmatch(value) is None:
        raise ValueError(f"{label} must be a canonical GitHub UTC timestamp")
    parsed = datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
        tzinfo=timezone.utc
    )
    return value, parsed


def check_run_times(
    source: dict[str, Any],
    label: str,
) -> tuple[str, str, datetime]:
    if source.get("status") != "completed":
        raise ValueError(f"{label} is not completed")
    started_at, started = parse_github_timestamp(
        source.get("started_at"),
        f"{label} started_at",
    )
    completed_at, completed = parse_github_timestamp(
        source.get("completed_at"),
        f"{label} completed_at",
    )
    if completed < started:
        raise ValueError(f"{label} completed before it started")
    return started_at, completed_at, completed


def select_check_runs(
    check_runs: list[object],
    *,
    commit: str,
    excluded_run_id: str,
) -> list[dict[str, Any]]:
    if HEX_COMMIT.fullmatch(commit) is None:
        raise ValueError("required-check selection commit must be a Git object ID")
    if POSITIVE_DECIMAL.fullmatch(excluded_run_id) is None:
        raise ValueError("excluded workflow run ID must be a positive decimal string")
    excluded_fragment = f"/actions/runs/{excluded_run_id}/"
    selected: list[dict[str, Any]] = []
    for name in REQUIRED_CHECK_NAMES:
        relevant: list[tuple[datetime, dict[str, Any]]] = []
        for check_run in check_runs:
            if not isinstance(check_run, dict):
                continue
            app = check_run.get("app")
            if (
                check_run.get("name") == name
                and check_run.get("head_sha") == commit
                and isinstance(app, dict)
                and app.get("id") == GITHUB_ACTIONS_APP_ID
                and app.get("slug") == GITHUB_ACTIONS_APP_SLUG
                and isinstance(check_run.get("id"), int)
                and not isinstance(check_run.get("id"), bool)
                and excluded_fragment not in str(check_run.get("details_url", ""))
            ):
                _, _, completed_at = check_run_times(
                    check_run,
                    f"check run {name}",
                )
                relevant.append((completed_at, check_run))
        if not relevant:
            raise ValueError(f"required check lacks a prior run for {commit}: {name}")
        latest_completed_at = max(item[0] for item in relevant)
        latest_runs = [
            check_run
            for completed_at, check_run in relevant
            if completed_at == latest_completed_at
        ]
        if len(latest_runs) != 1:
            raise ValueError(
                f"required check has an ambiguous latest completion for {commit}: {name}"
            )
        selected.append(latest_runs[0])
    return selected


def canonical_pull_request(
    pull_requests: list[object],
    *,
    repository: str,
    commit: str,
) -> dict[str, object]:
    if len(pull_requests) != 1:
        raise ValueError(
            "release commit must be associated with exactly one pull request"
        )
    source = require_object(pull_requests[0], "release pull request")
    number = source.get("number")
    head = require_object(source.get("head"), "release pull request head")
    base = require_object(source.get("base"), "release pull request base")
    head_repository = require_object(
        head.get("repo"),
        "release pull request head repository",
    )
    base_repository = require_object(
        base.get("repo"),
        "release pull request base repository",
    )
    head_ref = head.get("ref")
    base_sha = base.get("sha")
    merge_commit_sha = source.get("merge_commit_sha")
    merged_at, _ = parse_github_timestamp(
        source.get("merged_at"),
        "release pull request merged_at",
    )
    if (
        isinstance(number, bool)
        or not isinstance(number, int)
        or number < 1
        or source.get("state") != "closed"
        or head_repository.get("full_name") != repository
        or base_repository.get("full_name") != repository
        or not isinstance(head_ref, str)
        or HEAD_REF.fullmatch(head_ref) is None
        or head.get("sha") != commit
        or base.get("ref") != CANONICAL_BASE_REF
        or not isinstance(base_sha, str)
        or HEX_COMMIT.fullmatch(base_sha) is None
        or not isinstance(merge_commit_sha, str)
        or HEX_COMMIT.fullmatch(merge_commit_sha) is None
    ):
        raise ValueError(
            "release commit pull request is not a canonical merged main PR"
        )
    return {
        "number": number,
        "head_ref": head_ref,
        "head_sha": commit,
        "base_ref": CANONICAL_BASE_REF,
        "base_sha": base_sha,
        "merge_commit_sha": merge_commit_sha,
        "merged_at": merged_at,
        "base_ref_history": "unchanged",
    }


def require_stable_base_events(events: list[object]) -> None:
    for index, event in enumerate(events):
        source = require_object(event, f"pull request issue events[{index}]")
        if source.get("event") in PROHIBITED_BASE_EVENTS:
            raise ValueError(
                "release pull request base ref changed during its history"
            )


def require_unique_head_pull_request(
    pull_request: dict[str, object],
    head_pull_requests: list[object],
    *,
    repository: str,
    commit: str,
) -> None:
    head_pull_request = canonical_pull_request(
        head_pull_requests,
        repository=repository,
        commit=commit,
    )
    if head_pull_request != pull_request:
        raise ValueError(
            "release head branch history differs from its associated pull request"
        )


def canonical_job_run(
    job_run: object,
    *,
    check_run_id: int,
    workflow_run_id: int,
    workflow_run_attempt: int,
    expected_name: str,
    commit: str,
) -> dict[str, Any]:
    source = require_object(job_run, f"job run {check_run_id}")
    run_attempt = source.get("run_attempt")
    if (
        source.get("id") != check_run_id
        or source.get("run_id") != workflow_run_id
        or source.get("name") != expected_name
        or source.get("head_sha") != commit
        or source.get("status") != "completed"
        or source.get("conclusion") != "success"
        or isinstance(run_attempt, bool)
        or not isinstance(run_attempt, int)
        or run_attempt < 1
    ):
        raise ValueError(
            f"job run {check_run_id} differs from its successful check"
        )
    if run_attempt != workflow_run_attempt:
        raise ValueError(
            f"job run {check_run_id} attempt differs from workflow run "
            f"{workflow_run_id}"
        )
    return source


def canonical_integration_guard(
    job_run: dict[str, Any],
    *,
    check_run_id: int,
) -> dict[str, object]:
    steps = job_run.get("steps")
    if not isinstance(steps, list):
        raise ValueError(f"job run {check_run_id} lacks step evidence")
    guards = [
        require_object(step, f"job run {check_run_id} integration guard")
        for step in steps
        if isinstance(step, dict) and step.get("name") == INTEGRATION_GUARD_STEP
    ]
    if len(guards) != 1:
        raise ValueError(
            f"job run {check_run_id} must execute exactly one integration guard"
        )
    guard = guards[0]
    number = guard.get("number")
    if (
        isinstance(number, bool)
        or not isinstance(number, int)
        or number < 1
        or guard.get("status") != "completed"
        or guard.get("conclusion") != "success"
    ):
        raise ValueError(
            f"job run {check_run_id} integration guard is not successful"
        )
    return {
        "name": INTEGRATION_GUARD_STEP,
        "number": number,
        "status": "completed",
        "conclusion": "success",
    }


def canonical_workflow_run(
    workflow_run: object,
    *,
    workflow_run_id: int,
    repository: str,
    commit: str,
    expected_head_branch: str,
    expected_path: str,
    expected_event: str,
) -> dict[str, object]:
    source = require_object(workflow_run, f"workflow run {workflow_run_id}")
    source_repository = require_object(
        source.get("repository"),
        f"workflow run {workflow_run_id} repository",
    )
    head_repository = require_object(
        source.get("head_repository"),
        f"workflow run {workflow_run_id} head repository",
    )
    source_id = source.get("id")
    run_attempt = source.get("run_attempt")
    if (
        isinstance(source_id, bool)
        or not isinstance(source_id, int)
        or source_id != workflow_run_id
    ):
        raise ValueError(f"workflow run {workflow_run_id} ID differs from its check")
    if source.get("path") != expected_path:
        raise ValueError(
            f"workflow run {workflow_run_id} path is not canonical: {expected_path}"
        )
    if source.get("head_sha") != commit:
        raise ValueError(
            f"workflow run {workflow_run_id} head_sha differs from {commit}"
        )
    if source.get("status") != "completed" or source.get("conclusion") != "success":
        raise ValueError(f"workflow run {workflow_run_id} is not successful")
    if (
        source.get("event") != expected_event
        or source.get("head_branch") != expected_head_branch
        or source_repository.get("full_name") != repository
        or head_repository.get("full_name") != repository
        or isinstance(run_attempt, bool)
        or not isinstance(run_attempt, int)
        or run_attempt < 1
    ):
        raise ValueError(
            f"workflow run {workflow_run_id} is not bound to the release pull request"
        )
    return {
        "path": expected_path,
        "event": expected_event,
        "head_branch": expected_head_branch,
        "run_attempt": run_attempt,
    }


def canonical_check_run(
    check_run: object,
    *,
    workflow_runs: dict[int, object],
    job_runs: dict[int, object],
    repository: str,
    commit: str,
    expected_head_branch: str,
    expected_name: str,
) -> dict[str, object]:
    source = require_object(check_run, f"check run {expected_name}")
    app = require_object(source.get("app"), f"check run {expected_name} app")
    check_run_id = source.get("id")
    if (
        isinstance(check_run_id, bool)
        or not isinstance(check_run_id, int)
        or check_run_id < 1
    ):
        raise ValueError(f"check run {expected_name} has an invalid ID")
    if (
        source.get("name") != expected_name
        or source.get("head_sha") != commit
        or source.get("status") != "completed"
        or source.get("conclusion") != "success"
    ):
        raise ValueError(f"check run {expected_name} is not successful for {commit}")
    if (
        app.get("id") != GITHUB_ACTIONS_APP_ID
        or app.get("slug") != GITHUB_ACTIONS_APP_SLUG
    ):
        raise ValueError(f"check run {expected_name} is not from GitHub Actions")
    workflow_run_id, canonical_url = check_run_url_identity(
        source.get("html_url"),
        repository=repository,
        check_run_id=check_run_id,
    )
    if source.get("details_url") != canonical_url:
        raise ValueError(
            f"check run {expected_name} details URL differs from its canonical URL"
        )
    if workflow_run_id not in workflow_runs:
        raise ValueError(
            f"workflow run metadata is missing for check run {expected_name}"
        )
    workflow = canonical_workflow_run(
        workflow_runs[workflow_run_id],
        workflow_run_id=workflow_run_id,
        repository=repository,
        commit=commit,
        expected_head_branch=expected_head_branch,
        expected_path=REQUIRED_CHECK_WORKFLOWS[expected_name],
        expected_event=REQUIRED_CHECK_EVENTS[expected_name],
    )
    if check_run_id not in job_runs:
        raise ValueError(f"job metadata is missing for check run {expected_name}")
    job = canonical_job_run(
        job_runs[check_run_id],
        check_run_id=check_run_id,
        workflow_run_id=workflow_run_id,
        workflow_run_attempt=int(workflow["run_attempt"]),
        expected_name=expected_name,
        commit=commit,
    )
    started_at, completed_at, _ = check_run_times(
        source,
        f"check run {expected_name}",
    )
    integration_guard = None
    if expected_name in INTEGRATION_GUARD_CHECKS:
        integration_guard = canonical_integration_guard(
            job,
            check_run_id=check_run_id,
        )
    return {
        "name": expected_name,
        "check_run_id": check_run_id,
        "workflow_run_id": workflow_run_id,
        "workflow_path": workflow["path"],
        "workflow_event": workflow["event"],
        "workflow_run_attempt": job["run_attempt"],
        "head_branch": workflow["head_branch"],
        "check_run_url": canonical_url,
        "app_id": GITHUB_ACTIONS_APP_ID,
        "app_slug": GITHUB_ACTIONS_APP_SLUG,
        "head_sha": commit,
        "status": "completed",
        "conclusion": "success",
        "started_at": started_at,
        "completed_at": completed_at,
        "integration_guard": integration_guard,
    }


def build_report(
    check_runs: list[object],
    *,
    workflow_runs: dict[int, object],
    job_runs: dict[int, object],
    pull_requests: list[object],
    head_pull_requests: list[object],
    pull_request_events: list[object],
    repository: str,
    commit: str,
    excluded_run_id: str,
) -> dict[str, object]:
    if repository != REPOSITORY_SLUG:
        raise ValueError("required-check report repository is not canonical")
    if HEX_COMMIT.fullmatch(commit) is None:
        raise ValueError("required-check report commit must be a Git object ID")
    if POSITIVE_DECIMAL.fullmatch(excluded_run_id) is None:
        raise ValueError("excluded workflow run ID must be a positive decimal string")
    pull_request = canonical_pull_request(
        pull_requests,
        repository=repository,
        commit=commit,
    )
    require_unique_head_pull_request(
        pull_request,
        head_pull_requests,
        repository=repository,
        commit=commit,
    )
    require_stable_base_events(pull_request_events)
    selected = [
        canonical_check_run(
            check_run,
            workflow_runs=workflow_runs,
            job_runs=job_runs,
            repository=repository,
            commit=commit,
            expected_head_branch=str(pull_request["head_ref"]),
            expected_name=name,
        )
        for name, check_run in zip(
            REQUIRED_CHECK_NAMES,
            select_check_runs(
                check_runs,
                commit=commit,
                excluded_run_id=excluded_run_id,
            ),
            strict=True,
        )
    ]
    report: dict[str, object] = {
        "schema": REPORT_SCHEMA,
        "schema_version": REPORT_SCHEMA_VERSION,
        "repository": repository,
        "head_sha": commit,
        "pull_request": pull_request,
        "checks": selected,
    }
    validate_report(report, expected_commit=commit)
    return report


def validate_report(document: object, *, expected_commit: str) -> None:
    root = require_object(document, "required-check report")
    require_exact_keys(
        root,
        {
            "schema",
            "schema_version",
            "repository",
            "head_sha",
            "pull_request",
            "checks",
        },
        "required-check report",
    )
    if (
        root["schema"] != REPORT_SCHEMA
        or root["schema_version"] != REPORT_SCHEMA_VERSION
        or root["repository"] != REPOSITORY_SLUG
    ):
        raise ValueError("required-check report identity is not canonical")
    if (
        HEX_COMMIT.fullmatch(expected_commit) is None
        or root["head_sha"] != expected_commit
    ):
        raise ValueError("required-check report head_sha differs from the release commit")
    pull_request = require_object(root["pull_request"], "pull_request")
    require_exact_keys(
        pull_request,
        {
            "number",
            "head_ref",
            "head_sha",
            "base_ref",
            "base_sha",
            "merge_commit_sha",
            "merged_at",
            "base_ref_history",
        },
        "pull_request",
    )
    number = pull_request["number"]
    head_ref = pull_request["head_ref"]
    try:
        merged_at, _ = parse_github_timestamp(
            pull_request["merged_at"],
            "pull_request.merged_at",
        )
    except ValueError as error:
        raise ValueError("required-check report pull request is invalid") from error
    if (
        isinstance(number, bool)
        or not isinstance(number, int)
        or number < 1
        or not isinstance(head_ref, str)
        or HEAD_REF.fullmatch(head_ref) is None
        or pull_request["head_sha"] != expected_commit
        or pull_request["base_ref"] != CANONICAL_BASE_REF
        or not isinstance(pull_request["base_sha"], str)
        or HEX_COMMIT.fullmatch(pull_request["base_sha"]) is None
        or not isinstance(pull_request["merge_commit_sha"], str)
        or HEX_COMMIT.fullmatch(pull_request["merge_commit_sha"]) is None
        or pull_request["merged_at"] != merged_at
        or pull_request["base_ref_history"] != "unchanged"
    ):
        raise ValueError("required-check report pull request is invalid")
    checks = root["checks"]
    if not isinstance(checks, list) or len(checks) != len(REQUIRED_CHECK_NAMES):
        raise ValueError("required-check report must contain exactly 18 checks")
    seen_ids: set[int] = set()
    seen_workflow_runs: dict[int, tuple[str, int]] = {}
    workflow_run_by_path: dict[str, int] = {}
    for index, (check, expected_name) in enumerate(
        zip(checks, REQUIRED_CHECK_NAMES, strict=True)
    ):
        record = require_object(check, f"checks[{index}]")
        require_exact_keys(
            record,
            {
                "name",
                "check_run_id",
                "workflow_run_id",
                "workflow_run_attempt",
                "workflow_path",
                "workflow_event",
                "check_run_url",
                "app_id",
                "app_slug",
                "head_sha",
                "head_branch",
                "status",
                "conclusion",
                "started_at",
                "completed_at",
                "integration_guard",
            },
            f"checks[{index}]",
        )
        check_run_id = record["check_run_id"]
        workflow_run_id = record["workflow_run_id"]
        workflow_run_attempt = record["workflow_run_attempt"]
        workflow_path = record["workflow_path"]
        integration_guard = record["integration_guard"]
        try:
            url_workflow_run_id, _ = check_run_url_identity(
                record["check_run_url"],
                repository=REPOSITORY_SLUG,
                check_run_id=check_run_id,
            )
        except ValueError as error:
            raise ValueError(
                f"required check record is invalid: {expected_name}"
            ) from error
        try:
            started_at, started = parse_github_timestamp(
                record["started_at"],
                f"required check {expected_name} started_at",
            )
            completed_at, completed = parse_github_timestamp(
                record["completed_at"],
                f"required check {expected_name} completed_at",
            )
        except ValueError as error:
            raise ValueError(
                f"required check record is invalid: {expected_name}"
            ) from error
        if (
            record["name"] != expected_name
            or record["head_sha"] != expected_commit
            or record["status"] != "completed"
            or record["conclusion"] != "success"
            or isinstance(check_run_id, bool)
            or not isinstance(check_run_id, int)
            or check_run_id < 1
            or check_run_id in seen_ids
            or isinstance(workflow_run_id, bool)
            or not isinstance(workflow_run_id, int)
            or workflow_run_id < 1
            or workflow_run_id != url_workflow_run_id
            or isinstance(workflow_run_attempt, bool)
            or not isinstance(workflow_run_attempt, int)
            or workflow_run_attempt < 1
            or workflow_path != REQUIRED_CHECK_WORKFLOWS[expected_name]
            or record["workflow_event"] != REQUIRED_CHECK_EVENTS[expected_name]
            or record["head_branch"] != head_ref
            or (
                workflow_run_id in seen_workflow_runs
                and seen_workflow_runs[workflow_run_id]
                != (workflow_path, workflow_run_attempt)
            )
            or (
                workflow_path in workflow_run_by_path
                and workflow_run_by_path[workflow_path] != workflow_run_id
            )
            or record["app_id"] != GITHUB_ACTIONS_APP_ID
            or record["app_slug"] != GITHUB_ACTIONS_APP_SLUG
            or record["started_at"] != started_at
            or record["completed_at"] != completed_at
            or completed < started
        ):
            raise ValueError(f"required check record is invalid: {expected_name}")
        expects_integration_guard = expected_name in INTEGRATION_GUARD_CHECKS
        if expects_integration_guard:
            try:
                guard = require_object(
                    integration_guard,
                    f"required check {expected_name} integration_guard",
                )
                require_exact_keys(
                    guard,
                    {"name", "number", "status", "conclusion"},
                    f"required check {expected_name} integration_guard",
                )
            except ValueError as error:
                raise ValueError(
                    f"required check record is invalid: {expected_name}"
                ) from error
            guard_number = guard["number"]
            if (
                guard["name"] != INTEGRATION_GUARD_STEP
                or isinstance(guard_number, bool)
                or not isinstance(guard_number, int)
                or guard_number < 1
                or guard["status"] != "completed"
                or guard["conclusion"] != "success"
            ):
                raise ValueError(
                    f"required check record is invalid: {expected_name}"
                )
        elif integration_guard is not None:
            raise ValueError(f"required check record is invalid: {expected_name}")
        seen_ids.add(check_run_id)
        seen_workflow_runs[workflow_run_id] = (
            workflow_path,
            workflow_run_attempt,
        )
        workflow_run_by_path[workflow_path] = workflow_run_id


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_report(path: Path, *, expected_commit: str) -> object:
    if not path.is_file():
        raise ValueError(f"required-check report is missing: {path.name}")
    document = json.loads(
        path.read_text("utf-8"),
        object_pairs_hook=reject_duplicate_keys,
    )
    validate_report(document, expected_commit=expected_commit)
    return document


def fetch_check_runs(
    *,
    repository: str,
    commit: str,
    token: str,
) -> list[object]:
    if repository != REPOSITORY_SLUG or HEX_COMMIT.fullmatch(commit) is None:
        raise ValueError("repository or commit is not canonical")
    if not token:
        raise ValueError("GITHUB_TOKEN is required")
    check_runs: list[object] = []
    for page in range(1, 101):
        query = urllib.parse.urlencode(
            {"filter": "all", "per_page": 100, "page": page}
        )
        url = (
            f"https://api.github.com/repos/{repository}/commits/{commit}"
            f"/check-runs?{query}"
        )
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "User-Agent": "hyphae-release-evidence",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.load(response)
        page_runs = payload.get("check_runs")
        if not isinstance(page_runs, list):
            raise ValueError("GitHub Checks API response lacks check_runs")
        check_runs.extend(page_runs)
        if len(page_runs) < 100:
            return check_runs
    raise RuntimeError("GitHub Checks API pagination exceeded 10,000 runs")


def fetch_workflow_runs(
    check_runs: list[object],
    *,
    repository: str,
    commit: str,
    excluded_run_id: str,
    token: str,
) -> dict[int, object]:
    if repository != REPOSITORY_SLUG or HEX_COMMIT.fullmatch(commit) is None:
        raise ValueError("repository or commit is not canonical")
    if POSITIVE_DECIMAL.fullmatch(excluded_run_id) is None:
        raise ValueError("excluded workflow run ID must be a positive decimal string")
    if not token:
        raise ValueError("GITHUB_TOKEN is required")
    excluded_fragment = f"/actions/runs/{excluded_run_id}/"
    workflow_run_ids: set[int] = set()
    for check_run in check_runs:
        if not isinstance(check_run, dict):
            continue
        app = check_run.get("app")
        check_run_id = check_run.get("id")
        details_url = check_run.get("details_url")
        if (
            check_run.get("name") not in REQUIRED_CHECK_WORKFLOWS
            or check_run.get("head_sha") != commit
            or not isinstance(app, dict)
            or app.get("id") != GITHUB_ACTIONS_APP_ID
            or app.get("slug") != GITHUB_ACTIONS_APP_SLUG
            or isinstance(check_run_id, bool)
            or not isinstance(check_run_id, int)
            or excluded_fragment in str(details_url)
        ):
            continue
        workflow_run_id, _ = check_run_url_identity(
            details_url,
            repository=repository,
            check_run_id=check_run_id,
        )
        workflow_run_ids.add(workflow_run_id)

    workflow_runs: dict[int, object] = {}
    for workflow_run_id in sorted(workflow_run_ids):
        url = (
            f"https://api.github.com/repos/{repository}/actions/runs/"
            f"{workflow_run_id}"
        )
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "User-Agent": "hyphae-release-evidence",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            workflow_runs[workflow_run_id] = json.load(response)
    return workflow_runs


def fetch_job_runs(
    check_runs: list[object],
    *,
    repository: str,
    commit: str,
    excluded_run_id: str,
    token: str,
) -> dict[int, object]:
    if repository != REPOSITORY_SLUG or HEX_COMMIT.fullmatch(commit) is None:
        raise ValueError("repository or commit is not canonical")
    if POSITIVE_DECIMAL.fullmatch(excluded_run_id) is None:
        raise ValueError("excluded workflow run ID must be a positive decimal string")
    if not token:
        raise ValueError("GITHUB_TOKEN is required")
    excluded_fragment = f"/actions/runs/{excluded_run_id}/"
    job_run_ids: set[int] = set()
    for check_run in check_runs:
        if not isinstance(check_run, dict):
            continue
        app = check_run.get("app")
        check_run_id = check_run.get("id")
        details_url = check_run.get("details_url")
        if (
            check_run.get("name") not in REQUIRED_CHECK_WORKFLOWS
            or check_run.get("head_sha") != commit
            or not isinstance(app, dict)
            or app.get("id") != GITHUB_ACTIONS_APP_ID
            or app.get("slug") != GITHUB_ACTIONS_APP_SLUG
            or isinstance(check_run_id, bool)
            or not isinstance(check_run_id, int)
            or excluded_fragment in str(details_url)
        ):
            continue
        check_run_url_identity(
            details_url,
            repository=repository,
            check_run_id=check_run_id,
        )
        job_run_ids.add(check_run_id)

    job_runs: dict[int, object] = {}
    for job_run_id in sorted(job_run_ids):
        url = (
            f"https://api.github.com/repos/{repository}/actions/jobs/"
            f"{job_run_id}"
        )
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "User-Agent": "hyphae-release-evidence",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            job_runs[job_run_id] = json.load(response)
    return job_runs


def fetch_pull_requests(
    *,
    repository: str,
    commit: str,
    token: str,
) -> list[object]:
    if repository != REPOSITORY_SLUG or HEX_COMMIT.fullmatch(commit) is None:
        raise ValueError("repository or commit is not canonical")
    if not token:
        raise ValueError("GITHUB_TOKEN is required")
    pull_requests: list[object] = []
    for page in range(1, 101):
        query = urllib.parse.urlencode({"per_page": 100, "page": page})
        url = (
            f"https://api.github.com/repos/{repository}/commits/{commit}"
            f"/pulls?{query}"
        )
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "User-Agent": "hyphae-release-evidence",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            page_pull_requests = json.load(response)
        if not isinstance(page_pull_requests, list):
            raise ValueError("GitHub commit pulls API response must be an array")
        pull_requests.extend(page_pull_requests)
        if len(page_pull_requests) < 100:
            return pull_requests
    raise RuntimeError("GitHub commit pulls API pagination exceeded 10,000 PRs")


def fetch_head_pull_requests(
    *,
    repository: str,
    head_ref: str,
    token: str,
) -> list[object]:
    if (
        repository != REPOSITORY_SLUG
        or HEAD_REF.fullmatch(head_ref) is None
    ):
        raise ValueError("repository or head ref is not canonical")
    if not token:
        raise ValueError("GITHUB_TOKEN is required")
    owner = repository.split("/", 1)[0]
    pull_requests: list[object] = []
    for page in range(1, 101):
        query = urllib.parse.urlencode(
            {
                "state": "all",
                "head": f"{owner}:{head_ref}",
                "per_page": 100,
                "page": page,
            }
        )
        url = f"https://api.github.com/repos/{repository}/pulls?{query}"
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "User-Agent": "hyphae-release-evidence",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            page_pull_requests = json.load(response)
        if not isinstance(page_pull_requests, list):
            raise ValueError("GitHub head pulls API response must be an array")
        pull_requests.extend(page_pull_requests)
        if len(page_pull_requests) < 100:
            return pull_requests
    raise RuntimeError("GitHub head pulls API pagination exceeded 10,000 PRs")


def fetch_pull_request_events(
    *,
    repository: str,
    number: int,
    token: str,
) -> list[object]:
    if (
        repository != REPOSITORY_SLUG
        or isinstance(number, bool)
        or not isinstance(number, int)
        or number < 1
    ):
        raise ValueError("repository or pull request number is not canonical")
    if not token:
        raise ValueError("GITHUB_TOKEN is required")
    events: list[object] = []
    for page in range(1, 101):
        query = urllib.parse.urlencode({"per_page": 100, "page": page})
        url = (
            f"https://api.github.com/repos/{repository}/issues/{number}"
            f"/events?{query}"
        )
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "User-Agent": "hyphae-release-evidence",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            page_events = json.load(response)
        if not isinstance(page_events, list):
            raise ValueError("GitHub pull request issue events response must be an array")
        events.extend(page_events)
        if len(page_events) < 100:
            return events
    raise RuntimeError("GitHub pull request issue events exceeded 10,000 entries")


def write_report(path: Path, document: object) -> None:
    with path.open("x", encoding="utf-8", newline="\n") as output:
        json.dump(document, output, indent=2, sort_keys=True)
        output.write("\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--exclude-run-id", required=True)
    parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()
    token = os.environ.get("GITHUB_TOKEN", "")
    check_runs = fetch_check_runs(
        repository=arguments.repository,
        commit=arguments.commit,
        token=token,
    )
    pull_requests = fetch_pull_requests(
        repository=arguments.repository,
        commit=arguments.commit,
        token=token,
    )
    pull_request = canonical_pull_request(
        pull_requests,
        repository=arguments.repository,
        commit=arguments.commit,
    )
    selected_check_runs = select_check_runs(
        check_runs,
        commit=arguments.commit,
        excluded_run_id=arguments.exclude_run_id,
    )
    report = build_report(
        selected_check_runs,
        workflow_runs=fetch_workflow_runs(
            selected_check_runs,
            repository=arguments.repository,
            commit=arguments.commit,
            excluded_run_id=arguments.exclude_run_id,
            token=token,
        ),
        job_runs=fetch_job_runs(
            selected_check_runs,
            repository=arguments.repository,
            commit=arguments.commit,
            excluded_run_id=arguments.exclude_run_id,
            token=token,
        ),
        pull_requests=pull_requests,
        head_pull_requests=fetch_head_pull_requests(
            repository=arguments.repository,
            head_ref=str(pull_request["head_ref"]),
            token=token,
        ),
        pull_request_events=fetch_pull_request_events(
            repository=arguments.repository,
            number=int(pull_request["number"]),
            token=token,
        ),
        repository=arguments.repository,
        commit=arguments.commit,
        excluded_run_id=arguments.exclude_run_id,
    )
    write_report(arguments.output, report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
