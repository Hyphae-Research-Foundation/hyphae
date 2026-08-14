#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Orchestrate a Rust-produced thread-scaling diagnostic receipt.

The source commit and tree are supplied by an external exact-source authority;
this diagnostic tool does not inspect or claim a clean worktree. The producer
identity is independently bound by hashing the bytes passed through
``--producer`` before accepting its receipt.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
from pathlib import Path

try:
    from tools.check_native_hardware_calibration_diagnostic import (
        DIGEST,
        SHA1,
        parse_worker_counts,
        validate_receipt,
    )
except ModuleNotFoundError:
    from check_native_hardware_calibration_diagnostic import (
        DIGEST,
        SHA1,
        parse_worker_counts,
        validate_receipt,
    )


ERROR_TAIL_CHARACTERS = 4_096


def producer_blake3(producer: Path) -> str:
    try:
        from blake3 import blake3
    except ImportError as error:
        raise RuntimeError(
            "diagnostic producer verification requires blake3==1.0.9"
        ) from error
    hasher = blake3()
    with producer.open("rb") as executable:
        for chunk in iter(lambda: executable.read(1_024 * 1_024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def write_json_atomic(path: Path, payload: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(payload, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        temporary.replace(path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def run_diagnostic(
    producer: Path,
    *,
    source_commit: str,
    source_tree: str,
    platform: str,
    hardware_profile: Path,
    producer_executable_blake3: str,
    compiler_identity: str,
    hyphae_build_identity: str,
    worker_counts: list[int],
    output: Path,
    timeout_seconds: float,
) -> dict[str, object]:
    if not producer.is_file():
        raise RuntimeError(f"diagnostic producer not found: {producer}")
    if SHA1.fullmatch(source_commit) is None or SHA1.fullmatch(source_tree) is None:
        raise ValueError("diagnostic source commit and tree must be canonical Git objects")
    if DIGEST.fullmatch(producer_executable_blake3) is None:
        raise ValueError("diagnostic producer executable digest is invalid")
    measured_producer_blake3 = producer_blake3(producer)
    if measured_producer_blake3 != producer_executable_blake3:
        raise RuntimeError("diagnostic producer bytes differ from the supplied BLAKE3 digest")
    if not platform or not compiler_identity or not hyphae_build_identity:
        raise ValueError("diagnostic platform, compiler, and build identity must be non-empty")
    if timeout_seconds <= 0:
        raise ValueError("diagnostic producer timeout must be positive")
    if worker_counts != sorted(set(worker_counts)) or any(count <= 0 for count in worker_counts):
        raise ValueError("diagnostic worker counts must be positive, unique, and ordered")
    profile = json.loads(hardware_profile.read_text(encoding="utf-8"))
    hardware_fingerprint = profile.get("fingerprint") if isinstance(profile, dict) else None
    if not isinstance(hardware_fingerprint, str) or DIGEST.fullmatch(hardware_fingerprint) is None:
        raise RuntimeError("hardware profile omitted its canonical fingerprint")

    command = [
        str(producer.resolve()),
        "--hardware-calibration-diagnostic",
        "--source-commit",
        source_commit,
        "--source-tree",
        source_tree,
        "--platform",
        platform,
        "--hardware-profile",
        str(hardware_profile.resolve()),
        "--producer-executable-blake3",
        producer_executable_blake3,
        "--compiler-identity",
        compiler_identity,
        "--hyphae-build-identity",
        hyphae_build_identity,
        "--worker-counts",
        ",".join(str(count) for count in worker_counts),
    ]
    try:
        completed = subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(
            f"thread-scaling diagnostic producer exceeded {timeout_seconds:.0f}s"
        ) from error
    if completed.returncode != 0:
        stderr_tail = completed.stderr[-ERROR_TAIL_CHARACTERS:].strip()
        raise RuntimeError(
            "thread-scaling diagnostic producer failed; "
            f"stderr tail: {stderr_tail}"
        )
    try:
        payload = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError("diagnostic producer stdout was not one JSON receipt") from error
    receipt = validate_receipt(
        payload,
        expected_source_commit=source_commit,
        expected_source_tree=source_tree,
        expected_platform=platform,
        expected_hardware_fingerprint=hardware_fingerprint,
        expected_producer_executable_blake3=producer_executable_blake3,
        expected_compiler_identity=compiler_identity,
        expected_hyphae_build_identity=hyphae_build_identity,
        expected_worker_counts=worker_counts,
    )
    write_json_atomic(output, receipt)
    return receipt


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--producer", type=Path, required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--hardware-profile", type=Path, required=True)
    parser.add_argument("--producer-executable-blake3", required=True)
    parser.add_argument("--compiler-identity", required=True)
    parser.add_argument("--hyphae-build-identity", required=True)
    parser.add_argument("--worker-counts", type=parse_worker_counts, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout-seconds", type=float, default=900.0)
    arguments = parser.parse_args()
    run_diagnostic(
        arguments.producer,
        source_commit=arguments.source_commit,
        source_tree=arguments.source_tree,
        platform=arguments.platform,
        hardware_profile=arguments.hardware_profile,
        producer_executable_blake3=arguments.producer_executable_blake3,
        compiler_identity=arguments.compiler_identity,
        hyphae_build_identity=arguments.hyphae_build_identity,
        worker_counts=arguments.worker_counts,
        output=arguments.output,
        timeout_seconds=arguments.timeout_seconds,
    )
    print(json.dumps({"status": "diagnostic-only", "output": str(arguments.output)}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
