#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Run the installed Python wheel through real Windows named-pipe interrupts."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import tempfile
import venv
from pathlib import Path
from typing import Any

from tools.check_python_windows_async_gate import (
    CASES,
    validate_receipt,
    validate_transcript,
)


ROOT = Path(__file__).resolve().parents[1]
HOSTED_TEST = ROOT / "sdks/python/tests/test_v2_async_windows.py"
OUTPUT_LIMIT = 16 * 1024


class WindowsAsyncRunFailure(RuntimeError):
    """The installed wheel failed the hosted Windows lifecycle gate."""


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


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
        raise WindowsAsyncRunFailure("Git source identity is invalid")
    return value


def source_identity() -> tuple[str, str]:
    completed = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        timeout=10,
    )
    if completed.stdout:
        raise WindowsAsyncRunFailure(
            "hosted gate requires a clean exact-source checkout"
        )
    return git_object("HEAD"), git_object("HEAD^{tree}")


def wheel_version(filename: str) -> str:
    match = re.fullmatch(r"hyphae_sdk-(\d+\.\d+\.\d+)-py3-none-any\.whl", filename)
    if match is None:
        raise WindowsAsyncRunFailure("exact Python wheel filename is invalid")
    return match.group(1)


def sanitized_environment(wheel: Path, transcript: Path) -> dict[str, str]:
    environment = dict(os.environ)
    environment.pop("PYTHONPATH", None)
    for name in tuple(environment):
        if "API_KEY" in name or name.endswith("TOKEN"):
            environment.pop(name)
    environment["PYTHONNOUSERSITE"] = "1"
    environment["HYPHAE_WINDOWS_ASYNC_TRANSCRIPT"] = str(transcript)
    environment["HYPHAE_WINDOWS_ASYNC_WHEEL"] = wheel.name
    environment["HYPHAE_WINDOWS_ASYNC_VERSION"] = wheel_version(wheel.name)
    return environment


def install_exact_wheel(environment: Path, wheel: Path) -> Path:
    venv.EnvBuilder(with_pip=True, clear=True).create(environment)
    python = environment / "Scripts/python.exe"
    completed = subprocess.run(
        [
            str(python),
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-index",
            "--no-deps",
            "--force-reinstall",
            str(wheel),
        ],
        cwd=ROOT,
        capture_output=True,
        timeout=60,
    )
    if completed.returncode != 0:
        raise WindowsAsyncRunFailure("exact Python wheel could not be installed")
    return python


def _write_json(path: Path, value: dict[str, Any]) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    temporary.replace(path)


def run_hosted_test(
    python: Path,
    wheel: Path,
    transcript_path: Path,
) -> tuple[bytes, dict[str, Any]]:
    completed = subprocess.run(
        [str(python), "-I", str(HOSTED_TEST)],
        cwd=ROOT,
        env=sanitized_environment(wheel, transcript_path),
        capture_output=True,
        timeout=30,
    )
    diagnostic = completed.stdout + completed.stderr
    if len(diagnostic) > OUTPUT_LIMIT:
        raise WindowsAsyncRunFailure("hosted test output exceeded its bound")
    if b"hyp1_" in diagnostic or b"\\\\.\\pipe\\" in diagnostic:
        raise WindowsAsyncRunFailure("hosted test output exposed runtime material")
    if completed.returncode != 0:
        raise WindowsAsyncRunFailure("hosted Windows named-pipe tests failed")
    if not transcript_path.is_file():
        raise WindowsAsyncRunFailure("hosted test did not produce a transcript")
    transcript_bytes = transcript_path.read_bytes()
    return transcript_bytes, validate_transcript(json.loads(transcript_bytes), wheel)


def run(wheel: Path, output: Path, transcript_output: Path) -> dict[str, Any]:
    if os.name != "nt":
        raise WindowsAsyncRunFailure("hosted named-pipe gate requires Windows")
    wheel = wheel.resolve()
    if not wheel.is_file():
        raise WindowsAsyncRunFailure("exact Python wheel is missing")
    source_commit, source_tree = source_identity()
    with tempfile.TemporaryDirectory(prefix="hyphae-windows-async-") as directory:
        workspace = Path(directory)
        transcript_path = workspace / "transcript.json"
        python = install_exact_wheel(workspace / "venv", wheel)
        transcript_bytes, transcript = run_hosted_test(
            python,
            wheel,
            transcript_path,
        )
        transcript_output.parent.mkdir(parents=True, exist_ok=True)
        temporary_transcript = transcript_output.with_suffix(
            transcript_output.suffix + ".tmp"
        )
        temporary_transcript.write_bytes(transcript_bytes)
        temporary_transcript.replace(transcript_output)
    receipt = {
        "schema": "hyphae-python-windows-async-receipt-v1",
        "status": "passed",
        "source_commit": source_commit,
        "source_tree": source_tree,
        "platform": "windows",
        "python_version": transcript["python_version"],
        "distribution": {"filename": wheel.name, "sha256": sha256(wheel)},
        "transport": "named-pipe",
        "cases": transcript["cases"],
        "transcript_sha256": hashlib.sha256(transcript_bytes).hexdigest(),
    }
    validate_receipt(
        receipt,
        expected_source_commit=source_commit,
        expected_source_tree=source_tree,
        expected_wheel=wheel,
        expected_transcript=transcript_output,
    )
    _write_json(output, receipt)
    return receipt


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--wheel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--transcript-output", type=Path, required=True)
    args = parser.parse_args()
    try:
        receipt = run(args.wheel, args.output, args.transcript_output)
    except (
        OSError,
        ValueError,
        json.JSONDecodeError,
        subprocess.SubprocessError,
        WindowsAsyncRunFailure,
    ) as error:
        print(f"python Windows async gate failed: {error}")
        return 1
    print(
        f"python Windows async gate passed: {len(receipt['cases'])}/{len(CASES)} cases"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
