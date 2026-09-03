#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
import zipfile
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from package import INCLUDED_DOCUMENTS, build_archive, product_version
from finalize_release import (
    create_checksums,
    release_assets,
    require_matching_tag,
    require_apache_release_version as require_final_apache_release_version,
    validate_release_layout,
    verify_checksums,
)
from provenance import BUILD_TYPE, BUILDER_ID, TARGET_RUNNERS, build_predicate
from release_evidence import (
    SCHEMA_NAME,
    SCHEMA_PATH,
    TARGET_ARCHIVES,
    archive_name,
    build_release_evidence,
    evidence_name,
    required_checks_name,
    source_identity,
    validate_release_evidence,
    validate_release_evidence_file,
    write_release_evidence,
)
from required_checks import (
    GITHUB_ACTIONS_APP_ID,
    GITHUB_ACTIONS_APP_SLUG,
    INTEGRATION_GUARD_CHECKS,
    INTEGRATION_GUARD_STEP,
    REPORT_SCHEMA_PATH,
    REQUIRED_CHECK_EVENTS,
    REQUIRED_CHECK_NAMES,
    REQUIRED_CHECK_WORKFLOWS,
    REPOSITORY_SLUG,
    build_report,
    check_run_url,
    check_run_url_identity,
    fetch_check_runs,
    fetch_head_pull_requests,
    fetch_job_runs,
    fetch_pull_requests,
    fetch_pull_request_events,
    fetch_workflow_runs,
    validate_report,
    write_report,
)
from verify_install import extract_archive, require_command_failure


ROOT = Path(__file__).resolve().parents[1]


def git_object(revision: str) -> str:
    completed = subprocess.run(
        ("git", "rev-parse", "--verify", revision),
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def fixture_check_runs(
    commit: str,
    *,
    release_commit: str | None = None,
) -> list[dict[str, object]]:
    if release_commit is None:
        release_commit = commit
    runs: list[dict[str, object]] = []
    workflow_run_ids = {
        path: 30_000 + index
        for index, path in enumerate(dict.fromkeys(REQUIRED_CHECK_WORKFLOWS.values()))
    }
    for check_run_id, name in enumerate(REQUIRED_CHECK_NAMES, start=10_000):
        check_commit = (
            release_commit
            if name == "Validate all exact-SHA G8 receipts"
            else commit
        )
        workflow_run_id = workflow_run_ids[REQUIRED_CHECK_WORKFLOWS[name]]
        url = check_run_url(
            REPOSITORY_SLUG,
            workflow_run_id,
            check_run_id,
        )
        runs.append(
            {
                "id": check_run_id,
                "name": name,
                "html_url": url,
                "details_url": url,
                "head_sha": check_commit,
                "status": "completed",
                "conclusion": "success",
                "started_at": "2026-07-29T18:00:00Z",
                "completed_at": "2026-07-29T18:01:00Z",
                "app": {
                    "id": GITHUB_ACTIONS_APP_ID,
                    "slug": GITHUB_ACTIONS_APP_SLUG,
                },
            }
        )
    return runs


def fixture_workflow_runs(
    commit: str,
    check_runs: list[dict[str, object]],
) -> dict[int, object]:
    workflows: dict[int, object] = {}
    for check_run in check_runs:
        check_run_id = int(check_run["id"])
        workflow_run_id, _ = check_run_url_identity(
            check_run["details_url"],
            repository=REPOSITORY_SLUG,
            check_run_id=check_run_id,
        )
        workflows[workflow_run_id] = {
            "id": workflow_run_id,
            "path": REQUIRED_CHECK_WORKFLOWS[str(check_run["name"])],
            "head_sha": check_run["head_sha"],
            "head_branch": (
                "release/codex/release-candidate-merge-evidence"
                if check_run["name"] == "Validate all exact-SHA G8 receipts"
                else "codex/release-candidate"
            ),
            "event": REQUIRED_CHECK_EVENTS[str(check_run["name"])],
            "run_attempt": 1,
            "repository": {"full_name": REPOSITORY_SLUG},
            "head_repository": {"full_name": REPOSITORY_SLUG},
            "status": "completed",
            "conclusion": "success",
        }
    return workflows


def fixture_pull_requests(
    commit: str,
    *,
    merge_commit: str | None = None,
) -> list[dict[str, object]]:
    if merge_commit is None:
        merge_commit = commit
    return [
        {
            "number": 14,
            "state": "closed",
            "merged_at": "2026-07-29T18:25:42Z",
            "merge_commit_sha": merge_commit,
            "head": {
                "ref": "codex/release-candidate",
                "sha": commit,
                "repo": {"full_name": REPOSITORY_SLUG},
            },
            "base": {
                "ref": "main",
                "sha": "b" * 40,
                "repo": {"full_name": REPOSITORY_SLUG},
            },
        }
    ]


def fixture_job_runs(
    commit: str,
    check_runs: list[dict[str, object]],
) -> dict[int, object]:
    jobs: dict[int, object] = {}
    for check_run in check_runs:
        name = str(check_run["name"])
        check_run_id = int(check_run["id"])
        workflow_run_id, _ = check_run_url_identity(
            check_run["details_url"],
            repository=REPOSITORY_SLUG,
            check_run_id=check_run_id,
        )
        jobs[check_run_id] = {
            "id": check_run_id,
            "run_id": workflow_run_id,
            "run_attempt": 1,
            "name": name,
            "head_sha": check_run["head_sha"],
            "status": "completed",
            "conclusion": "success",
            "steps": (
                [
                    {
                        "number": 3,
                        "name": INTEGRATION_GUARD_STEP,
                        "status": "completed",
                        "conclusion": "success",
                    }
                ]
                if name in INTEGRATION_GUARD_CHECKS
                else []
            ),
        }
    return jobs


def write_test_primary_payloads(
    directory: Path,
    *,
    tag_release: bool,
    include_required_checks: bool | None = None,
    workflow_ref_override: str | None = None,
    event_override: str | None = None,
) -> tuple[object, str, str]:
    commit = git_object("HEAD^{commit}")
    identity = source_identity(commit)
    workflow_ref = (
        f"refs/tags/{identity.tag}"
        if tag_release
        else "refs/heads/release-candidate"
    )
    event = "push" if tag_release else "workflow_dispatch"
    if workflow_ref_override is not None:
        workflow_ref = workflow_ref_override
    if event_override is not None:
        event = event_override
    invocation_id = (
        "https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/123456/attempts/1"
    )
    for target in TARGET_ARCHIVES:
        archive = archive_name(identity.version, target)
        (directory / archive).write_bytes(f"archive:{target}".encode())
        predicate = build_predicate(
            target=target,
            commit=identity.commit,
            git_ref=workflow_ref,
            invocation_id=invocation_id,
            runner_os=TARGET_RUNNERS[target][0],
            runner_arch=TARGET_RUNNERS[target][1],
        )
        (directory / f"{archive}.provenance.json").write_text(
            json.dumps(predicate, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
            newline="\n",
        )
        (directory / f"{archive}.intoto.sigstore.json").write_text(
            "{}\n",
            encoding="utf-8",
        )
    for suffix in ("spdx.json", "cdx.json"):
        (directory / f"hyphae-{identity.tag}.{suffix}").write_text(
            "{}\n",
            encoding="utf-8",
        )
    should_include_checks = (
        tag_release
        if include_required_checks is None
        else include_required_checks
    )
    if should_include_checks:
        check_runs = fixture_check_runs(identity.commit)
        report = build_report(
            check_runs,
            workflow_runs=fixture_workflow_runs(identity.commit, check_runs),
            job_runs=fixture_job_runs(identity.commit, check_runs),
            pull_requests=fixture_pull_requests(identity.commit),
            head_pull_requests=fixture_pull_requests(identity.commit),
            pull_request_events=[],
            repository=REPOSITORY_SLUG,
            commit=identity.commit,
            excluded_run_id="999",
        )
        write_report(
            directory / required_checks_name(identity.tag),
            report,
        )
    return identity, workflow_ref, event


def add_test_release_evidence(
    directory: Path,
    *,
    tag_release: bool = False,
) -> Path:
    identity, workflow_ref, event = write_test_primary_payloads(
        directory=directory,
        tag_release=tag_release,
    )
    document = build_release_evidence(
        directory=directory,
        tag=identity.tag,
        commit=identity.commit,
        workflow_ref=workflow_ref,
        event=event,
        run_id="123456",
        run_attempt=1,
        tag_object=identity.commit if tag_release else None,
        tag_target=identity.commit if tag_release else None,
    )
    path = directory / evidence_name(identity.tag)
    write_release_evidence(path, document)
    return path


def add_final_signature_bundles(directory: Path) -> None:
    create_checksums(directory)
    ordinary = release_assets(directory) + [directory / "SHA256SUMS"]
    for artifact in ordinary:
        (directory / f"{artifact.name}.sigstore.json").write_text(
            "{}\n",
            encoding="utf-8",
        )
    identity = source_identity(git_object("HEAD^{commit}"))
    for target in TARGET_ARCHIVES:
        archive = archive_name(identity.version, target)
        for kind in ("spdx", "cyclonedx"):
            (directory / f"{archive}.{kind}.attestation.sigstore.json").write_text(
                "{}\n",
                encoding="utf-8",
            )


class PackageTests(unittest.TestCase):
    def test_apache_publication_is_blocked_until_version_1_2_2(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "3.0.0"):
            require_final_apache_release_version("1.2.1")
        require_final_apache_release_version("3.0.0")

    def test_release_candidate_versions_are_aligned(self) -> None:
        cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        version = cargo["workspace"]["package"]["version"]
        python = tomllib.loads(
            (ROOT / "sdks/python/pyproject.toml").read_text(encoding="utf-8")
        )
        typescript = json.loads(
            (ROOT / "sdks/typescript/package.json").read_text(encoding="utf-8")
        )
        integrations = json.loads(
            (ROOT / "integrations/javascript/package.json").read_text(
                encoding="utf-8"
            )
        )
        typescript_lock = json.loads(
            (ROOT / "sdks/typescript/package-lock.json").read_text(
                encoding="utf-8"
            )
        )
        integrations_lock = json.loads(
            (ROOT / "integrations/javascript/package-lock.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(version, "3.0.0")
        self.assertEqual(python["project"]["version"], version)
        self.assertEqual(typescript["version"], version)
        self.assertEqual(typescript_lock["version"], version)
        self.assertEqual(typescript_lock["packages"][""]["version"], version)
        self.assertEqual(integrations["version"], version)
        self.assertEqual(integrations_lock["version"], version)
        self.assertEqual(integrations_lock["packages"][""]["version"], version)
        self.assertEqual(
            integrations["peerDependencies"]["@hyphae_/hyphae"], version
        )
        changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
        self.assertRegex(
            changelog,
            rf"(?m)^## \[{re.escape(version)}\] - (?:Unreleased|\d{{4}}-\d{{2}}-\d{{2}})$",
        )

    def test_release_workflow_separates_native_and_candidate_artifacts(
        self,
    ) -> None:
        workflow = (ROOT / ".github" / "workflows" / "release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("name: hyphae-native-${{ matrix.target }}", workflow)
        self.assertIn("pattern: hyphae-native-*", workflow)
        self.assertNotIn("pattern: hyphae-*", workflow)
        recovery_guard = (
            "(github.event_name == 'workflow_dispatch' && "
            "inputs.release_tag != '' && inputs.release_commit != '')"
        )
        self.assertEqual(workflow.count(recovery_guard), 2)
        self.assertIn("release_tag:", workflow)
        self.assertIn("release_commit:", workflow)
        self.assertIn("RELEASE_SOURCE_REF", workflow)
        self.assertEqual(
            workflow.count(
                "RELEASE_SOURCE_REF: ${{ inputs.release_commit || "
                "github.event.pull_request.head.sha || github.sha }}"
            ),
            2,
        )
        self.assertIn("RELEASE_SOURCE_COMMIT", workflow)
        self.assertIn('if [[ "$tag_target" != "$RELEASE_COMMIT_INPUT" ]]', workflow)
        self.assertIn('git rev-list --parents -n 1 "$RELEASE_COMMIT_INPUT"', workflow)
        self.assertIn('test "${parents[2]}" = "$tag_target"', workflow)
        self.assertIn('"${RELEASE_COMMIT_INPUT}^{tree}"', workflow)
        self.assertEqual(workflow.count("name: Check out recovery control plane"), 2)
        self.assertEqual(
            workflow.count("HYPHAE_RELEASE_SOURCE_ROOT=$GITHUB_WORKSPACE"),
            2,
        )
        self.assertIn("refs/hyphae/release-tag", workflow)
        self.assertIn("refs/hyphae/publish-tag", workflow)
        self.assertIn("git merge-base --is-ancestor", workflow)
        self.assertIn("--tag-object", workflow)
        self.assertIn("--tag-target", workflow)
        self.assertNotIn("if: github.event_name != 'pull_request'", workflow)
        self.assertEqual(workflow.count("pull-requests: read"), 2)
        self.assertEqual(workflow.count('${merge_commit}^{tree}'), 2)
        self.assertEqual(workflow.count('${merge_commit}^1'), 2)
        self.assertEqual(workflow.count('${merge_commit}^2'), 2)
        self.assertEqual(workflow.count('test "$merge_commit" ='), 2)
        self.assertIn('test -n "${{ inputs.release_commit }}"', workflow)
        self.assertIn('test -z "${{ inputs.release_commit }}"', workflow)
        self.assertIn("anchore/sbom-action/download-syft@", workflow)
        self.assertIn("syft-version: v1.46.0", workflow)
        scan = workflow.index('scan dir:. --exclude ./embed -o "syft-json=${native_sbom}"')
        conclude = workflow.index("packaging/conclude_release_sbom_licenses.py")
        spdx = workflow.index('convert "$native_sbom" -o "spdx-json=${spdx_sbom}"')
        cyclonedx = workflow.index(
            'convert "$native_sbom" -o "cyclonedx-json=${cyclonedx_sbom}"'
        )
        evidence = workflow.index("name: Generate post-commit release evidence")
        self.assertLess(scan, conclude)
        self.assertLess(conclude, spdx)
        self.assertLess(conclude, cyclonedx)
        self.assertLess(spdx, evidence)
        self.assertLess(cyclonedx, evidence)
        pull_request_workflows = {
            REQUIRED_CHECK_WORKFLOWS[name]
            for name, event in REQUIRED_CHECK_EVENTS.items()
            if event == "pull_request"
        }
        for workflow_path in pull_request_workflows:
            workflow_source = (ROOT / workflow_path).read_text(encoding="utf-8")
            self.assertIn(
                "name: Verify the pull-request integration tree",
                workflow_source,
            )
            self.assertIn("'FETCH_HEAD^{tree}'", workflow_source)

    def test_release_control_plane_can_target_an_exact_source_checkout(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-release-source-") as source:
            environment = os.environ.copy()
            environment["HYPHAE_RELEASE_SOURCE_ROOT"] = source
            environment["PYTHONPATH"] = str(ROOT / "packaging")
            completed = subprocess.run(
                (
                    sys.executable,
                    "-c",
                    "import conclude_release_sbom_licenses as c; "
                    "import g8_release_verification as g; "
                    "import provenance as p; import release_evidence as r; "
                    "print(c.ROOT, g.ROOT, p.ROOT, r.ROOT, sep='\\n')",
                ),
                cwd=ROOT,
                env=environment,
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertEqual(
                completed.stdout.splitlines(),
                [str(Path(source).resolve())] * 4,
            )

    def test_ci_binds_major_semver_check_to_latest_release_baseline(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "cargo-semver-checks/releases/download/v0.48.0/"
            "cargo-semver-checks-x86_64-unknown-linux-gnu.tar.gz",
            workflow,
        )
        self.assertIn(
            "fc0144b6cfd8820bdb4180228212c26e6ee6b58363a20849c3386a5d22e90c3d",
            workflow,
        )
        self.assertIn(
            "BASELINE_COMMIT: 08028e8dac077846c638f067ce74fbcf6fb75501",
            workflow,
        )
        self.assertIn("refs/tags/v0.2.1:refs/tags/v0.2.1", workflow)
        self.assertIn("--release-type major", workflow)
        self.assertIn("--all-features", workflow)
        self.assertNotIn("Defer platform execution", workflow)
        self.assertIn("runs-on: ${{ matrix.os }}", workflow)
        self.assertNotIn(
            "name: Optional framework integrations\n    if:",
            workflow,
        )

    def test_archives_are_reproducible_and_rooted(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-package-") as temporary:
            root = Path(temporary)
            binary = root / "binary"
            binary.write_bytes(b"native-binary")
            first_dir = root / "first"
            second_dir = root / "second"
            for target in ("x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"):
                first = build_archive(binary, target, first_dir, 1_700_000_000)
                second = build_archive(binary, target, second_dir, 1_700_000_000)
                self.assertEqual(first.read_bytes(), second.read_bytes())
                self.assertTrue(first.name.startswith(f"hyphae-{product_version()}-"))

    def test_release_archives_include_notice_and_the_complete_document_set(self) -> None:
        self.assertEqual(
            INCLUDED_DOCUMENTS,
            (
                "LICENSE",
                "LICENSE-DOCUMENTATION",
                "LICENSE-POLICY.md",
                "NOTICE",
                "README.md",
                "THIRD_PARTY_NOTICES.md",
                "THIRD_PARTY_LICENSES.txt",
            ),
        )

    def test_checksum_manifest_is_complete_and_tamper_evident(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-checksums-") as temporary:
            root = Path(temporary)
            (root / "hyphae-test.tar.gz").write_bytes(b"archive")
            (root / "hyphae-test.spdx.json").write_text("{}\n", encoding="utf-8")
            create_checksums(root)
            verify_checksums(root)
            (root / "hyphae-test.tar.gz").write_bytes(b"tampered")
            with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                verify_checksums(root)

    def test_release_tag_and_slsa_predicate_are_bound_to_source(self) -> None:
        identity = source_identity(git_object("HEAD^{commit}"))
        with patch("finalize_release.source_identity") as source:
            source.return_value = SimpleNamespace(version="1.2.0", tag="release-v1.2.0-crates")
            require_matching_tag("release-v1.2.0-crates")
            with self.assertRaisesRegex(RuntimeError, "does not match"):
                require_matching_tag("v0.0.0")
        predicate = build_predicate(
            target="x86_64-unknown-linux-gnu",
            commit=identity.commit,
            git_ref=f"refs/tags/{identity.tag}",
            invocation_id="https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/1/attempts/1",
            runner_os="Linux",
            runner_arch="X64",
        )
        definition = predicate["buildDefinition"]
        details = predicate["runDetails"]
        self.assertEqual(definition["buildType"], BUILD_TYPE)
        self.assertEqual(details["builder"]["id"], BUILDER_ID)
        self.assertEqual(
            definition["resolvedDependencies"][0]["digest"]["gitCommit"],
            identity.commit,
        )
        with self.assertRaisesRegex(ValueError, "native runner identity"):
            build_predicate(
                target="x86_64-pc-windows-msvc",
                commit=identity.commit,
                git_ref=f"refs/tags/{identity.tag}",
                invocation_id=(
                    "https://github.com/Hyphae-Research-Foundation/hyphae/"
                    "actions/runs/1/attempts/1"
                ),
                runner_os="Linux",
                runner_arch="X64",
            )

    def test_release_layout_rejects_unknown_or_missing_supply_chain_files(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-release-layout-") as temporary:
            root = Path(temporary)
            add_test_release_evidence(root)
            validate_release_layout(root, final=False)
            (root / "unexpected.txt").write_text("unexpected\n", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "unexpected files"):
                validate_release_layout(root, final=False)
            (root / "unexpected.txt").unlink()
            with self.assertRaisesRegex(RuntimeError, "SHA256SUMS"):
                validate_release_layout(root, final=True)

    def test_final_tag_layout_binds_evidence_checks_and_every_signature(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-final-layout-") as temporary:
            root = Path(temporary)
            manifest = add_test_release_evidence(root, tag_release=True)
            identity = source_identity(git_object("HEAD^{commit}"))
            report = root / required_checks_name(identity.tag)
            add_final_signature_bundles(root)
            verify_checksums(root)
            validate_release_layout(root, final=True)
            checksums = (root / "SHA256SUMS").read_text("ascii")
            self.assertIn(f"  {manifest.name}\n", checksums)
            self.assertIn(f"  {report.name}\n", checksums)
            evidence = json.loads(manifest.read_text("utf-8"))
            check_records = [
                artifact
                for artifact in evidence["artifacts"]
                if artifact["role"] == "required-checks"
            ]
            self.assertEqual([record["name"] for record in check_records], [report.name])
            self.assertEqual(
                check_records[0]["sha256"],
                hashlib.sha256(report.read_bytes()).hexdigest(),
            )
            self.assertTrue((root / f"{manifest.name}.sigstore.json").is_file())
            self.assertTrue((root / f"{report.name}.sigstore.json").is_file())

    def test_release_evidence_binds_source_run_and_payload_without_self_reference(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-release-evidence-") as temporary:
            root = Path(temporary)
            manifest = add_test_release_evidence(root)
            identity = source_identity(git_object("HEAD^{commit}"))
            validate_release_evidence_file(
                manifest,
                directory=root,
                expected_commit=identity.commit,
            )
            document = json.loads(manifest.read_text("utf-8"))
            names = [artifact["name"] for artifact in document["artifacts"]]
            self.assertNotIn(manifest.name, names)
            self.assertNotIn("SHA256SUMS", names)
            self.assertEqual(document["schema"], SCHEMA_NAME)
            self.assertEqual(
                document["workflow"]["url"],
                "https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/123456/attempts/1",
            )
            rerun = build_release_evidence(
                directory=root,
                tag=identity.tag,
                commit=identity.commit,
                workflow_ref="refs/heads/release-candidate",
                event="workflow_dispatch",
                run_id="123456",
                run_attempt=2,
            )
            self.assertEqual(rerun["workflow"]["run_attempt"], 2)

            archive = archive_name(
                identity.version,
                "x86_64-unknown-linux-gnu",
            )
            (root / archive).write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "artifact mismatch"):
                validate_release_evidence_file(
                    manifest,
                    directory=root,
                    expected_commit=identity.commit,
                )

    def test_release_rejects_missing_or_noncanonical_target(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-missing-target-") as temporary:
            root = Path(temporary)
            identity, workflow_ref, event = write_test_primary_payloads(
                root,
                tag_release=False,
            )
            missing = archive_name(identity.version, "aarch64-apple-darwin")
            (root / missing).unlink()
            with self.assertRaisesRegex(ValueError, "canonical target set"):
                build_release_evidence(
                    directory=root,
                    tag=identity.tag,
                    commit=identity.commit,
                    workflow_ref=workflow_ref,
                    event=event,
                    run_id="123456",
                    run_attempt=1,
                )

        with tempfile.TemporaryDirectory(prefix="hyphae-wrong-target-") as temporary:
            root = Path(temporary)
            identity, workflow_ref, event = write_test_primary_payloads(
                root,
                tag_release=False,
            )
            archive = archive_name(identity.version, "x86_64-unknown-linux-gnu")
            (root / archive).rename(root / f"hyphae-{identity.version}-unknown.tar.gz")
            with self.assertRaisesRegex(ValueError, "canonical target set"):
                build_release_evidence(
                    directory=root,
                    tag=identity.tag,
                    commit=identity.commit,
                    workflow_ref=workflow_ref,
                    event=event,
                    run_id="123456",
                    run_attempt=1,
                )

    def test_release_rejects_crossed_provenance(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-crossed-provenance-") as temporary:
            root = Path(temporary)
            identity, workflow_ref, event = write_test_primary_payloads(
                root,
                tag_release=False,
            )
            archive = archive_name(identity.version, "aarch64-apple-darwin")
            predicate_path = root / f"{archive}.provenance.json"
            original = json.loads(predicate_path.read_text("utf-8"))

            def wrong_target(predicate) -> None:
                predicate["buildDefinition"]["externalParameters"]["target"] = (
                    "x86_64-apple-darwin"
                )

            def wrong_commit(predicate) -> None:
                predicate["buildDefinition"]["resolvedDependencies"][0]["digest"][
                    "gitCommit"
                ] = "0" * 40

            def wrong_ref(predicate) -> None:
                predicate["buildDefinition"]["externalParameters"]["workflow"][
                    "ref"
                ] = "refs/heads/other"

            def wrong_invocation(predicate) -> None:
                predicate["runDetails"]["metadata"]["invocationId"] = (
                    "https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/1/attempts/1"
                )

            def future_invocation(predicate) -> None:
                predicate["runDetails"]["metadata"]["invocationId"] = (
                    "https://github.com/Hyphae-Research-Foundation/hyphae/"
                    "actions/runs/123456/attempts/2"
                )

            def wrong_runner(predicate) -> None:
                predicate["buildDefinition"]["internalParameters"]["runner_os"] = (
                    "Linux"
                )

            def wrong_lock_digest(predicate) -> None:
                predicate["buildDefinition"]["resolvedDependencies"][1]["digest"][
                    "sha256"
                ] = "0" * 64

            def wrong_workflow_digest(predicate) -> None:
                predicate["buildDefinition"]["resolvedDependencies"][2]["digest"][
                    "sha256"
                ] = "0" * 64

            mutations = (
                ("target", wrong_target),
                ("commit", wrong_commit),
                ("ref", wrong_ref),
                ("invocation", wrong_invocation),
                ("future invocation", future_invocation),
                ("runner", wrong_runner),
                ("Cargo.lock digest", wrong_lock_digest),
                ("workflow digest", wrong_workflow_digest),
            )
            for label, mutate in mutations:
                with self.subTest(label=label):
                    predicate = copy.deepcopy(original)
                    mutate(predicate)
                    predicate_path.write_text(
                        json.dumps(predicate) + "\n",
                        encoding="utf-8",
                    )
                    with self.assertRaises(ValueError):
                        build_release_evidence(
                            directory=root,
                            tag=identity.tag,
                            commit=identity.commit,
                            workflow_ref=workflow_ref,
                            event=event,
                            run_id="123456",
                            run_attempt=1,
                        )
            predicate_path.write_text(
                json.dumps(original) + "\n",
                encoding="utf-8",
            )

    def test_tag_evidence_requires_the_hosted_check_report(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-tag-checks-") as temporary:
            root = Path(temporary)
            identity, workflow_ref, event = write_test_primary_payloads(
                root,
                tag_release=True,
                include_required_checks=False,
            )
            with self.assertRaisesRegex(ValueError, "required-checks"):
                build_release_evidence(
                    directory=root,
                    tag=identity.tag,
                    commit=identity.commit,
                    workflow_ref=workflow_ref,
                    event=event,
                    run_id="123456",
                    run_attempt=1,
                )

    def test_tag_identity_and_manual_tag_runs_fail_closed(self) -> None:
        commit = git_object("HEAD^{commit}")
        with tempfile.TemporaryDirectory(prefix="hyphae-tag-identity-") as temporary:
            root = Path(temporary)
            manifest = add_test_release_evidence(root, tag_release=True)
            document = json.loads(manifest.read_text("utf-8"))
            validate_release_evidence(
                document,
                directory=root,
                expected_commit=commit,
                expected_tag_object=commit,
                expected_tag_target=commit,
            )
            with self.assertRaisesRegex(ValueError, "live tag object and target"):
                validate_release_evidence(
                    document,
                    directory=root,
                    expected_commit=commit,
                    require_live_tag_binding=True,
                )
            verification = (
                sys.executable,
                str(ROOT / "packaging" / "release_evidence.py"),
                "verify",
                "--directory",
                str(root),
                "--manifest",
                str(manifest),
                "--commit",
                commit,
            )
            missing_live_tag = subprocess.run(
                verification,
                cwd=ROOT,
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(missing_live_tag.returncode, 0)
            self.assertIn(
                "live tag object and target",
                missing_live_tag.stderr,
            )
            subprocess.run(
                (
                    *verification,
                    "--tag-object",
                    commit,
                    "--tag-target",
                    commit,
                ),
                cwd=ROOT,
                check=True,
                capture_output=True,
                text=True,
            )

            missing_object = copy.deepcopy(document)
            missing_object["source"]["tag_object"] = None
            with self.assertRaisesRegex(ValueError, "tag object IDs"):
                validate_release_evidence(
                    missing_object,
                    directory=root,
                    expected_commit=commit,
                )

            wrong_target = copy.deepcopy(document)
            wrong_target["source"]["tag_target"] = "0" * 40
            with self.assertRaisesRegex(ValueError, "tag target"):
                validate_release_evidence(
                    wrong_target,
                    directory=root,
                    expected_commit=commit,
                )

            with self.assertRaisesRegex(ValueError, "live tag binding"):
                validate_release_evidence(
                    document,
                    directory=root,
                    expected_commit=commit,
                    expected_tag_object="0" * 40,
                    expected_tag_target=commit,
                )

        with tempfile.TemporaryDirectory(prefix="hyphae-branch-identity-") as temporary:
            root = Path(temporary)
            manifest = add_test_release_evidence(root)
            document = json.loads(manifest.read_text("utf-8"))
            document["source"]["tag_object"] = commit
            document["source"]["tag_target"] = commit
            with self.assertRaisesRegex(ValueError, "must not claim a tag object"):
                validate_release_evidence(
                    document,
                    directory=root,
                    expected_commit=commit,
                )

        with tempfile.TemporaryDirectory(prefix="hyphae-manual-tag-") as temporary:
            root = Path(temporary)
            identity, workflow_ref, _ = write_test_primary_payloads(
                root,
                tag_release=True,
                include_required_checks=True,
                workflow_ref_override="refs/heads/main",
                event_override="workflow_dispatch",
            )
            document = build_release_evidence(
                directory=root,
                tag=identity.tag,
                commit=identity.commit,
                workflow_ref=workflow_ref,
                event="workflow_dispatch",
                run_id="123456",
                run_attempt=1,
                tag_object=identity.commit,
                tag_target=identity.commit,
            )
            self.assertIn(
                "required-checks",
                {artifact["role"] for artifact in document["artifacts"]},
            )

        with tempfile.TemporaryDirectory(prefix="hyphae-pr-tag-") as temporary:
            root = Path(temporary)
            identity, _, _ = write_test_primary_payloads(
                root,
                tag_release=True,
                include_required_checks=True,
                workflow_ref_override="refs/pull/231/merge",
                event_override="pull_request",
            )
            document = build_release_evidence(
                directory=root,
                tag=identity.tag,
                commit=identity.commit,
                workflow_ref="refs/pull/231/merge",
                event="pull_request",
                run_id="123456",
                run_attempt=1,
                tag_object=identity.commit,
                tag_target=identity.commit,
            )
            self.assertIn(
                "required-checks",
                {artifact["role"] for artifact in document["artifacts"]},
            )
            validate_release_evidence(
                document,
                directory=root,
                expected_commit=identity.commit,
                expected_tag_object=identity.commit,
                expected_tag_target=identity.commit,
            )

    def test_required_checks_are_exact_and_latest_prior_run_must_succeed(self) -> None:
        head_commit = git_object("HEAD^{commit}")
        commit = "c" * 40
        runs = fixture_check_runs(head_commit, release_commit=commit)
        workflow_runs = fixture_workflow_runs(head_commit, runs)
        job_runs = fixture_job_runs(head_commit, runs)
        pull_requests = fixture_pull_requests(head_commit, merge_commit=commit)
        head_pull_requests = fixture_pull_requests(
            head_commit,
            merge_commit=commit,
        )
        pull_request_events: list[object] = []
        report = build_report(
            runs,
            workflow_runs=workflow_runs,
            job_runs=job_runs,
            pull_requests=pull_requests,
            head_pull_requests=head_pull_requests,
            pull_request_events=pull_request_events,
            repository=REPOSITORY_SLUG,
            commit=commit,
            excluded_run_id="999",
        )
        validate_report(report, expected_commit=commit)
        self.assertEqual(
            [check["name"] for check in report["checks"]],
            list(REQUIRED_CHECK_NAMES),
        )
        self.assertTrue(
            all(
                check["app_id"] == GITHUB_ACTIONS_APP_ID
                and check["app_slug"] == GITHUB_ACTIONS_APP_SLUG
                for check in report["checks"]
            )
        )
        self.assertEqual(
            report["checks"][0]["check_run_url"],
            "https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/30000/job/10000",
        )
        self.assertEqual(report["checks"][0]["workflow_run_id"], 30_000)
        self.assertEqual(
            report["checks"][0]["workflow_path"],
            ".github/workflows/release.yml",
        )
        self.assertEqual(
            report["checks"][0]["completed_at"],
            "2026-07-29T18:01:00Z",
        )
        self.assertEqual(report["pull_request"]["number"], 14)
        self.assertEqual(report["pull_request"]["base_ref"], "main")
        self.assertEqual(report["head_sha"], commit)
        self.assertEqual(report["pull_request"]["head_sha"], head_commit)
        self.assertEqual(report["pull_request"]["merge_commit_sha"], commit)
        self.assertEqual(
            {
                check["head_sha"]
                for check in report["checks"]
                if check["name"] != "Validate all exact-SHA G8 receipts"
            },
            {head_commit},
        )
        self.assertEqual(report["checks"][-1]["head_sha"], commit)
        self.assertEqual(
            {check["workflow_event"] for check in report["checks"]},
            {"pull_request", "workflow_dispatch"},
        )
        self.assertEqual(
            [
                check["name"]
                for check in report["checks"]
                if check["workflow_event"] == "workflow_dispatch"
            ],
            ["Validate all exact-SHA G8 receipts"],
        )
        self.assertEqual(
            {check["head_branch"] for check in report["checks"]},
            {"codex/release-candidate", "release/codex/release-candidate-merge-evidence"},
        )
        self.assertEqual(
            {
                check["name"]
                for check in report["checks"]
                if check["integration_guard"] is not None
            },
            set(INTEGRATION_GUARD_CHECKS),
        )
        self.assertEqual(
            {check["workflow_run_attempt"] for check in report["checks"]},
            {1},
        )

        rerun_workflows = copy.deepcopy(workflow_runs)
        for workflow_run in rerun_workflows.values():
            workflow_run["run_attempt"] = 2
        rerun_jobs = copy.deepcopy(job_runs)
        for job_run in rerun_jobs.values():
            job_run["run_attempt"] = 2
        rerun_report = build_report(
            runs,
            workflow_runs=rerun_workflows,
            job_runs=rerun_jobs,
            pull_requests=pull_requests,
            head_pull_requests=head_pull_requests,
            pull_request_events=pull_request_events,
            repository=REPOSITORY_SLUG,
            commit=commit,
            excluded_run_id="999",
        )
        self.assertEqual(
            {check["workflow_run_attempt"] for check in rerun_report["checks"]},
            {2},
        )

        partial_rerun_jobs = copy.deepcopy(rerun_jobs)
        partial_rerun_jobs[10_004]["run_attempt"] = 1
        with self.assertRaisesRegex(
            ValueError,
            "attempt differs from workflow run",
        ):
            build_report(
                runs,
                workflow_runs=rerun_workflows,
                job_runs=partial_rerun_jobs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        missing_non_guard_job = copy.deepcopy(job_runs)
        self.assertNotIn(
            str(runs[4]["name"]),
            INTEGRATION_GUARD_CHECKS,
        )
        missing_non_guard_job.pop(10_004)
        with self.assertRaisesRegex(
            ValueError,
            "job metadata is missing for check run",
        ):
            build_report(
                runs,
                workflow_runs=workflow_runs,
                job_runs=missing_non_guard_job,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        duplicate_id = copy.deepcopy(report)
        duplicate_id["checks"][1]["check_run_id"] = duplicate_id["checks"][0][
            "check_run_id"
        ]
        duplicate_id["checks"][1]["check_run_url"] = duplicate_id["checks"][0][
            "check_run_url"
        ]
        duplicate_id["checks"][1]["workflow_run_id"] = duplicate_id["checks"][0][
            "workflow_run_id"
        ]
        with self.assertRaisesRegex(ValueError, "invalid"):
            validate_report(duplicate_id, expected_commit=commit)
        wrong_app = copy.deepcopy(report)
        wrong_app["checks"][0]["app_slug"] = "untrusted-app"
        with self.assertRaisesRegex(ValueError, "invalid"):
            validate_report(wrong_app, expected_commit=commit)
        failed_guard = copy.deepcopy(report)
        failed_guard["checks"][0]["integration_guard"]["conclusion"] = "failure"
        with self.assertRaisesRegex(ValueError, "invalid"):
            validate_report(failed_guard, expected_commit=commit)
        crossed_pull_request = copy.deepcopy(report)
        crossed_pull_request["pull_request"]["base_sha"] = "not-a-commit"
        with self.assertRaisesRegex(ValueError, "pull request"):
            validate_report(crossed_pull_request, expected_commit=commit)
        crossed_merge = copy.deepcopy(report)
        crossed_merge["pull_request"]["merge_commit_sha"] = "d" * 40
        with self.assertRaisesRegex(ValueError, "pull request"):
            validate_report(crossed_merge, expected_commit=commit)
        crossed_pr_check = copy.deepcopy(report)
        crossed_pr_check["checks"][0]["head_sha"] = commit
        with self.assertRaisesRegex(ValueError, "invalid"):
            validate_report(crossed_pr_check, expected_commit=commit)
        crossed_closure = copy.deepcopy(report)
        crossed_closure["checks"][-1]["head_sha"] = head_commit
        with self.assertRaisesRegex(ValueError, "invalid"):
            validate_report(crossed_closure, expected_commit=commit)
        crossed_closure_branch = copy.deepcopy(report)
        crossed_closure_branch["checks"][-1]["head_branch"] = (
            "codex/release-candidate"
        )
        with self.assertRaisesRegex(ValueError, "invalid"):
            validate_report(crossed_closure_branch, expected_commit=commit)
        mixed_workflow_runs = copy.deepcopy(report)
        mixed_workflow_runs["checks"][10]["workflow_run_id"] = 39_999
        mixed_workflow_runs["checks"][10]["check_run_url"] = check_run_url(
            REPOSITORY_SLUG,
            39_999,
            mixed_workflow_runs["checks"][10]["check_run_id"],
        )
        with self.assertRaisesRegex(ValueError, "invalid"):
            validate_report(mixed_workflow_runs, expected_commit=commit)
        crossed_url = copy.deepcopy(report)
        crossed_url["checks"][0]["check_run_url"] = (
            "https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/30000/job/10001"
        )
        with self.assertRaisesRegex(ValueError, "invalid"):
            validate_report(crossed_url, expected_commit=commit)
        for invalid_url in (
            "http://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/30000/job/10000",
            "https://github.com/other/hyphae/actions/runs/30000/job/10000",
            "https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/30000/jobs/10000",
            "https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/30000/job/10001",
        ):
            invalid_source = copy.deepcopy(runs)
            invalid_source[0]["html_url"] = invalid_url
            invalid_source[0]["details_url"] = invalid_url
            with self.assertRaisesRegex(ValueError, "URL"):
                build_report(
                    invalid_source,
                    workflow_runs=workflow_runs,
                    job_runs=job_runs,
                    pull_requests=pull_requests,
                    head_pull_requests=head_pull_requests,
                    pull_request_events=pull_request_events,
                    repository=REPOSITORY_SLUG,
                    commit=commit,
                    excluded_run_id="999",
                )
        unequal_source_urls = copy.deepcopy(runs)
        unequal_source_urls[0]["details_url"] = check_run_url(
            REPOSITORY_SLUG,
            30_001,
            10_000,
        )
        with self.assertRaisesRegex(ValueError, "details URL differs"):
            build_report(
                unequal_source_urls,
                workflow_runs=workflow_runs,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )
        with self.assertRaisesRegex(ValueError, "lacks a prior run"):
            build_report(
                runs[:-1],
                workflow_runs=workflow_runs,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        for name in ("Security hard-kill aggregate", "MCP real hosts"):
            with self.subTest(missing=name), self.assertRaisesRegex(
                ValueError, "lacks a prior run"
            ):
                build_report(
                    [run for run in runs if run["name"] != name],
                    workflow_runs=workflow_runs,
                    job_runs=job_runs,
                    pull_requests=pull_requests,
                    head_pull_requests=head_pull_requests,
                    pull_request_events=pull_request_events,
                    repository=REPOSITORY_SLUG,
                    commit=commit,
                    excluded_run_id="999",
                )

        failed_latest = copy.deepcopy(runs[0])
        # GitHub defines "latest" by completion time, not by check-run ID.
        # A later failure with a lower ID must still block the report.
        failed_latest["id"] = 9_999
        failed_latest["html_url"] = check_run_url(
            REPOSITORY_SLUG,
            88_888,
            9_999,
        )
        failed_latest["details_url"] = failed_latest["html_url"]
        failed_latest["conclusion"] = "failure"
        failed_latest["started_at"] = "2026-07-29T18:02:00Z"
        failed_latest["completed_at"] = "2026-07-29T18:03:00Z"
        with self.assertRaisesRegex(ValueError, "not successful"):
            build_report(
                [*runs, failed_latest],
                workflow_runs=workflow_runs,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        current_run = copy.deepcopy(failed_latest)
        current_run["html_url"] = check_run_url(
            REPOSITORY_SLUG,
            999,
            9_999,
        )
        current_run["details_url"] = current_run["html_url"]
        build_report(
            [*runs, current_run],
            workflow_runs=workflow_runs,
            job_runs=job_runs,
            pull_requests=pull_requests,
            head_pull_requests=head_pull_requests,
            pull_request_events=pull_request_events,
            repository=REPOSITORY_SLUG,
            commit=commit,
            excluded_run_id="999",
        )

        in_progress = copy.deepcopy(runs[0])
        in_progress["id"] = 9_998
        in_progress["html_url"] = check_run_url(
            REPOSITORY_SLUG,
            777,
            9_998,
        )
        in_progress["details_url"] = in_progress["html_url"]
        in_progress["status"] = "in_progress"
        in_progress["conclusion"] = None
        in_progress["started_at"] = "2026-07-29T18:04:00Z"
        in_progress["completed_at"] = None
        with self.assertRaisesRegex(ValueError, "not completed"):
            build_report(
                [*runs, in_progress],
                workflow_runs=workflow_runs,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        ambiguous = copy.deepcopy(runs[0])
        ambiguous["id"] = 9_997
        ambiguous["html_url"] = check_run_url(
            REPOSITORY_SLUG,
            776,
            9_997,
        )
        ambiguous["details_url"] = ambiguous["html_url"]
        with self.assertRaisesRegex(ValueError, "ambiguous latest completion"):
            build_report(
                [*runs, ambiguous],
                workflow_runs=workflow_runs,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        homonymous = copy.deepcopy(runs[0])
        homonymous["id"] = 9_996
        homonymous["html_url"] = check_run_url(
            REPOSITORY_SLUG,
            775,
            9_996,
        )
        homonymous["details_url"] = homonymous["html_url"]
        homonymous["started_at"] = "2026-07-29T18:05:00Z"
        homonymous["completed_at"] = "2026-07-29T18:06:00Z"
        homonymous_workflows = copy.deepcopy(workflow_runs)
        homonymous_workflows[775] = copy.deepcopy(workflow_runs[30_000])
        homonymous_workflows[775]["id"] = 775
        homonymous_workflows[775]["path"] = ".github/workflows/ci.yml"
        with self.assertRaisesRegex(ValueError, "path is not canonical"):
            build_report(
                [*runs, homonymous],
                workflow_runs=homonymous_workflows,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        crossed_workflow_run = copy.deepcopy(workflow_runs)
        crossed_workflow_run[30_000]["id"] = 30_001
        with self.assertRaisesRegex(ValueError, "ID differs"):
            build_report(
                runs,
                workflow_runs=crossed_workflow_run,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        crossed_workflow_head = copy.deepcopy(workflow_runs)
        crossed_workflow_head[30_000]["head_sha"] = "f" * 40
        with self.assertRaisesRegex(ValueError, "head_sha differs"):
            build_report(
                runs,
                workflow_runs=crossed_workflow_head,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        failed_workflow_run = copy.deepcopy(workflow_runs)
        failed_workflow_run[30_000]["conclusion"] = "failure"
        with self.assertRaisesRegex(ValueError, "workflow run 30000 is not successful"):
            build_report(
                runs,
                workflow_runs=failed_workflow_run,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        crossed_event = copy.deepcopy(workflow_runs)
        crossed_event[30_000]["event"] = "workflow_dispatch"
        with self.assertRaisesRegex(ValueError, "release pull request"):
            build_report(
                runs,
                workflow_runs=crossed_event,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        with self.assertRaisesRegex(ValueError, "exactly one pull request"):
            build_report(
                runs,
                workflow_runs=workflow_runs,
                job_runs=job_runs,
                pull_requests=[*pull_requests, copy.deepcopy(pull_requests[0])],
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        with self.assertRaisesRegex(ValueError, "exactly one pull request"):
            build_report(
                runs,
                workflow_runs=workflow_runs,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=[
                    *head_pull_requests,
                    copy.deepcopy(head_pull_requests[0]),
                ],
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        with self.assertRaisesRegex(ValueError, "base ref changed"):
            build_report(
                runs,
                workflow_runs=workflow_runs,
                job_runs=job_runs,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=[{"event": "base_ref_changed"}],
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

        missing_guard = copy.deepcopy(job_runs)
        missing_guard[10_000]["steps"] = []
        with self.assertRaisesRegex(ValueError, "exactly one integration guard"):
            build_report(
                runs,
                workflow_runs=workflow_runs,
                job_runs=missing_guard,
                pull_requests=pull_requests,
                head_pull_requests=head_pull_requests,
                pull_request_events=pull_request_events,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
            )

    def test_github_api_fetches_are_paginated_and_never_use_the_network(self) -> None:
        class JsonResponse:
            def __init__(self, payload: object) -> None:
                self.payload = payload

            def __enter__(self):
                return self

            def __exit__(self, *_arguments) -> None:
                return None

            def read(self) -> bytes:
                return json.dumps(self.payload).encode()

        first_page = [{"id": value} for value in range(100)]
        second_page = [{"id": 100}]
        with patch(
            "required_checks.urllib.request.urlopen",
            side_effect=(
                JsonResponse({"check_runs": first_page}),
                JsonResponse({"check_runs": second_page}),
            ),
        ) as open_url:
            runs = fetch_check_runs(
                repository=REPOSITORY_SLUG,
                commit=git_object("HEAD^{commit}"),
                token="test-token",
            )
        self.assertEqual(len(runs), 101)
        first_request = open_url.call_args_list[0].args[0]
        second_request = open_url.call_args_list[1].args[0]
        self.assertIn("filter=all", first_request.full_url)
        self.assertIn("page=1", first_request.full_url)
        self.assertIn("page=2", second_request.full_url)
        self.assertEqual(
            first_request.get_header("Authorization"),
            "Bearer test-token",
        )

        commit = git_object("HEAD^{commit}")
        check_runs = fixture_check_runs(commit)
        expected_workflows = fixture_workflow_runs(commit, check_runs)
        with patch(
            "required_checks.urllib.request.urlopen",
            side_effect=tuple(
                JsonResponse(expected_workflows[workflow_run_id])
                for workflow_run_id in sorted(expected_workflows)
            ),
        ) as open_url:
            workflow_runs = fetch_workflow_runs(
                check_runs,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
                token="test-token",
            )
        self.assertEqual(workflow_runs, expected_workflows)
        self.assertEqual(open_url.call_count, len(expected_workflows))
        for call, workflow_run_id in zip(
            open_url.call_args_list,
            sorted(expected_workflows),
            strict=True,
        ):
            request = call.args[0]
            self.assertEqual(
                request.full_url,
                (
                    "https://api.github.com/repos/Hyphae-Research-Foundation/hyphae/actions/runs/"
                    f"{workflow_run_id}"
                ),
            )
            self.assertEqual(
                request.get_header("Authorization"),
                "Bearer test-token",
            )

        expected_jobs = fixture_job_runs(commit, check_runs)
        with patch(
            "required_checks.urllib.request.urlopen",
            side_effect=tuple(
                JsonResponse(expected_jobs[job_run_id])
                for job_run_id in sorted(expected_jobs)
            ),
        ) as open_url:
            job_runs = fetch_job_runs(
                check_runs,
                repository=REPOSITORY_SLUG,
                commit=commit,
                excluded_run_id="999",
                token="test-token",
            )
        self.assertEqual(job_runs, expected_jobs)
        self.assertEqual(open_url.call_count, len(REQUIRED_CHECK_NAMES))
        self.assertEqual(
            job_runs[10_004]["run_attempt"],
            1,
        )
        self.assertNotIn(
            str(job_runs[10_004]["name"]),
            INTEGRATION_GUARD_CHECKS,
        )
        for call, job_run_id in zip(
            open_url.call_args_list,
            sorted(expected_jobs),
            strict=True,
        ):
            request = call.args[0]
            self.assertEqual(
                request.full_url,
                (
                    "https://api.github.com/repos/Hyphae-Research-Foundation/hyphae/actions/jobs/"
                    f"{job_run_id}"
                ),
            )

        first_pull_page = [fixture_pull_requests(commit)[0] for _ in range(100)]
        second_pull_page = [fixture_pull_requests(commit)[0]]
        with patch(
            "required_checks.urllib.request.urlopen",
            side_effect=(
                JsonResponse(first_pull_page),
                JsonResponse(second_pull_page),
            ),
        ) as open_url:
            pull_requests = fetch_pull_requests(
                repository=REPOSITORY_SLUG,
                commit=commit,
                token="test-token",
            )
        self.assertEqual(len(pull_requests), 101)
        first_request = open_url.call_args_list[0].args[0]
        second_request = open_url.call_args_list[1].args[0]
        self.assertIn(f"/commits/{commit}/pulls?", first_request.full_url)
        self.assertIn("page=1", first_request.full_url)
        self.assertIn("page=2", second_request.full_url)
        self.assertEqual(
            first_request.get_header("Authorization"),
            "Bearer test-token",
        )

        with patch(
            "required_checks.urllib.request.urlopen",
            return_value=JsonResponse(fixture_pull_requests(commit)),
        ) as open_url:
            head_pull_requests = fetch_head_pull_requests(
                repository=REPOSITORY_SLUG,
                head_ref="codex/release-candidate",
                token="test-token",
            )
        self.assertEqual(head_pull_requests, fixture_pull_requests(commit))
        request = open_url.call_args.args[0]
        self.assertIn("state=all", request.full_url)
        self.assertIn(
            "head=Hyphae-Research-Foundation%3Acodex%2Frelease-candidate",
            request.full_url,
        )

        events = [{"event": "merged"}]
        with patch(
            "required_checks.urllib.request.urlopen",
            return_value=JsonResponse(events),
        ) as open_url:
            fetched_events = fetch_pull_request_events(
                repository=REPOSITORY_SLUG,
                number=14,
                token="test-token",
            )
        self.assertEqual(fetched_events, events)
        request = open_url.call_args.args[0]
        self.assertIn("/issues/14/events?", request.full_url)
        self.assertIn("page=1", request.full_url)

    def test_release_identity_is_derived_from_the_commit_not_the_worktree(self) -> None:
        commit = git_object("HEAD^{commit}")
        committed_cargo = (
            b'[workspace]\n[workspace.package]\nversion = "9.8.7"\n'
        )
        with patch("release_evidence.commit_file", return_value=committed_cargo):
            identity = source_identity(commit)
        self.assertEqual(identity.version, "9.8.7")
        self.assertEqual(identity.tag, "release-v9.8.7-crates")
        self.assertEqual(identity.commit, commit)

    @patch("release_evidence.current_commit")
    @patch("release_evidence.git")
    @patch("release_evidence.resolve_commit")
    def test_dirty_tracked_worktree_is_rejected(
        self,
        resolve,
        run_git,
        current,
    ) -> None:
        from release_evidence import require_tracked_worktree_matches

        commit = "a" * 40
        resolve.return_value = commit
        current.return_value = commit
        run_git.return_value.returncode = 1
        with self.assertRaisesRegex(ValueError, "tracked index or worktree"):
            require_tracked_worktree_matches(commit)
        run_git.assert_called_once_with(
            "diff",
            "--quiet",
            commit,
            "--",
            check=False,
        )

    def test_release_evidence_schema_matches_the_emitted_identifier(self) -> None:
        try:
            from jsonschema import Draft202012Validator, ValidationError
        except ImportError:
            self.skipTest("jsonschema is not installed")

        release_schema = json.loads(SCHEMA_PATH.read_text("utf-8"))
        checks_schema = json.loads(REPORT_SCHEMA_PATH.read_text("utf-8"))
        Draft202012Validator.check_schema(release_schema)
        Draft202012Validator.check_schema(checks_schema)

        with tempfile.TemporaryDirectory(prefix="hyphae-schema-") as temporary:
            root = Path(temporary)
            manifest = add_test_release_evidence(root, tag_release=True)
            document = json.loads(manifest.read_text("utf-8"))
            Draft202012Validator(release_schema).validate(document)
            candidate_with_report = copy.deepcopy(document)
            candidate_with_report["workflow"]["ref"] = (
                "refs/heads/release-candidate"
            )
            with self.assertRaises(ValidationError):
                Draft202012Validator(release_schema).validate(candidate_with_report)
            candidate = copy.deepcopy(candidate_with_report)
            candidate["artifacts"] = [
                artifact
                for artifact in candidate["artifacts"]
                if artifact["role"] != "required-checks"
            ]
            candidate["source"]["tag_object"] = None
            candidate["source"]["tag_target"] = None
            Draft202012Validator(release_schema).validate(candidate)
            manual_tag = copy.deepcopy(document)
            manual_tag["workflow"]["event"] = "workflow_dispatch"
            manual_tag["workflow"]["ref"] = "refs/heads/main"
            Draft202012Validator(release_schema).validate(manual_tag)
            manual_without_checks = copy.deepcopy(manual_tag)
            manual_without_checks["artifacts"] = [
                artifact
                for artifact in manual_without_checks["artifacts"]
                if artifact["role"] != "required-checks"
            ]
            with self.assertRaises(ValidationError):
                Draft202012Validator(release_schema).validate(
                    manual_without_checks
                )
            missing_tag_object = copy.deepcopy(document)
            missing_tag_object["source"]["tag_object"] = None
            with self.assertRaises(ValidationError):
                Draft202012Validator(release_schema).validate(missing_tag_object)
            report = json.loads(
                (root / required_checks_name(document["release"]["tag"])).read_text(
                    "utf-8"
                )
            )
            Draft202012Validator(checks_schema).validate(report)
            crossed_workflow_path = copy.deepcopy(report)
            crossed_workflow_path["checks"][0]["workflow_path"] = (
                ".github/workflows/ci.yml"
            )
            with self.assertRaises(ValidationError):
                Draft202012Validator(checks_schema).validate(crossed_workflow_path)
            with self.assertRaisesRegex(ValueError, "invalid"):
                validate_report(
                    crossed_workflow_path,
                    expected_commit=document["source"]["commit"],
                )
            crossed_workflow_id = copy.deepcopy(report)
            crossed_workflow_id["checks"][0]["workflow_run_id"] += 1
            Draft202012Validator(checks_schema).validate(crossed_workflow_id)
            with self.assertRaisesRegex(ValueError, "invalid"):
                validate_report(
                    crossed_workflow_id,
                    expected_commit=document["source"]["commit"],
                )
            crossed_head_sha = copy.deepcopy(report)
            crossed_head_sha["checks"][0]["head_sha"] = "f" * 40
            Draft202012Validator(checks_schema).validate(crossed_head_sha)
            with self.assertRaisesRegex(ValueError, "invalid"):
                validate_report(
                    crossed_head_sha,
                    expected_commit=document["source"]["commit"],
                )
            wrong_order = copy.deepcopy(report)
            wrong_order["checks"][0], wrong_order["checks"][1] = (
                wrong_order["checks"][1],
                wrong_order["checks"][0],
            )
            with self.assertRaises(ValidationError):
                Draft202012Validator(checks_schema).validate(wrong_order)
            legacy_check_url = copy.deepcopy(report)
            legacy_check_url["checks"][0]["check_run_url"] = (
                "https://github.com/Hyphae-Research-Foundation/hyphae/runs/10000"
            )
            with self.assertRaises(ValidationError):
                Draft202012Validator(checks_schema).validate(legacy_check_url)

            missing_archive = copy.deepcopy(document)
            missing_archive["artifacts"] = [
                artifact
                for artifact in missing_archive["artifacts"]
                if artifact["name"]
                != (
                    f"hyphae-{document['release']['version']}-"
                    "aarch64-apple-darwin.tar.gz"
                )
            ]
            with self.assertRaises(ValidationError):
                Draft202012Validator(release_schema).validate(missing_archive)

            crossed_release_run_id = copy.deepcopy(document)
            crossed_release_run_id["workflow"]["run_id"] = "1234567"
            Draft202012Validator(release_schema).validate(crossed_release_run_id)
            with self.assertRaisesRegex(ValueError, "run identity"):
                validate_release_evidence(
                    crossed_release_run_id,
                    directory=root,
                    expected_commit=git_object("HEAD^{commit}"),
                )

            invalid = copy.deepcopy(document)
            invalid["workflow"]["run_id"] = "0123456"
            invalid["workflow"]["url"] = (
                "https://github.com/Hyphae-Research-Foundation/hyphae/actions/runs/"
                "0123456/attempts/1"
            )
            with self.assertRaisesRegex(ValueError, "positive decimal"):
                validate_release_evidence(
                    invalid,
                    directory=root,
                    expected_commit=git_object("HEAD^{commit}"),
                )
            with self.assertRaises(ValidationError):
                Draft202012Validator(release_schema).validate(invalid)

    def test_verify_cli_requires_an_explicit_commit(self) -> None:
        completed = subprocess.run(
            (
                sys.executable,
                str(ROOT / "packaging" / "release_evidence.py"),
                "verify",
                "--directory",
                ".",
                "--manifest",
                "missing.json",
            ),
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("--commit", completed.stderr)

    def test_install_extractor_rejects_parent_traversal(self) -> None:
        with tempfile.TemporaryDirectory(prefix="hyphae-install-extract-") as temporary:
            root = Path(temporary)
            archive = root / "host.zip"
            with zipfile.ZipFile(archive, "w") as bundle:
                bundle.writestr("../escape", b"forbidden")
            with self.assertRaisesRegex(RuntimeError, "unsafe archive member"):
                extract_archive(archive, root / "installed")

    def test_install_verifier_negative_control_must_fail(self) -> None:
        failed = subprocess.CompletedProcess(("hyphae", "verify"), 1, "", "corrupt")
        with patch("verify_install.subprocess.run", return_value=failed) as invoked:
            require_command_failure(Path("hyphae"), ["verify"], {})
        invoked.assert_called_once()

        accepted = subprocess.CompletedProcess(("hyphae", "verify"), 0, "{}", "")
        with patch("verify_install.subprocess.run", return_value=accepted):
            with self.assertRaisesRegex(RuntimeError, "accepted the tampered proof"):
                require_command_failure(Path("hyphae"), ["verify"], {})


if __name__ == "__main__":
    unittest.main()
