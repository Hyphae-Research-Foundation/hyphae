# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import base64
import hashlib
import io
import json
import os
import re
import socket
import stat
import subprocess
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

from conformance.postgresql import run as runner
from tools.check_postgresql_application_core import GateFailure, MANIFEST_DIGEST


ROOT = Path(__file__).resolve().parents[1]
PROFILE = ROOT / "conformance/postgresql/application-core-v1.json"
IMAGE_DIGEST = "sha256:" + "b" * 64
MANIFEST_MEDIA_TYPE = "application/vnd.oci.image.manifest.v1+json"
INDEX_MEDIA_TYPE = "application/vnd.oci.image.index.v1+json"
CONFIG_MEDIA_TYPE = "application/vnd.oci.image.config.v1+json"
CHILD_DOCUMENT = json.dumps(
    {
        "schemaVersion": 2,
        "mediaType": MANIFEST_MEDIA_TYPE,
        "config": {"mediaType": CONFIG_MEDIA_TYPE, "digest": IMAGE_DIGEST, "size": 123},
        "layers": [],
    },
    sort_keys=True,
    separators=(",", ":"),
).encode()
CHILD_DIGEST = "sha256:" + hashlib.sha256(CHILD_DOCUMENT).hexdigest()
PLATFORM = {"os": "linux", "architecture": "amd64"}
INDEX_DOCUMENT = json.dumps(
    {
        "schemaVersion": 2,
        "mediaType": INDEX_MEDIA_TYPE,
        "manifests": [
            {
                "mediaType": MANIFEST_MEDIA_TYPE,
                "digest": CHILD_DIGEST,
                "size": len(CHILD_DOCUMENT),
                "platform": PLATFORM,
            }
        ],
    },
    sort_keys=True,
    separators=(",", ":"),
).encode()
INDEX_DIGEST = "sha256:" + hashlib.sha256(INDEX_DOCUMENT).hexdigest()


def container_pin() -> dict[str, object]:
    return {
        "authority": "test",
        "reference": f"docker.io/library/postgres:test@{INDEX_DIGEST}",
        "manifest_digest": INDEX_DIGEST,
        "registry": "registry-1.docker.io",
        "repository": "library/postgres",
    }


def oci_evidence() -> dict[str, object]:
    return {
        "reference": container_pin()["reference"],
        "registry": "registry-1.docker.io",
        "repository": "library/postgres",
        "index": {
            "digest": INDEX_DIGEST,
            "media_type": INDEX_MEDIA_TYPE,
            "size": len(INDEX_DOCUMENT),
            "document_base64": base64.b64encode(INDEX_DOCUMENT).decode(),
        },
        "resolved_manifest": {
            "digest": CHILD_DIGEST,
            "media_type": MANIFEST_MEDIA_TYPE,
            "size": len(CHILD_DOCUMENT),
            "document_base64": base64.b64encode(CHILD_DOCUMENT).decode(),
            "config": {
                "digest": IMAGE_DIGEST,
                "media_type": CONFIG_MEDIA_TYPE,
                "size": 123,
            },
        },
        "image_digest": IMAGE_DIGEST,
        "platform": PLATFORM,
    }


def completed(
    command: list[str], returncode: int = 0, stdout: str = "", stderr: str = ""
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(command, returncode, stdout, stderr)


class FakeDocker:
    def __init__(
        self,
        *,
        ready: bool = True,
        failing_case: str | None = None,
        setting_mismatch: bool = False,
        digest_mismatch: bool = False,
        platform_mismatch: bool = False,
        config_mismatch: bool = False,
        cleanup_failure: bool = False,
        index_digest: str = MANIFEST_DIGEST,
    ) -> None:
        self.profile = json.loads(PROFILE.read_text(encoding="utf-8"))
        self.ready = ready
        self.failing_case = failing_case
        self.setting_mismatch = setting_mismatch
        self.digest_mismatch = digest_mismatch
        self.platform_mismatch = platform_mismatch
        self.config_mismatch = config_mismatch
        self.cleanup_failure = cleanup_failure
        self.index_digest = index_digest
        self.commands: list[tuple[list[str], str | None]] = []

    def __call__(
        self, command: list[str], input_text: str | None = None
    ) -> subprocess.CompletedProcess[str]:
        self.commands.append((command, input_text))
        if command[1] == "run":
            return completed(command, stdout="container-id\n")
        if command[1:3] == ["image", "inspect"]:
            local_image = "sha256:" + "c" * 64 if self.config_mismatch else IMAGE_DIGEST
            image = {
                "Id": local_image,
                "RepoDigests": [f"docker.io/library/postgres@{self.index_digest}"],
                "Os": "linux",
                "Architecture": "arm64" if self.platform_mismatch else "amd64",
                "Descriptor": {
                    "digest": CHILD_DIGEST,
                    "mediaType": MANIFEST_MEDIA_TYPE,
                    "size": len(CHILD_DOCUMENT),
                },
            }
            return completed(command, stdout=json.dumps([image]))
        if command[1:3] == ["container", "inspect"]:
            local_image = "sha256:" + "c" * 64 if self.config_mismatch else IMAGE_DIGEST
            image = "sha256:" + "d" * 64 if self.digest_mismatch else local_image
            return completed(
                command, stdout=json.dumps([{"Image": image, "Platform": "linux"}])
            )
        if command[1:3] == ["rm", "--force"]:
            if self.cleanup_failure:
                return completed(command, returncode=1, stderr="cleanup failed")
            return completed(command)
        if command[1] == "exec" and command[3] == "pg_isready":
            return completed(command, returncode=0 if self.ready else 1)
        if command[1] == "exec" and "--command" in command:
            postgresql = self.profile["postgresql"]
            configuration = postgresql["configuration"]
            values = [
                postgresql["server_version_num"],
                configuration["server_encoding"],
                configuration["client_encoding"],
                configuration["lc_collate"],
                configuration["lc_ctype"],
                configuration["timezone"],
                *configuration["settings"].values(),
            ]
            if self.setting_mismatch:
                values[-1] = "off"
            return completed(command, stdout="|".join(values) + "\n")
        if command[1] == "exec" and "--file=-" in command:
            match = re.search(r"SELECT 'ok:([^']+)'", input_text or "")
            if match is None:
                return completed(command, returncode=2, stderr="missing marker")
            case_id = match.group(1)
            if case_id == self.failing_case:
                return completed(command, returncode=3, stderr="oracle failed")
            return completed(command, stdout=f"ok:{case_id}\n")
        raise AssertionError(f"unexpected Docker command: {command!r}")


class FakeResponse:
    def __init__(self, body: bytes, content_digest: str | None = None) -> None:
        self.body = body
        self.headers = {"Docker-Content-Digest": content_digest}

    def __enter__(self):
        return self

    def __exit__(self, exception_type, exception, traceback):
        return False

    def read(self, _maximum: int) -> bytes:
        return self.body


class PostgreSQLApplicationCoreRunnerTests(unittest.TestCase):
    @staticmethod
    def admission() -> dict[str, object]:
        return runner.build_execution_admission("Hyphae test operator", True)

    def execute(self, fake: FakeDocker) -> dict[str, object]:
        with (
            mock.patch.object(runner, "run", side_effect=fake),
            mock.patch.object(runner, "inspect_resolved_container", return_value=oci_evidence()),
            mock.patch.object(runner, "validate_receipt"),
            mock.patch.object(runner.time, "sleep"),
            mock.patch.object(runner.platform, "system", return_value="Linux"),
            mock.patch.object(runner.platform, "machine", return_value="x86_64"),
        ):
            return runner.execute_profile("docker", PROFILE, self.admission())

    def test_startup_readiness_settings_and_resolved_digests_are_recorded(self) -> None:
        fake = FakeDocker()
        receipt = self.execute(fake)

        self.assertEqual(receipt["status"], "observed-passed")
        self.assertFalse(receipt["claim_authority"])
        self.assertEqual(receipt["claims"], [])
        self.assertFalse(receipt["closure_declared"])
        self.assertEqual(receipt["host"], {"os": "linux", "architecture": "x86_64"})
        self.assertEqual(receipt["container"]["index"]["digest"], INDEX_DIGEST)
        self.assertEqual(
            receipt["container"]["resolved_manifest"]["digest"], CHILD_DIGEST
        )
        self.assertEqual(receipt["container"]["image_digest"], IMAGE_DIGEST)
        self.assertEqual(receipt["cleanup"], "container-removed")

        start = next(command for command, _ in fake.commands if command[1] == "run")
        self.assertIn("--network=none", start)
        self.assertFalse(any(argument.startswith("--publish") for argument in start))
        configuration = fake.profile["postgresql"]["configuration"]
        self.assertIn(f"--env=POSTGRES_INITDB_ARGS={configuration['initdb_args']}", start)
        configured = [start[index + 1] for index, value in enumerate(start) if value == "-c"]
        expected = [
            f"client_encoding={configuration['client_encoding']}",
            f"timezone={configuration['timezone']}",
            *(f"{name}={value}" for name, value in configuration["settings"].items()),
        ]
        self.assertEqual(configured, expected)
        self.assertEqual(receipt["observed_configuration"]["settings"], configuration["settings"])
        self.assertRegex(receipt["source"]["tree"], r"^[0-9a-f]{40}$")
        self.assertEqual(
            set(receipt["code_authorities"]),
            {"runner", "profile_checker", "receipt_validator"},
        )
        self.assertTrue(all(result["oracle_sha256"] for result in receipt["results"]))

    def test_startup_failure_is_reported_and_attempts_cleanup(self) -> None:
        calls: list[list[str]] = []

        def fail_start(command: list[str], input_text: str | None = None):
            del input_text
            calls.append(command)
            if command[1] == "run":
                return completed(command, returncode=125, stderr="start failed")
            self.assertEqual(command[1:3], ["rm", "--force"])
            return completed(command, returncode=1, stderr="no such container")

        with mock.patch.object(runner, "run", side_effect=fail_start):
            with self.assertRaisesRegex(GateFailure, "could not start"):
                runner.execute_profile("docker", PROFILE, self.admission())
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0][1], "run")
        self.assertEqual(calls[1][1:3], ["rm", "--force"])

    def test_readiness_timeout_still_removes_the_container(self) -> None:
        fake = FakeDocker(ready=False)
        with (
            mock.patch.object(runner, "run", side_effect=fake),
            mock.patch.object(runner.time, "sleep") as sleep,
        ):
            with self.assertRaisesRegex(GateFailure, "did not become ready"):
                runner.execute_profile("docker", PROFILE, self.admission())
        readiness = [command for command, _ in fake.commands if "pg_isready" in command]
        self.assertEqual(len(readiness), 120)
        self.assertEqual(sleep.call_count, 120)
        self.assertEqual(fake.commands[-1][0][1:3], ["rm", "--force"])

    def test_case_failure_produces_a_failed_but_consistent_receipt_and_cleans_up(self) -> None:
        fake = FakeDocker(failing_case="returning")
        receipt = self.execute(fake)

        self.assertEqual(receipt["status"], "observed-failed")
        self.assertEqual(receipt["passed_count"], 10)
        failed = next(result for result in receipt["results"] if result["id"] == "returning")
        self.assertEqual(failed["status"], "observed-failed")
        self.assertEqual(failed["diagnostic"], "oracle failed")
        self.assertEqual(fake.commands[-1][0][1:3], ["rm", "--force"])

    def test_settings_and_cleanup_failures_fail_closed(self) -> None:
        for label, fake, message in [
            ("settings", FakeDocker(setting_mismatch=True), "environment differs"),
            ("cleanup", FakeDocker(cleanup_failure=True), "could not remove"),
        ]:
            with self.subTest(failure=label):
                with self.assertRaisesRegex(GateFailure, message):
                    self.execute(fake)
                self.assertEqual(fake.commands[-1][0][1:3], ["rm", "--force"])

    def test_execution_requires_explicit_non_claim_operator_admission(self) -> None:
        for operator, accepted in [("", True), ("operator", False), (" operator", True)]:
            with self.subTest(operator=operator, accepted=accepted):
                with self.assertRaisesRegex(GateFailure, "operator non-claim attestation"):
                    runner.build_execution_admission(operator, accepted)

        with self.assertRaisesRegex(GateFailure, "execution admission"):
            runner.execute_profile("docker", PROFILE, {"claim_authority": True})

    def test_receipt_output_must_be_outside_source_repository(self) -> None:
        with self.assertRaisesRegex(GateFailure, "requires an explicit --output"):
            runner.validate_output_path(ROOT, None)
        with self.assertRaisesRegex(GateFailure, "outside the source repository"):
            runner.validate_output_path(ROOT, ROOT / "receipt.json")
        with self.assertRaisesRegex(GateFailure, "outside the source repository"):
            runner.validate_output_path(ROOT, Path("relative-receipt.json"))

        with tempfile.TemporaryDirectory() as directory:
            link = Path(directory) / "receipt.json"
            link.symlink_to(ROOT / "receipt.json")
            with self.assertRaisesRegex(GateFailure, "must not be a symlink"):
                runner.validate_output_path(ROOT, link)

        with tempfile.TemporaryDirectory() as directory:
            runner.validate_output_path(ROOT, Path(directory) / "receipt.json")

    def test_secure_output_rejects_existing_special_and_alias_targets(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            output = base / "receipt.json"
            runner.write_receipt_file(ROOT, output, "{\"status\":\"observed-passed\"}\n")
            metadata = output.stat()
            self.assertTrue(stat.S_ISREG(metadata.st_mode))
            self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o600)
            original = output.read_bytes()
            with self.assertRaisesRegex(GateFailure, "already exists"):
                runner.write_receipt_file(ROOT, output, "tampered")
            self.assertEqual(output.read_bytes(), original)

            with self.assertRaisesRegex(GateFailure, "must already exist"):
                runner.validate_output_path(ROOT, base / "missing" / "receipt.json")
            parent_file = base / "parent-file"
            parent_file.write_text("not a directory", encoding="utf-8")
            with self.assertRaisesRegex(GateFailure, "parent must be a normal directory"):
                runner.validate_output_path(ROOT, parent_file / "receipt.json")

            fifo = base / "receipt.fifo"
            os.mkfifo(fifo)
            with self.assertRaisesRegex(GateFailure, "FIFO, socket, or device"):
                runner.validate_output_path(ROOT, fifo)

            unix_socket = base / "receipt.sock"
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                listener.bind(str(unix_socket))
                with self.assertRaisesRegex(GateFailure, "FIFO, socket, or device"):
                    runner.validate_output_path(ROOT, unix_socket)
            finally:
                listener.close()

            real_parent = base / "real-parent"
            real_parent.mkdir()
            linked_parent = base / "linked-parent"
            linked_parent.symlink_to(real_parent, target_is_directory=True)
            with self.assertRaisesRegex(GateFailure, "symlink components"):
                runner.validate_output_path(ROOT, linked_parent / "receipt.json")

            failed = base / "failed.json"
            with mock.patch.object(runner.os, "fsync", side_effect=OSError("injected")):
                with self.assertRaisesRegex(GateFailure, "could not publish"):
                    runner.write_receipt_file(ROOT, failed, "{}\n")
            self.assertFalse(failed.exists())

        for alias in [
            Path("/dev/null"),
            Path("/dev/stdout"),
            Path("/proc/self/fd/1"),
            Path("/sys/kernel/receipt.json"),
        ]:
            with self.subTest(alias=alias):
                with self.assertRaisesRegex(GateFailure, "is forbidden"):
                    runner.validate_output_path(ROOT, alias)

    def test_cli_requires_file_output_and_never_publishes_receipt_to_stdout(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as missing:
                runner.main(
                    [
                        "--operator",
                        "Hyphae test operator",
                        "--attest-non-claim-authoritative",
                    ]
                )
        self.assertEqual(missing.exception.code, 2)
        self.assertIn("--output", stderr.getvalue())
        self.assertEqual(stdout.getvalue(), "")

        synthetic = {"status": "observed-passed", "claim_authority": False}
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "receipt.json"
            stdout = io.StringIO()
            with mock.patch.object(runner, "execute_profile", return_value=synthetic):
                with redirect_stdout(stdout):
                    status = runner.main(
                        [
                            "--operator",
                            "Hyphae test operator",
                            "--attest-non-claim-authoritative",
                            "--output",
                            str(output),
                        ]
                    )
            self.assertEqual(status, 0)
            self.assertEqual(stdout.getvalue(), "")
            self.assertEqual(json.loads(output.read_text(encoding="utf-8")), synthetic)
            self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o600)

    def test_oci_index_membership_child_config_and_local_image_are_bound(self) -> None:
        fake = FakeDocker(index_digest=INDEX_DIGEST)

        def fetch(registry: str, repository: str, digest: str) -> bytes:
            self.assertEqual((registry, repository), ("registry-1.docker.io", "library/postgres"))
            return INDEX_DOCUMENT if digest == INDEX_DIGEST else CHILD_DOCUMENT

        with (
            mock.patch.object(runner, "run", side_effect=fake),
            mock.patch.object(runner, "fetch_authenticated_oci_document", side_effect=fetch),
        ):
            evidence = runner.inspect_resolved_container("docker", "container", container_pin())

        self.assertEqual(evidence, oci_evidence())

    def test_registry_fetch_authenticates_header_and_content_digest(self) -> None:
        token = FakeResponse(json.dumps({"token": "public-token"}).encode())
        manifest = FakeResponse(INDEX_DOCUMENT, INDEX_DIGEST)
        with mock.patch.object(runner, "urlopen", side_effect=[token, manifest]) as opened:
            raw = runner.fetch_authenticated_oci_document(
                "registry-1.docker.io", "library/postgres", INDEX_DIGEST
            )

        self.assertEqual(raw, INDEX_DOCUMENT)
        self.assertEqual(opened.call_count, 2)

        for label, response in [
            ("header", FakeResponse(INDEX_DOCUMENT, "sha256:" + "0" * 64)),
            ("content", FakeResponse(b"{}", INDEX_DIGEST)),
        ]:
            with self.subTest(mismatch=label):
                with mock.patch.object(
                    runner,
                    "urlopen",
                    side_effect=[token, response],
                ):
                    with self.assertRaisesRegex(GateFailure, "content digest"):
                        runner.fetch_authenticated_oci_document(
                            "registry-1.docker.io", "library/postgres", INDEX_DIGEST
                        )

    def test_oci_platform_config_and_running_image_mismatches_fail_closed(self) -> None:
        def fetch(_registry: str, _repository: str, digest: str) -> bytes:
            return INDEX_DOCUMENT if digest == INDEX_DIGEST else CHILD_DOCUMENT

        for label, fake, message in [
            (
                "platform",
                FakeDocker(platform_mismatch=True, index_digest=INDEX_DIGEST),
                "not a platform member",
            ),
            (
                "config",
                FakeDocker(config_mismatch=True, index_digest=INDEX_DIGEST),
                "not bound to the recorded local image identity",
            ),
            (
                "running image",
                FakeDocker(digest_mismatch=True, index_digest=INDEX_DIGEST),
                "image identity",
            ),
        ]:
            with self.subTest(mismatch=label):
                with (
                    mock.patch.object(runner, "run", side_effect=fake),
                    mock.patch.object(
                        runner, "fetch_authenticated_oci_document", side_effect=fetch
                    ),
                ):
                    with self.assertRaisesRegex(GateFailure, message):
                        runner.inspect_resolved_container("docker", "container", container_pin())


if __name__ == "__main__":
    unittest.main()
