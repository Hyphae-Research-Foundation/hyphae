#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import hashlib
import io
import json
import shlex
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.check_registry_publish import (
    EXPECTED_ARTIFACTS,
    EXPECTED_AUTHORITY,
    EXPECTED_CHECKS,
    EXPECTED_CONTROL_FILES,
    ROOT,
    GateFailure,
    fetch_required_checks,
    guarded_publish_command,
    parse_arguments,
    reconcile_registry_artifact,
    reconcile_registry_layer,
    validate_authority_receipt,
    validate_evidence_receipt,
    validate_publish_authority,
    validate_publish_command,
    validate_publish_workflow,
)


COMMIT = "a" * 40
TREE = "b" * 40
TAG_OBJECT = "c" * 40
WORKFLOW_SHA = "d" * 40


def policy() -> dict:
    return json.loads(
        (ROOT / "config/registry-publish-authority.json").read_text(encoding="utf-8")
    )


def authority(ecosystem: str = "crates-io") -> dict:
    checks = []
    run_by_workflow = {
        workflow: index + 100
        for index, workflow in enumerate(dict.fromkeys(row[1] for row in EXPECTED_CHECKS))
    }
    for index, (name, workflow, event, branch) in enumerate(EXPECTED_CHECKS):
        checks.append(
            {
                "name": name,
                "check_run_id": index + 1,
                "workflow_run_id": run_by_workflow[workflow],
                "workflow_run_attempt": 1,
                "workflow": workflow,
                "event": event,
                "head_branch": branch,
                "head_sha": COMMIT,
            }
        )
    artifacts = [
        {
            "id": identifier,
            "artifact_id": index + 1,
            "name": name.format(commit=COMMIT),
            "service_digest": f"sha256:{str(index + 1) * 64}",
            "workflow": workflow,
            "workflow_run_id": run_by_workflow[workflow],
            "workflow_run_attempt": 1,
        }
        for index, (identifier, workflow, name) in enumerate(EXPECTED_ARTIFACTS)
    ]
    files = [
        {"path": path, "sha256": hashlib.sha256(path.encode()).hexdigest()}
        for path in EXPECTED_CONTROL_FILES
    ]
    return {
        "schema": "hyphae-registry-publish-github-authority-v1",
        "repository": "celiumsai/hyphae",
        "ecosystem": ecosystem,
        "source": {
            "tag": "v2.0.0",
            "tag_object": TAG_OBJECT,
            "commit": COMMIT,
            "tree": TREE,
            "origin_main": COMMIT,
        },
        "control": {
            "workflow_sha": WORKFLOW_SHA,
            "workflow_run_id": "999",
            "workflow_ref": (
                "celiumsai/hyphae/.github/workflows/registry-publish.yml@refs/heads/main"
            ),
            "files": files,
        },
        "tag_signature": {"required": False, "verified": False},
        "checks": checks,
        "artifacts": artifacts,
    }


def evidence(ecosystem: str = "crates-io") -> dict:
    source = authority(ecosystem)
    return {
        "schema": "hyphae-registry-publish-evidence-v1",
        "ecosystem": ecosystem,
        "source": source["source"],
        "control": source["control"],
        "transition": {
            "target_release": "1.2.0",
            "tree": TREE,
        },
        "release": {
            name: {"path": f"{name}.json", "sha256": "e" * 64}
            for name in (
                "release_evidence",
                "required_checks",
                "signed_release",
                "signed_release_receipt",
                "g8_aggregate",
            )
        },
        "external_ci": {
            "security_hard_kill": {
                "path": "security-hard-kill",
                "sha256": "f" * 64,
            },
            "mcp_real_hosts": {
                "path": "mcp-real-hosts",
                "sha256": "f" * 64,
            },
        },
        "package_inventory": {
            "version": "2.0.0",
            "config": "config/crates-io-release.json",
        },
    }


def publication_state(ecosystem: str = "crates-io") -> dict:
    source = authority(ecosystem)["source"]
    inventory = (
        ["crate-a", "crate-b"]
        if ecosystem == "crates-io"
        else ["@hyphae_/hyphae", "@hyphae_/hyphae-integrations"]
    )
    return {
        "schema": "hyphae-registry-publication-state-v1",
        "ecosystem": ecosystem,
        "version": "2.0.0",
        "source": source,
        "inventory": inventory,
        "status": "in-progress",
        "artifacts": {},
    }


class RegistryPublishGateTests(unittest.TestCase):
    def test_crate_download_requests_the_archive_not_a_url_document(self) -> None:
        from tools.check_registry_publish import _url_bytes

        captured = []

        class _Response:
            def __enter__(self):
                return self

            def __exit__(self, *exc_info):
                return False

            def read(self):
                return b"payload"

        def opener(request, timeout):
            captured.append(request)
            return _Response()

        with patch("tools.check_registry_publish.urllib.request.urlopen", opener):
            _url_bytes("https://crates.io/api/v1/crates/demo/2.0.0", "metadata")
            _url_bytes(
                "https://crates.io/api/v1/crates/demo/2.0.0/download",
                "crate",
                accept=None,
            )
        self.assertEqual(captured[0].get_header("Accept"), "application/json")
        self.assertIsNone(captured[1].get_header("Accept"))

    def test_crate_vcs_metadata_accepts_clean_and_rejects_dirty_sources(self) -> None:
        from tools.check_registry_publish import _crate_vcs_commit

        def crate(vcs: dict) -> bytes:
            payload = io.BytesIO()
            with tarfile.open(fileobj=payload, mode="w:gz") as archive:
                encoded = json.dumps(vcs).encode("utf-8")
                member = tarfile.TarInfo("demo-2.0.0/.cargo_vcs_info.json")
                member.size = len(encoded)
                archive.addfile(member, io.BytesIO(encoded))
            return payload.getvalue()

        clean_without_marker = {"git": {"sha1": COMMIT}, "path_in_vcs": "crates/demo"}
        clean_with_marker = {"git": {"sha1": COMMIT, "dirty": False}, "path_in_vcs": ""}
        dirty = {"git": {"sha1": COMMIT, "dirty": True}, "path_in_vcs": ""}
        self.assertEqual(_crate_vcs_commit(crate(clean_without_marker), "demo", "2.0.0"), COMMIT)
        self.assertEqual(_crate_vcs_commit(crate(clean_with_marker), "demo", "2.0.0"), COMMIT)
        with self.assertRaisesRegex(GateFailure, "dirty"):
            _crate_vcs_commit(crate(dirty), "demo", "2.0.0")

    def materialize(self, root: Path, version: str = "1.1.0") -> None:
        (root / "config").mkdir()
        (root / "config/registry-publish-authority.json").write_bytes(
            (ROOT / "config/registry-publish-authority.json").read_bytes()
        )
        (root / "config/crates-io-release.json").write_text(
            json.dumps(
                {
                    "version": version,
                    "layers": [["crate"]],
                    "apache_publication_authority": EXPECTED_AUTHORITY,
                }
            ),
            encoding="utf-8",
        )
        (root / "Cargo.toml").write_text(
            f'[workspace]\n[workspace.package]\nversion = "{version}"\nlicense = "Apache-2.0"\n',
            encoding="utf-8",
        )
        packages = []
        for path, name in (
            ("sdks/typescript", "@hyphae_/hyphae"),
            ("integrations/javascript", "@hyphae_/hyphae-integrations"),
        ):
            directory = root / path
            directory.mkdir(parents=True)
            packages.append({"path": path, "name": name})
            (directory / "package.json").write_text(
                json.dumps({"name": name, "version": version, "license": "Apache-2.0"}),
                encoding="utf-8",
            )
        (root / "config/npm-release.json").write_text(
            json.dumps(
                {
                    "version": version,
                    "packages": packages,
                    "apache_publication_authority": EXPECTED_AUTHORITY,
                }
            ),
            encoding="utf-8",
        )

    def test_checked_in_workflow_separates_unprivileged_dry_run_and_live_paths(self) -> None:
        self.assertEqual(validate_publish_workflow(ROOT), [])

    def test_exact_workflow_publish_invocations_preserve_command_arguments(self) -> None:
        workflow = (ROOT / ".github/workflows/registry-publish.yml").read_text(
            encoding="utf-8"
        )
        cargo = 'cargo publish --locked -p "$package"'
        npm_typescript = "npm publish ./sdks/typescript --provenance --access public"
        npm_integrations = "npm publish ./integrations/javascript --provenance --access public"
        for ecosystem, invocation, expected in (
            (
                "crates-io",
                cargo,
                ["cargo", "publish", "--locked", "-p", "$package"],
            ),
            (
                "npm",
                npm_typescript,
                [
                    "npm",
                    "publish",
                    "./sdks/typescript",
                    "--provenance",
                    "--access",
                    "public",
                ],
            ),
            (
                "npm",
                npm_integrations,
                [
                    "npm",
                    "publish",
                    "./integrations/javascript",
                    "--provenance",
                    "--access",
                    "public",
                ],
            ),
        ):
            with self.subTest(invocation=invocation):
                indentation = "              " if ecosystem == "crates-io" else "            "
                marker = "-- \\" + "\n" + f"{indentation}{invocation}"
                self.assertIn(marker, workflow)
                yaml_command = shlex.split(invocation)
                self.assertEqual(yaml_command, expected)
                _parser, parsed = parse_arguments(
                    [
                        "--ecosystem",
                        ecosystem,
                        "--publication-state",
                        "state.json",
                        "--",
                        *yaml_command,
                    ]
                )
                self.assertEqual(parsed.command, expected)
                with patch(
                    "tools.check_registry_publish._policy",
                    side_effect=GateFailure("receipt fixture stopped command validation"),
                ):
                    failures = validate_publish_command(ecosystem, parsed.command)
                self.assertFalse(any("command prefix differs" in item for item in failures))
                self.assertFalse(any("command shape differs" in item for item in failures))

        self.assertEqual(guarded_publish_command(["--", "--", "cargo"]), ["--", "cargo"])

    def test_legacy_npm_prefix_publish_shape_is_rejected(self) -> None:
        failures = validate_publish_command(
            "npm",
            [
                "npm",
                "--prefix",
                "sdks/typescript",
                "publish",
                "--provenance",
                "--access",
                "public",
            ],
        )
        self.assertIn("npm: publish command prefix differs", failures)
        self.assertIn("npm: publish command shape differs", failures)

    def test_npm_package_intent_packs_the_validated_root_relative_path(self) -> None:
        from tools.check_registry_publish import _package_intent

        completed = subprocess.CompletedProcess(
            [], 0, json.dumps([{"filename": "hyphae_-hyphae-1.2.0.tgz"}]), ""
        )
        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.check_registry_publish.subprocess.run", return_value=completed
        ) as run, patch("pathlib.Path.is_file", return_value=True), patch(
            "pathlib.Path.is_symlink", return_value=False
        ), patch("pathlib.Path.read_bytes", return_value=b"package"), patch(
            "tools.check_registry_publish._archive_content_sha256",
            return_value="a" * 64,
        ):
            root = Path(directory)
            self.materialize(root, "1.2.0")
            intent, _artifact = _package_intent(
                "npm",
                [
                    "npm",
                    "publish",
                    "./sdks/typescript",
                    "--provenance",
                    "--access",
                    "public",
                ],
                root,
                {"commit": COMMIT, "tree": TREE},
            )
            intent["temporary"].cleanup()
        self.assertEqual(run.call_args.args[0][:3], ["npm", "pack", "./sdks/typescript"])

    def test_workflow_mutations_fail_closed(self) -> None:
        original = (ROOT / ".github/workflows/registry-publish.yml").read_text(
            encoding="utf-8"
        )
        mutations = (
            ("environment: registry-production", "environment: unprotected"),
            ("ref: ${{ github.workflow_sha }}", "ref: refs/tags/${{ inputs.source_tag }}"),
            ("--live-resolve", "--dry-run"),
            ("--live-evidence", "--dry-run"),
            ("--live-recheck", "--dry-run"),
            (
                'test "${{ github.ref }}" = refs/heads/main',
                'test "${{ github.ref }}" = refs/tags/v1.2.0',
            ),
        )
        for before, after in mutations:
            with self.subTest(mutation=before), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                path = root / ".github/workflows/registry-publish.yml"
                path.parent.mkdir(parents=True)
                path.write_text(original.replace(before, after, 1), encoding="utf-8")
                self.assertNotEqual(validate_publish_workflow(root), [])

    def test_dry_runs_remain_available_before_the_version_bump(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.materialize(root)
            self.assertEqual(validate_publish_authority("crates-io", root, dry_run=True), [])
            self.assertEqual(validate_publish_authority("npm", root, dry_run=True), [])

    def test_live_publish_is_blocked_before_exact_1_2_2(self) -> None:
        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.check_registry_publish._git"
        ) as git:
            git.side_effect = (
                subprocess.CompletedProcess([], 0, "", ""),
                subprocess.CompletedProcess([], 1, "", "missing"),
            )
            root = Path(directory)
            self.materialize(root)
            failures = validate_publish_authority("crates-io", root)
        self.assertTrue(any("blocked until exact version 2.0.0" in item for item in failures))

    def test_policy_mutations_fail_closed(self) -> None:
        mutations = (
            ("tag", "v1.2.0"),
            ("environment", "unprotected"),
            ("required_checks", []),
            ("required_artifacts", []),
            ("control_files", ["tools/check_registry_publish.py"]),
        )
        for field, value in mutations:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.materialize(root)
                document = policy()
                document[field] = value
                (root / "config/registry-publish-authority.json").write_text(
                    json.dumps(document), encoding="utf-8"
                )
                failures = validate_publish_authority("npm", root, dry_run=True)
                self.assertTrue(any("policy" in failure for failure in failures))

    def test_authority_receipt_mutations_fail_closed(self) -> None:
        mutations = (
            ("origin-main", lambda value: value["source"].update(origin_main="f" * 40)),
            ("check-name", lambda value: value["checks"][0].update(name="Self attested")),
            ("check-sha", lambda value: value["checks"][0].update(head_sha="f" * 40)),
            ("workflow", lambda value: value["checks"][0].update(workflow="tag/check.yml")),
            ("artifact", lambda value: value["artifacts"][0].update(name="foreign")),
            ("digest", lambda value: value["artifacts"][0].update(service_digest="sha256:no")),
            ("control", lambda value: value["control"]["files"].pop()),
        )
        git = (
            subprocess.CompletedProcess([], 0, TAG_OBJECT + "\n", ""),
            subprocess.CompletedProcess([], 0, COMMIT + "\n", ""),
            subprocess.CompletedProcess([], 0, TREE + "\n", ""),
        )
        for name, mutate in mutations:
            with self.subTest(mutation=name), patch(
                "tools.check_registry_publish._git", side_effect=git
            ):
                value = authority()
                mutate(value)
                with self.assertRaises(GateFailure):
                    validate_authority_receipt(value, "crates-io", ROOT, policy())
            git = (
                subprocess.CompletedProcess([], 0, TAG_OBJECT + "\n", ""),
                subprocess.CompletedProcess([], 0, COMMIT + "\n", ""),
                subprocess.CompletedProcess([], 0, TREE + "\n", ""),
            )

    def test_security_and_mcp_external_authority_cannot_be_omitted(self) -> None:
        required = ("Security hard-kill aggregate", "MCP real hosts")
        for name in required:
            with self.subTest(check=name):
                value = authority()
                value["checks"] = [check for check in value["checks"] if check["name"] != name]
                git = (
                    subprocess.CompletedProcess([], 0, TAG_OBJECT + "\n", ""),
                    subprocess.CompletedProcess([], 0, COMMIT + "\n", ""),
                    subprocess.CompletedProcess([], 0, TREE + "\n", ""),
                )
                with patch("tools.check_registry_publish._git", side_effect=git), self.assertRaises(
                    GateFailure
                ):
                    validate_authority_receipt(value, "crates-io", ROOT, policy())

        for identifier in ("security-hard-kill", "mcp-real-hosts"):
            with self.subTest(artifact=identifier):
                value = authority()
                value["artifacts"] = [
                    artifact for artifact in value["artifacts"] if artifact["id"] != identifier
                ]
                git = (
                    subprocess.CompletedProcess([], 0, TAG_OBJECT + "\n", ""),
                    subprocess.CompletedProcess([], 0, COMMIT + "\n", ""),
                    subprocess.CompletedProcess([], 0, TREE + "\n", ""),
                )
                with patch("tools.check_registry_publish._git", side_effect=git), self.assertRaises(
                    GateFailure
                ):
                    validate_authority_receipt(value, "crates-io", ROOT, policy())

    def test_failed_security_or_mcp_exact_sha_check_fails_closed(self) -> None:
        for name in ("Security hard-kill aggregate", "MCP real hosts"):
            expected = next(
                {"name": check, "workflow": workflow, "event": event, "head_branch": branch}
                for check, workflow, event, branch in EXPECTED_CHECKS
                if check == name
            )
            candidate = {
                "id": 91,
                "name": name,
                "head_sha": COMMIT,
                "status": "completed",
                "conclusion": "failure",
                "completed_at": "2026-08-17T12:00:00Z",
                "details_url": "https://github.com/celiumsai/hyphae/actions/runs/901/job/91",
                "html_url": "https://github.com/celiumsai/hyphae/actions/runs/901/job/91",
                "app": {"id": 15368, "slug": "github-actions"},
            }
            with self.subTest(check=name), patch(
                "tools.check_registry_publish._workflow_run",
                return_value={
                    "id": 901,
                    "run_attempt": 1,
                    "path": expected["workflow"],
                    "head_sha": COMMIT,
                    "head_branch": "main",
                    "event": "push",
                    "status": "completed",
                    "conclusion": "failure",
                    "repository": {"full_name": "celiumsai/hyphae"},
                },
            ), self.assertRaisesRegex(GateFailure, "not a successful"):
                from tools.check_registry_publish import _run_for_check

                _run_for_check(candidate, expected, "celiumsai/hyphae", COMMIT, "token")

    def test_evidence_receipt_mutations_fail_closed(self) -> None:
        base_authority = authority()
        mutations = (
            ("tree", lambda value: value["transition"].update(tree="f" * 40)),
            ("version", lambda value: value["package_inventory"].update(version="1.2.0")),
            ("missing-g8", lambda value: value["release"].pop("g8_aggregate")),
            ("bad-digest", lambda value: value["release"]["g8_aggregate"].update(sha256="bad")),
            ("missing-security", lambda value: value["external_ci"].pop("security_hard_kill")),
            ("bad-mcp-digest", lambda value: value["external_ci"]["mcp_real_hosts"].update(sha256="bad")),
        )
        for name, mutate in mutations:
            with self.subTest(mutation=name):
                value = evidence()
                mutate(value)
                with self.assertRaises(GateFailure):
                    validate_evidence_receipt(value, "crates-io", base_authority)

    def test_external_ci_evidence_directories_fail_closed_when_omitted(self) -> None:
        from tools.check_registry_publish import _directory_digest

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(GateFailure, "missing"):
                _directory_digest(root / "security-hard-kill")
            empty = root / "mcp-real-hosts"
            empty.mkdir()
            with self.assertRaisesRegex(GateFailure, "empty"):
                _directory_digest(empty)

    def test_external_ci_artifact_validation_requires_both_successful_validators(self) -> None:
        from tools.check_registry_publish import _verify_external_ci_artifacts

        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            security = evidence / "security-hard-kill"
            mcp = evidence / "mcp-real-hosts"
            security.mkdir()
            mcp.mkdir()
            for shard in range(4):
                (security / f"security-process-crash-matrix-{shard}.json").write_text(
                    "{}\n", encoding="utf-8"
                )
            for name in ("claude-code.receipt.json", "codex.receipt.json"):
                (mcp / name).write_text("{}\n", encoding="utf-8")
            with patch(
                "tools.check_security_crash_matrix.validate"
            ), patch(
                "tools.check_security_crash_matrix.validate_receipts",
                side_effect=ValueError("failed security aggregate"),
            ), patch(
                "tools.check_mcp_host_receipts.validate"
            ), self.assertRaisesRegex(ValueError, "failed security aggregate"):
                _verify_external_ci_artifacts(ROOT, ROOT, evidence, COMMIT)

    def test_live_command_requires_external_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.materialize(root, "1.2.0")
            failures = validate_publish_command(
                "crates-io", ["cargo", "publish", "--locked", "-p", "crate"], root
            )
        self.assertIn("live command requires external authority and evidence receipts", failures)

    def test_live_command_rejects_packages_outside_inventory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.materialize(root, "1.2.0")
            failures = validate_publish_command(
                "crates-io", ["cargo", "publish", "--locked", "-p", "outside"], root
            )
        self.assertIn("crates-io: package is outside the exact release closure", failures)

    def test_registry_reconcile_marks_already_exact_without_publish(self) -> None:
        intended = {
            "ecosystem": "crates-io",
            "name": "crate-a",
            "version": "1.2.0",
            "bytes": b"exact crate bytes",
            "source_commit": COMMIT,
            "source_tree": TREE,
            "layer": 0,
        }
        remote = {
            "bytes": intended["bytes"],
            "metadata": b'{"checksum":"exact"}',
            "provenance": None,
            "source_commit": COMMIT,
            "provenance_verified": True,
        }
        state: dict = {}
        published: list[str] = []
        record = reconcile_registry_artifact(
            intended,
            query=lambda _intent: remote,
            publish=lambda intent: published.append(intent["name"]),
            state=state,
            persist=lambda _state: None,
            attempts=2,
            interval_seconds=0,
            sleep=lambda _seconds: None,
        )
        self.assertEqual(record["outcome"], "already-complete")
        self.assertEqual(published, [])
        self.assertEqual(state["artifacts"]["crate-a"]["status"], "complete")

    def test_registry_reconcile_fails_closed_on_exact_version_mismatch(self) -> None:
        intended = {
            "ecosystem": "npm",
            "name": "@celiums/example",
            "version": "1.2.0",
            "bytes": b"intended npm bytes",
            "source_commit": COMMIT,
            "source_tree": TREE,
            "layer": 0,
        }
        remote = {
            "bytes": b"foreign npm bytes",
            "metadata": b"{}",
            "provenance": b"signed provenance",
            "source_commit": COMMIT,
            "provenance_verified": True,
        }
        state: dict = {}
        with self.assertRaisesRegex(GateFailure, "differs from intended packaged bytes"):
            reconcile_registry_artifact(
                intended,
                query=lambda _intent: remote,
                publish=lambda _intent: self.fail("mismatched version was republished"),
                state=state,
                persist=lambda _state: None,
                attempts=1,
                interval_seconds=0,
                sleep=lambda _seconds: None,
            )
        self.assertEqual(state["artifacts"]["@celiums/example"]["status"], "mismatch")

    def test_registry_reconcile_fails_closed_when_existing_npm_provenance_is_absent(self) -> None:
        intended = {
            "ecosystem": "npm",
            "name": "@celiums/example",
            "version": "1.2.0",
            "bytes": b"intended npm bytes",
            "source_commit": COMMIT,
            "source_tree": TREE,
            "layer": 0,
        }
        remote = {
            "bytes": intended["bytes"],
            "metadata": b"{}",
            "provenance": None,
            "source_commit": None,
            "provenance_verified": False,
            "incomplete": "verified npm provenance",
        }
        state: dict = {}
        with self.assertRaisesRegex(GateFailure, "lacks verified npm provenance"):
            reconcile_registry_artifact(
                intended,
                query=lambda _intent: remote,
                publish=lambda _intent: self.fail("existing npm version was republished"),
                state=state,
                persist=lambda _state: None,
                attempts=2,
                interval_seconds=0,
                sleep=lambda _seconds: None,
            )
        self.assertEqual(state["artifacts"]["@celiums/example"]["status"], "mismatch")

    def test_registry_reconcile_continues_partial_topological_layers(self) -> None:
        intents = [
            {
                "ecosystem": "crates-io",
                "name": name,
                "version": "1.2.0",
                "bytes": f"{name} bytes".encode(),
                "source_commit": COMMIT,
                "source_tree": TREE,
                "layer": layer,
            }
            for name, layer in (("foundation", 0), ("dependent", 1))
        ]
        visible = {"foundation"}
        published: list[str] = []

        def query(intent: dict) -> dict | None:
            if intent["name"] not in visible:
                return None
            return {
                "bytes": intent["bytes"],
                "metadata": b"{}",
                "provenance": None,
                "source_commit": COMMIT,
                "provenance_verified": True,
            }

        def publish(intent: dict) -> None:
            published.append(intent["name"])
            visible.add(intent["name"])

        state: dict = {}
        first = reconcile_registry_layer(
            intents[:1],
            query=query,
            publish=publish,
            state=state,
            persist=lambda _state: None,
            attempts=2,
            interval_seconds=0,
            sleep=lambda _seconds: None,
        )
        second = reconcile_registry_layer(
            intents[1:],
            query=query,
            publish=publish,
            state=state,
            persist=lambda _state: None,
            attempts=2,
            interval_seconds=0,
            sleep=lambda _seconds: None,
        )
        self.assertEqual(first[0]["outcome"], "already-complete")
        self.assertEqual(second[0]["outcome"], "published")
        self.assertEqual(published, ["dependent"])
        self.assertTrue(all(row["status"] == "complete" for row in state["artifacts"].values()))

    def test_registry_reconcile_records_ambiguous_timeout_for_rerun(self) -> None:
        intended = {
            "ecosystem": "crates-io",
            "name": "ambiguous",
            "version": "1.2.0",
            "bytes": b"ambiguous bytes",
            "source_commit": COMMIT,
            "source_tree": TREE,
            "layer": 0,
        }
        state: dict = {}

        def failed_upload(_intent: dict) -> None:
            raise subprocess.CalledProcessError(1, ["cargo", "publish"])

        with self.assertRaisesRegex(GateFailure, "ambiguous after registry polling timeout"):
            reconcile_registry_artifact(
                intended,
                query=lambda _intent: None,
                publish=failed_upload,
                state=state,
                persist=lambda _state: None,
                attempts=2,
                interval_seconds=0,
                sleep=lambda _seconds: None,
            )
        record = state["artifacts"]["ambiguous"]
        self.assertEqual(record["status"], "ambiguous")
        self.assertEqual(record["poll_attempts"], 2)
        self.assertEqual(record["intended_sha256"], hashlib.sha256(intended["bytes"]).hexdigest())

    def test_registry_layer_waits_for_index_propagation_and_times_out(self) -> None:
        intended = {
            "ecosystem": "crates-io",
            "name": "slow-index",
            "version": "1.2.0",
            "bytes": b"slow index bytes",
            "source_commit": COMMIT,
            "source_tree": TREE,
            "layer": 0,
        }
        remote = {
            "bytes": intended["bytes"],
            "metadata": b"{}",
            "provenance": None,
            "source_commit": COMMIT,
            "provenance_verified": True,
            "layer_visible": False,
        }
        state: dict = {}
        with self.assertRaisesRegex(GateFailure, "layer propagation timed out"):
            reconcile_registry_layer(
                [intended],
                query=lambda _intent: remote,
                publish=lambda _intent: self.fail("existing artifact was republished"),
                state=state,
                persist=lambda _state: None,
                attempts=2,
                interval_seconds=0,
                sleep=lambda _seconds: None,
            )
        self.assertEqual(state["artifacts"]["slow-index"]["status"], "propagation-timeout")

    def test_publication_receipt_rerun_accepts_exact_authority_and_rejects_drift(self) -> None:
        from tools.check_registry_publish import _load_publication_state

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.materialize(root, "1.2.0")
            release = json.loads((root / "config/crates-io-release.json").read_text())
            release["layers"] = [["crate-a"], ["crate-b"]]
            (root / "config/crates-io-release.json").write_text(
                json.dumps(release), encoding="utf-8"
            )
            state_path = root / "state.json"
            state_path.write_text(json.dumps(publication_state()), encoding="utf-8")
            loaded = _load_publication_state(
                state_path, "crates-io", root, authority()
            )
            self.assertEqual(loaded["artifacts"], {})
            loaded["source"]["tree"] = "f" * 40
            state_path.write_text(json.dumps(loaded), encoding="utf-8")
            with self.assertRaisesRegex(GateFailure, "authority differs"):
                _load_publication_state(state_path, "crates-io", root, authority())

    def test_required_check_selection_pins_all_release_jobs_to_one_run(self) -> None:
        release_run = 700
        check_runs = []
        workflow_runs = {
            workflow: index + 800
            for index, workflow in enumerate(
                dict.fromkeys(workflow for _name, workflow, _event, _branch in EXPECTED_CHECKS)
            )
        }
        workflow_runs[".github/workflows/release.yml"] = release_run
        for index, (name, workflow, _event, _branch) in enumerate(EXPECTED_CHECKS):
            run_id = workflow_runs[workflow]
            check_runs.append(
                {
                    "id": index + 1,
                    "name": name,
                    "head_sha": COMMIT,
                    "completed_at": f"2026-08-16T13:{index:02d}:00Z",
                    "details_url": (
                        f"https://github.com/celiumsai/hyphae/actions/runs/{run_id}/job/{index + 1}"
                    ),
                }
            )
        release_index = next(
            index
            for index, (_name, workflow, _event, _branch) in enumerate(EXPECTED_CHECKS)
            if workflow == ".github/workflows/release.yml"
        )
        stale = copy.deepcopy(check_runs[release_index])
        stale["id"] = 999
        stale["completed_at"] = "2026-08-16T14:59:00Z"
        stale["details_url"] = (
            "https://github.com/celiumsai/hyphae/actions/runs/9999/job/999"
        )
        check_runs.append(stale)

        def verified(check, expected, _repository, _commit, _token):
            run_id = int(check["details_url"].split("/runs/", 1)[1].split("/", 1)[0])
            return {"id": run_id, "run_attempt": 1}, {}

        with patch(
            "tools.check_registry_publish._pages", return_value=check_runs
        ), patch(
            "tools.check_registry_publish._run_for_check", side_effect=verified
        ):
            selected, runs = fetch_required_checks(
                "celiumsai/hyphae", COMMIT, "token", "12345", policy()
            )
        release_checks = [
            check
            for check in selected
            if check["workflow"] == ".github/workflows/release.yml"
        ]
        self.assertTrue(release_checks)
        self.assertEqual({check["workflow_run_id"] for check in release_checks}, {release_run})
        self.assertEqual(runs[".github/workflows/release.yml"]["id"], release_run)

    def test_latest_nonrelease_check_mutations_fail_closed(self) -> None:
        check_runs = []
        workflow_runs = {
            workflow: index + 800
            for index, workflow in enumerate(
                dict.fromkeys(workflow for _name, workflow, _event, _branch in EXPECTED_CHECKS)
            )
        }
        for index, (name, workflow, _event, _branch) in enumerate(EXPECTED_CHECKS):
            run_id = workflow_runs[workflow]
            check_runs.append(
                {
                    "id": index + 1,
                    "name": name,
                    "head_sha": COMMIT,
                    "completed_at": f"2026-08-16T13:{index:02d}:00Z",
                    "details_url": (
                        f"https://github.com/celiumsai/hyphae/actions/runs/{run_id}/job/{index + 1}"
                    ),
                }
            )

        def verified(check, _expected, _repository, _commit, _token):
            if check.get("conclusion") == "failure":
                raise GateFailure("latest check failed")
            run_id = int(check["details_url"].split("/runs/", 1)[1].split("/", 1)[0])
            return {"id": run_id, "run_attempt": 1}, {}

        quality = copy.deepcopy(check_runs[0])
        quality["id"] = 999
        quality["completed_at"] = "2026-08-16T14:59:00Z"
        quality["details_url"] = (
            "https://github.com/celiumsai/hyphae/actions/runs/9999/job/999"
        )
        mutations = (
            ("failed-latest", {**quality, "conclusion": "failure"}),
            ("ambiguous-latest", quality),
        )
        for name, mutation in mutations:
            with self.subTest(mutation=name):
                candidates = copy.deepcopy(check_runs)
                if name == "failed-latest":
                    candidates.append(mutation)
                    candidates[0]["conclusion"] = "success"
                    candidates[-1]["status"] = "completed"
                else:
                    candidates[0]["completed_at"] = mutation["completed_at"]
                    candidates.append(mutation)
                with patch(
                    "tools.check_registry_publish._pages", return_value=candidates
                ), patch(
                    "tools.check_registry_publish._run_for_check", side_effect=verified
                ), self.assertRaises(GateFailure):
                    fetch_required_checks(
                        "celiumsai/hyphae", COMMIT, "token", "12345", policy()
                    )


if __name__ == "__main__":
    unittest.main()
