#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Run the installed Python SDK against one real managed Native daemon."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
import tempfile
import time
import uuid
import venv
from pathlib import Path
from typing import BinaryIO

from tools.check_python_managed_v2_conformance import (
    CASES,
    READS,
    WRITES,
    validate_receipt,
)


ROOT = Path(__file__).resolve().parents[1]
RUNNER = ROOT / "conformance/v2/python_managed_live.py"
STARTUP_LIMIT = 64 * 1024
DIAGNOSTIC_LIMIT = 4 * 1024
STARTUP_PATTERN = re.compile(
    rb"hyphae native HTTP v2 listening on (127\.0\.0\.1:[0-9]+)\r?\n"
)
TRANSCRIPT_FIELDS = {"cases", "operations", "protocol", "schema", "status"}


class LiveConformanceFailure(RuntimeError):
    """The real daemon or installed Python client did not satisfy the contract."""


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def canonical_platform() -> str:
    if sys.platform.startswith("linux"):
        return "linux"
    if sys.platform == "darwin":
        return "macos"
    if os.name == "nt":
        return "windows"
    raise LiveConformanceFailure("unsupported conformance platform")


def resolve_executable(path: Path, lane: str) -> Path:
    candidate = path
    if lane == "windows" and candidate.suffix.casefold() != ".exe":
        candidate = candidate.with_suffix(".exe")
    if not candidate.is_file():
        raise LiveConformanceFailure(
            f"required conformance executable is missing: {candidate.name}"
        )
    return candidate.resolve()


def local_endpoint(lane: str) -> tuple[str, Path | None]:
    identity = uuid.uuid4().hex[:16]
    if lane == "windows":
        return f"hyphae-python-{identity}", None
    path = Path(tempfile.gettempdir()) / f"hy-{identity}.sock"
    if len(os.fsencode(path)) >= 100:
        raise LiveConformanceFailure("AF_UNIX conformance endpoint is too long")
    return str(path), path


def git_object(expression: str) -> str:
    completed = subprocess.run(
        ["git", "rev-parse", expression],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    )
    value = completed.stdout.strip()
    if re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise LiveConformanceFailure("Git source identity is invalid")
    return value


def require_clean_status(status: bytes) -> None:
    if status:
        raise LiveConformanceFailure(
            "source-bound conformance requires a clean exact-commit worktree"
        )


def source_identity() -> tuple[str, str]:
    completed = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        timeout=10,
    )
    require_clean_status(completed.stdout)
    return git_object("HEAD"), git_object("HEAD^{tree}")


def venv_python(directory: Path) -> Path:
    if os.name == "nt":
        return directory / "Scripts/python.exe"
    return directory / "bin/python"


def install_wheel(environment: Path, wheel: Path) -> Path:
    venv.EnvBuilder(with_pip=True, clear=True).create(environment)
    python = venv_python(environment)
    completed = subprocess.run(
        [
            str(python),
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-index",
            "--no-deps",
            str(wheel),
        ],
        cwd=ROOT,
        capture_output=True,
        timeout=60,
    )
    if completed.returncode != 0:
        raise LiveConformanceFailure("exact Python wheel could not be installed")
    return python


def run_setup(
    command: list[str], timeout: int = 60, credentials: tuple[bytes, ...] = ()
) -> None:
    completed = subprocess.run(
        command,
        cwd=ROOT,
        capture_output=True,
        timeout=timeout,
    )
    assert_safe_output(completed.stdout + completed.stderr, credentials)
    if completed.returncode != 0:
        raise LiveConformanceFailure(
            "managed conformance fixture setup failed: "
            + bounded_diagnostic(completed.stderr)
        )


def wait_for_http(
    process: subprocess.Popen[bytes],
    stderr: BinaryIO,
    timeout_seconds: float,
) -> str:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise LiveConformanceFailure("managed Native daemon exited before readiness")
        stderr.seek(0)
        output = stderr.read(STARTUP_LIMIT + 1)
        if len(output) > STARTUP_LIMIT:
            raise LiveConformanceFailure(
                "managed Native daemon startup output exceeded its bound"
            )
        match = STARTUP_PATTERN.search(output)
        if match is not None:
            return f"http://{match.group(1).decode('ascii')}"
        time.sleep(0.05)
    raise LiveConformanceFailure("managed Native daemon readiness timed out")


def assert_safe_output(output: bytes, credentials: tuple[bytes, ...]) -> None:
    if len(output) > STARTUP_LIMIT:
        raise LiveConformanceFailure("process output exceeded its bound")
    if any(credential and credential in output for credential in credentials):
        raise LiveConformanceFailure("a managed credential reached process output")
    if b"hyp1_" in output:
        raise LiveConformanceFailure("credential-shaped material reached process output")


def bounded_diagnostic(output: bytes) -> str:
    if len(output) > DIAGNOSTIC_LIMIT:
        return "diagnostic exceeded its bound"
    decoded = output.decode("utf-8", errors="replace").strip()
    return decoded or "no diagnostic was emitted"


def validate_transcript(value: object) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != TRANSCRIPT_FIELDS:
        raise LiveConformanceFailure("live transcript fields differ")
    if (
        value.get("schema") != "hyphae-python-managed-v2-transcript-v1"
        or value.get("status") != "passed"
        or value.get("protocol") != {"major": 1, "minor": 2}
        or value.get("operations") != {"reads": READS, "writes": WRITES}
        or value.get("cases") != {name: True for name in CASES}
    ):
        raise LiveConformanceFailure("live transcript contract differs")
    return value


def stop_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def require_daemon_running(exit_code: int | None) -> None:
    if exit_code is not None:
        raise LiveConformanceFailure(
            "managed Native daemon exited before controlled shutdown"
        )


def sanitized_environment() -> dict[str, str]:
    environment = dict(os.environ)
    for name in tuple(environment):
        if name == "PYTHONPATH" or "API_KEY" in name or name.endswith("TOKEN"):
            environment.pop(name)
    return environment


def run(arguments: argparse.Namespace) -> dict[str, object]:
    lane = canonical_platform()
    binary = resolve_executable(arguments.binary, lane)
    fixture_binary = resolve_executable(arguments.fixture_binary, lane)
    wheel = arguments.wheel.resolve()
    if not wheel.is_file():
        raise LiveConformanceFailure("exact Python wheel is missing")
    source_commit, source_tree = source_identity()
    endpoint, socket_path = local_endpoint(lane)

    with tempfile.TemporaryDirectory(prefix="hyphae-python-managed-v2-") as directory:
        workspace = Path(directory)
        data_dir = workspace / "data"
        owner_key = workspace / "owner.key"
        auditor_key = workspace / "auditor.key"
        fixture_metadata = workspace / "fixture.json"
        transcript_path = workspace / "transcript.json"
        python = install_wheel(workspace / "venv", wheel)
        run_setup([str(binary), "init", "--data-dir", str(data_dir)])
        run_setup(
            [
                str(binary),
                "security",
                "--data-dir",
                str(data_dir),
                "bootstrap",
                "--name",
                "Python Managed Conformance Owner",
                "--label",
                "python-managed-conformance-owner",
                "--key-out",
                str(owner_key),
            ]
        )
        run_setup(
            [
                str(fixture_binary),
                "--data-dir",
                str(data_dir),
                "--owner-key-file",
                str(owner_key),
                "--auditor-key-out",
                str(auditor_key),
                "--metadata-out",
                str(fixture_metadata),
            ],
            credentials=(owner_key.read_bytes(),),
        )
        credentials = (owner_key.read_bytes(), auditor_key.read_bytes())
        with (
            (workspace / "daemon.stdout").open("w+b") as daemon_stdout,
            (workspace / "daemon.stderr").open("w+b") as daemon_stderr,
        ):
            process = subprocess.Popen(
                [
                    str(binary),
                    "serve",
                    "--data-dir",
                    str(data_dir),
                    "--endpoint",
                    endpoint,
                    "--http-bind",
                    "127.0.0.1:0",
                    "--native-api-key-auth",
                ],
                cwd=ROOT,
                stdin=subprocess.DEVNULL,
                stdout=daemon_stdout,
                stderr=daemon_stderr,
            )
            try:
                http_base_url = wait_for_http(
                    process, daemon_stderr, arguments.startup_timeout_seconds
                )
                completed = subprocess.run(
                    [
                        str(python),
                        str(RUNNER),
                        "--local-endpoint",
                        endpoint,
                        "--http-base-url",
                        http_base_url,
                        "--owner-key-file",
                        str(owner_key),
                        "--auditor-key-file",
                        str(auditor_key),
                        "--fixture-metadata",
                        str(fixture_metadata),
                        "--transcript-out",
                        str(transcript_path),
                    ],
                    cwd=ROOT,
                    env=sanitized_environment(),
                    capture_output=True,
                    timeout=120,
                )
                assert_safe_output(completed.stdout + completed.stderr, credentials)
                if completed.returncode != 0:
                    raise LiveConformanceFailure(
                        "installed Python managed conformance failed: "
                        + bounded_diagnostic(completed.stderr)
                    )
                if json.loads(completed.stdout) != {"status": "passed"}:
                    raise LiveConformanceFailure("live Python runner emitted an invalid result")
                validate_transcript(
                    json.loads(transcript_path.read_text(encoding="utf-8"))
                )
                require_daemon_running(process.poll())
            finally:
                stop_process(process)
                daemon_stdout.flush()
                daemon_stderr.flush()
                daemon_stdout.seek(0)
                daemon_stderr.seek(0)
                assert_safe_output(
                    daemon_stdout.read(STARTUP_LIMIT + 1)
                    + daemon_stderr.read(STARTUP_LIMIT + 1),
                    credentials,
                )
                if socket_path is not None:
                    socket_path.unlink(missing_ok=True)
        receipt = {
            "schema": "hyphae-python-managed-v2-conformance-receipt-v1",
            "status": "passed",
            "source_commit": source_commit,
            "source_tree": source_tree,
            "platform": lane,
            "python_version": platform.python_version(),
            "distribution": {"filename": wheel.name, "sha256": sha256(wheel)},
            "binary": {"filename": binary.name, "sha256": sha256(binary)},
            "fixture_binary": {
                "filename": fixture_binary.name,
                "sha256": sha256(fixture_binary),
            },
            "protocol": {"major": 1, "minor": 2},
            "transports": (
                ["http-v2", "named-pipe"]
                if lane == "windows"
                else ["af-unix", "http-v2"]
            ),
            "operations": {"reads": READS, "writes": WRITES},
            "cases": {name: True for name in CASES},
            "transcript_sha256": sha256(transcript_path),
        }
    validate_receipt(receipt)
    return receipt


def write_json(path: Path, value: dict[str, object]) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    temporary.replace(path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--wheel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--startup-timeout-seconds", type=float, default=20.0)
    arguments = parser.parse_args()
    if arguments.startup_timeout_seconds <= 0:
        parser.error("startup timeout must be positive")
    return arguments


def main() -> int:
    arguments = parse_args()
    try:
        receipt = run(arguments)
        write_json(arguments.output, receipt)
    except (OSError, ValueError, subprocess.SubprocessError, LiveConformanceFailure) as error:
        print(f"Python managed Native v2 live conformance failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(receipt, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
