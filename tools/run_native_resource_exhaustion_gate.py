#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Privileged, isolated Linux resource-exhaustion gate for Native state."""

from __future__ import annotations

import argparse
import json
import os
import platform
import resource
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any, Sequence


ROOT = Path(__file__).resolve().parents[1]
IMAGE_BYTES = 128 * 1024 * 1024
REQUIRED = ("cargo", "df", "fallocate", "findmnt", "losetup", "mkfs.ext4", "mount", "sudo", "umount")


def run(
    arguments: Sequence[str | Path], *, check: bool = True, timeout: int = 120,
    preexec_fn: Any = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        tuple(str(value) for value in arguments), cwd=ROOT, check=check,
        capture_output=True, text=True, timeout=timeout, preexec_fn=preexec_fn,
    )


def sudo(*arguments: str | Path, check: bool = True) -> subprocess.CompletedProcess[str]:
    return run(("sudo", "-n", *arguments), check=check)


def require_environment() -> None:
    if not platform.system().lower() == "linux":
        raise RuntimeError("resource exhaustion gate requires Linux")
    missing = [command for command in REQUIRED if shutil.which(command) is None]
    if missing:
        raise RuntimeError(f"missing required commands: {', '.join(missing)}")
    sudo("true")


def git(*arguments: str) -> str:
    return run(("git", *arguments)).stdout.strip()


def source_tree(expected_commit: str) -> str:
    if len(expected_commit) != 40 or any(character not in "0123456789abcdef" for character in expected_commit):
        raise ValueError("source commit must be canonical lowercase SHA-1")
    if git("rev-parse", "HEAD") != expected_commit:
        raise RuntimeError("source commit differs from checked-out HEAD")
    if git("status", "--porcelain", "--untracked-files=no"):
        raise RuntimeError("tracked source worktree must be clean")
    return git("rev-parse", "HEAD^{tree}")


def build_binary() -> Path:
    run(("cargo", "build", "--release", "--locked", "-p", "hyphae-cli"), timeout=900)
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not target.is_absolute():
        target = ROOT / target
    return (target / "release" / "hyphae").resolve(strict=True)


def command(binary: Path, *arguments: str, preexec_fn: Any = None) -> subprocess.CompletedProcess[str]:
    return run((binary, *arguments), check=False, preexec_fn=preexec_fn)


def require_healthy(binary: Path, data: Path) -> None:
    completed = command(binary, "doctor", "--data-dir", str(data))
    if completed.returncode != 0 or json.loads(completed.stdout).get("status") != "healthy":
        raise RuntimeError(f"Native state is not healthy: {completed.stderr}")


def require_failure(completed: subprocess.CompletedProcess[str], label: str) -> dict[str, Any]:
    if completed.returncode == 0:
        raise RuntimeError(f"{label} unexpectedly succeeded")
    try:
        payload = json.loads(completed.stderr)
    except json.JSONDecodeError:
        payload = {"process_exit": completed.returncode, "stderr": completed.stderr.strip()[:512]}
    return payload


def limit_address_space() -> None:
    resource.setrlimit(resource.RLIMIT_AS, (32 * 1024 * 1024, 32 * 1024 * 1024))


def limit_descriptors() -> None:
    resource.setrlimit(resource.RLIMIT_NOFILE, (8, 8))


def mount_source(mountpoint: Path) -> str | None:
    completed = run(("findmnt", "--noheadings", "--output", "SOURCE", "--mountpoint", mountpoint), check=False)
    return completed.stdout.strip() if completed.returncode == 0 else None


def allocate_image(root: Path) -> tuple[Path, str, Path]:
    image = root / "resource.img"
    with image.open("xb") as output:
        output.truncate(IMAGE_BYTES)
    loop = sudo("losetup", "--find", "--show", image).stdout.strip()
    if not loop.startswith("/dev/loop") or not loop[9:].isdigit():
        raise RuntimeError(f"unsafe loop device identity: {loop!r}")
    mountpoint = root / "mount"
    mountpoint.mkdir(mode=0o700)
    sudo("mkfs.ext4", "-q", "-F", "-E", "nodiscard,lazy_itable_init=0,lazy_journal_init=0", loop)
    sudo("mount", "-t", "ext4", "-o", "rw,nodiscard", loop, mountpoint)
    sudo("chown", f"{os.getuid()}:{os.getgid()}", mountpoint)
    if mount_source(mountpoint) != loop:
        raise RuntimeError("isolated resource filesystem mount identity differs")
    return image, loop, mountpoint


def cleanup(loop: str | None, mountpoint: Path | None) -> None:
    failures = []
    if mountpoint is not None and mount_source(mountpoint) is not None:
        completed = sudo("umount", mountpoint, check=False)
        if completed.returncode != 0:
            failures.append(completed.stderr.strip())
    if loop is not None:
        completed = sudo("losetup", "--detach", loop, check=False)
        if completed.returncode != 0:
            failures.append(completed.stderr.strip())
    if failures:
        raise RuntimeError("isolated resource cleanup failed: " + "; ".join(failures))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--environment", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    require_environment()
    tree = source_tree(arguments.source_commit)
    binary = build_binary()
    loop: str | None = None
    mountpoint: Path | None = None
    observations: dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="hyphae-native-resource-", dir="/var/tmp") as temporary:
        root = Path(temporary).resolve()
        try:
            _, loop, mountpoint = allocate_image(root)
            data = mountpoint / "data"
            initialized = command(binary, "init", "--data-dir", str(data))
            if initialized.returncode != 0:
                raise RuntimeError(initialized.stderr)
            command(binary, "structure", "--data-dir", str(data), "set", "--key", "baseline", "--value", "durable").check_returncode()

            available = int(run(("df", "--output=avail", "-B1", mountpoint)).stdout.splitlines()[-1])
            filler = mountpoint / "owned-filler"
            fill_bytes = max(0, available - 32 * 1024)
            run(("fallocate", "-l", str(fill_bytes), filler))
            disk_full = command(
                binary, "structure", "--data-dir", str(data), "set", "--key", "disk-full",
                "--value", "x" * (64 * 1024),
            )
            observations["disk-full"] = require_failure(disk_full, "disk-full write")
            filler.unlink()
            require_healthy(binary, data)

            sudo("mount", "-o", "remount,ro", loop, mountpoint)
            read_only = command(binary, "structure", "--data-dir", str(data), "set", "--key", "read-only", "--value", "rejected")
            observations["read-only"] = require_failure(read_only, "read-only write")
            sudo("mount", "-o", "remount,rw", loop, mountpoint)
            require_healthy(binary, data)

            memory = command(binary, "doctor", "--data-dir", str(data), preexec_fn=limit_address_space)
            observations["memory"] = require_failure(memory, "memory-limited process")
            require_healthy(binary, data)
            descriptors = command(binary, "doctor", "--data-dir", str(data), preexec_fn=limit_descriptors)
            observations["descriptors"] = require_failure(descriptors, "descriptor-limited process")
            require_healthy(binary, data)

            oversized = json.dumps({"operation": "string_get", "keyspace": 20, "key": "k" * 70_000})
            bounded = command(binary, "structure", "--data-dir", str(data), "read", "--request-json", oversized)
            observations["bounded-input"] = require_failure(bounded, "oversized bounded input")
            require_healthy(binary, data)
        finally:
            cleanup(loop, mountpoint)

    receipt = {
        "schema": "hyphae-native-resource-exhaustion-v1",
        "status": "passed",
        "source_commit": arguments.source_commit,
        "source_tree": tree,
        "environment": arguments.environment,
        "platform": f"{platform.machine()}-{platform.system().lower()}",
        "isolation": "owned-loopback-ext4",
        "image_bytes": IMAGE_BYTES,
        "observations": observations,
        "post_failure_doctor": "healthy",
        "cleanup": "complete",
    }
    arguments.output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
