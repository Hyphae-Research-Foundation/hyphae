#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Replay native crash boundaries from dm-log-writes stable-media records."""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Sequence


ROOT = Path(__file__).resolve().parents[1]
REPLAY_TOOL_COMMIT = "7b70d8a6863c5de30933d42a7672d35d01d2dc6c"
RECEIPT_SCHEMA = "hyphae.native.block-power-loss-replay.v1"
RECEIPT_STATUS = "block-replay-not-physical-device-cut"
READY_PREFIX = "hyphae-native-crash-ready:"
IMAGE_BYTES = 128 * 1024 * 1024
LOG_BYTES = 512 * 1024 * 1024
READY_TIMEOUT_SECONDS = 15
LABEL = re.compile(r"^[A-Za-z0-9._:-]{1,128}$")
MAPPER_NAME = re.compile(r"^hyphae-pl-[0-9]+-[0-9]+$")
LOOP_DEVICE = re.compile(r"^/dev/loop[0-9]+$")

BOUNDARIES = (
    ("commit", "blob-staged"),
    ("commit", "blob-promoted"),
    ("commit", "page-appended"),
    ("commit", "page-synchronized"),
    ("commit", "wal-appended"),
    ("commit", "wal-synchronized"),
    ("commit", "root-published"),
    ("checkpoint", "manifest-staged"),
    ("checkpoint", "manifest-published"),
    ("checkpoint", "wal-appended"),
    ("checkpoint", "wal-synchronized"),
)

REQUIRED_COMMANDS = (
    "blockdev",
    "cargo",
    "chown",
    "dmsetup",
    "e2fsck",
    "findmnt",
    "losetup",
    "mkfs.ext4",
    "make",
    "modprobe",
    "mount",
    "sudo",
    "sync",
    "umount",
)


def run(
    arguments: Sequence[str | Path],
    *,
    check: bool = True,
    timeout: int = 60,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        tuple(str(argument) for argument in arguments),
        check=check,
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def sudo(*arguments: str | Path, check: bool = True, timeout: int = 60) -> subprocess.CompletedProcess[str]:
    return run(("sudo", "-n", *arguments), check=check, timeout=timeout)


def validate_label(name: str, value: str) -> str:
    if LABEL.fullmatch(value) is None:
        raise ValueError(f"{name} must match {LABEL.pattern}")
    return value


def validate_mapper_name(name: str) -> str:
    if MAPPER_NAME.fullmatch(name) is None:
        raise ValueError(f"unsafe device-mapper name: {name!r}")
    return name


def assert_owned_path(root: Path, candidate: Path) -> Path:
    resolved_root = root.resolve(strict=True)
    resolved_candidate = candidate.resolve(strict=True)
    if resolved_candidate == resolved_root or resolved_root not in resolved_candidate.parents:
        raise RuntimeError(
            f"resource path escapes its unique run root: {resolved_candidate}"
        )
    return resolved_candidate


def require_loop_device(value: str) -> str:
    candidate = value.strip()
    if LOOP_DEVICE.fullmatch(candidate) is None:
        raise RuntimeError(f"losetup returned an unexpected device: {candidate!r}")
    return candidate


def normalize_loop_backing(value: str) -> Path:
    candidate = value.strip()
    if candidate.endswith(" (deleted)"):
        candidate = candidate[: -len(" (deleted)")]
    return Path(candidate).resolve(strict=True)


def verify_loop_backing(loop_device: str, expected_file: Path) -> None:
    completed = sudo(
        "losetup",
        "--list",
        "--noheadings",
        "--output",
        "BACK-FILE",
        loop_device,
    )
    observed = normalize_loop_backing(completed.stdout)
    expected = expected_file.resolve(strict=True)
    if observed != expected:
        raise RuntimeError(
            f"{loop_device} maps {observed}, expected owned file {expected}"
        )


def require_commands() -> None:
    missing = [name for name in REQUIRED_COMMANDS if shutil.which(name) is None]
    if missing:
        raise RuntimeError(f"missing required commands: {', '.join(missing)}")
    sudo("true")


def git_output(*arguments: str, directory: Path = ROOT) -> str:
    return run(("git", "-C", directory, *arguments)).stdout.strip()


def verify_source_identity(source_commit: str) -> str:
    validate_label("source commit", source_commit)
    head = git_output("rev-parse", "HEAD")
    if head != source_commit:
        raise RuntimeError(f"source commit {source_commit} differs from HEAD {head}")
    tracked_status = git_output("status", "--porcelain", "--untracked-files=no")
    if tracked_status:
        raise RuntimeError("tracked source worktree is not clean")
    return git_output("rev-parse", "HEAD^{tree}")


def verify_replay_tool(source: Path) -> Path:
    resolved = source.resolve(strict=True)
    if git_output("rev-parse", "HEAD", directory=resolved) != REPLAY_TOOL_COMMIT:
        raise RuntimeError(
            f"replay tool source must be exact commit {REPLAY_TOOL_COMMIT}"
        )
    tracked_status = git_output(
        "status",
        "--porcelain",
        "--untracked-files=no",
        directory=resolved,
    )
    if tracked_status:
        raise RuntimeError("tracked replay tool source worktree is not clean")
    run(("make", "-C", resolved, "all"), timeout=120)
    executable = resolved / "replay-log"
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise RuntimeError(f"replay tool executable is absent: {executable}")
    return executable


def build_probe() -> Path:
    run(
        (
            "cargo",
            "build",
            "--release",
            "--locked",
            "-p",
            "hyphae-native-runtime",
            "--example",
            "process_crash_matrix",
        ),
        timeout=600,
    )
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not target.is_absolute():
        target = ROOT / target
    binary = target / "release" / "examples" / "process_crash_matrix"
    return binary.resolve(strict=True)


def create_sparse_file(root: Path, path: Path, size: int) -> None:
    assert_owned_path(root, path.parent)
    with path.open("xb") as output:
        output.truncate(size)


def allocate_loop(root: Path, image: Path) -> str:
    assert_owned_path(root, image)
    completed = sudo("losetup", "--find", "--show", image)
    loop_device = require_loop_device(completed.stdout)
    try:
        verify_loop_backing(loop_device, image)
    except BaseException:
        sudo("losetup", "--detach", loop_device, check=False)
        raise
    return loop_device


def mapper_exists(name: str) -> bool:
    validate_mapper_name(name)
    return sudo("dmsetup", "info", name, check=False).returncode == 0


def mounted_source(mountpoint: Path) -> str | None:
    completed = run(
        ("findmnt", "--noheadings", "--output", "SOURCE", "--mountpoint", mountpoint),
        check=False,
    )
    if completed.returncode != 0:
        return None
    return completed.stdout.strip()


def read_child_ready(process: subprocess.Popen[str], expected: str) -> None:
    if process.stdout is None:
        raise RuntimeError("boundary child stdout is unavailable")
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    try:
        events = selector.select(READY_TIMEOUT_SECONDS)
    finally:
        selector.close()
    if not events:
        raise TimeoutError(f"boundary child did not emit readiness: {expected}")
    line = process.stdout.readline().rstrip("\n")
    if line != expected:
        raise RuntimeError(
            f"boundary child emitted {line!r}, expected {expected!r}"
        )


def terminate_boundary_child(process: subprocess.Popen[str]) -> str:
    os.kill(process.pid, signal.SIGKILL)
    stdout, stderr = process.communicate(timeout=10)
    if process.returncode != -signal.SIGKILL:
        raise RuntimeError(
            "boundary child was not terminated by SIGKILL: "
            f"returncode={process.returncode}, stdout={stdout!r}, stderr={stderr!r}"
        )
    return "signal-9"


def stop_child(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.kill()
    process.wait(timeout=10)


@dataclass
class Topology:
    run_root: Path
    scenario_root: Path
    mapper_name: str
    live_image: Path
    replay_image: Path
    log_image: Path
    mountpoint: Path
    loops: dict[str, str] = field(default_factory=dict)
    mapper_created: bool = False
    mounted: bool = False

    @classmethod
    def create(cls, run_root: Path, index: int) -> "Topology":
        scenario_root = run_root / f"scenario-{index:02d}"
        scenario_root.mkdir(mode=0o700)
        mountpoint = scenario_root / "mount"
        mountpoint.mkdir(mode=0o700)
        topology = cls(
            run_root=run_root,
            scenario_root=scenario_root,
            mapper_name=validate_mapper_name(f"hyphae-pl-{os.getpid()}-{index}"),
            live_image=scenario_root / "live.img",
            replay_image=scenario_root / "replay.img",
            log_image=scenario_root / "writes.log",
            mountpoint=mountpoint,
        )
        for path, size in (
            (topology.live_image, IMAGE_BYTES),
            (topology.replay_image, IMAGE_BYTES),
            (topology.log_image, LOG_BYTES),
        ):
            create_sparse_file(run_root, path, size)
        return topology

    @property
    def mapped_device(self) -> str:
        return f"/dev/mapper/{self.mapper_name}"

    @property
    def data_directory(self) -> Path:
        return self.mountpoint / "data"

    def allocate_loops(self) -> None:
        for name, image in (
            ("live", self.live_image),
            ("replay", self.replay_image),
            ("log", self.log_image),
        ):
            self.loops[name] = allocate_loop(self.run_root, image)

    def create_mapping(self) -> str:
        if mapper_exists(self.mapper_name):
            raise RuntimeError(f"device-mapper name is already occupied: {self.mapper_name}")
        sectors = sudo("blockdev", "--getsz", self.loops["live"]).stdout.strip()
        if not sectors.isdecimal() or int(sectors) <= 0:
            raise RuntimeError(f"invalid live-device sector count: {sectors!r}")
        table = (
            f"0 {sectors} log-writes "
            f"{self.loops['live']} {self.loops['log']}"
        )
        sudo("dmsetup", "create", self.mapper_name, "--table", table)
        self.mapper_created = True
        status = sudo("dmsetup", "status", self.mapper_name).stdout.strip()
        if " log-writes " not in f" {status} ":
            raise RuntimeError(f"unexpected dm-log-writes status: {status!r}")
        return status

    def format_and_mount_live(self) -> str:
        verify_loop_backing(self.loops["live"], self.live_image)
        sudo(
            "mkfs.ext4",
            "-q",
            "-F",
            "-E",
            "nodiscard,lazy_itable_init=0,lazy_journal_init=0",
            self.mapped_device,
            timeout=120,
        )
        sudo("dmsetup", "message", self.mapper_name, "0", "mark", "ext4-ready")
        self.mount(self.mapped_device)
        sudo("chown", f"{os.getuid()}:{os.getgid()}", self.mountpoint)
        sudo("sync", "-f", self.mountpoint)
        sudo("dmsetup", "message", self.mapper_name, "0", "mark", "scenario-ready")
        return self.require_mount_source(self.mapped_device)

    def mount_replay(self) -> str:
        verify_loop_backing(self.loops["replay"], self.replay_image)
        self.mount(self.loops["replay"])
        return self.require_mount_source(self.loops["replay"])

    def mount(self, source: str) -> None:
        if self.mounted:
            raise RuntimeError("scenario mountpoint is already mounted")
        sudo(
            "mount",
            "-t",
            "ext4",
            "-o",
            "rw,data=ordered,commit=600,nodiscard",
            source,
            self.mountpoint,
        )
        self.mounted = True

    def require_mount_source(self, expected: str) -> str:
        source = mounted_source(self.mountpoint)
        if source != expected:
            raise RuntimeError(
                f"mountpoint source is {source!r}, expected isolated device {expected!r}"
            )
        return run(
            (
                "findmnt",
                "--noheadings",
                "--output",
                "OPTIONS",
                "--mountpoint",
                self.mountpoint,
            )
        ).stdout.strip()

    def unmount(self) -> None:
        if not self.mounted:
            return
        source = mounted_source(self.mountpoint)
        allowed = {self.mapped_device, self.loops.get("replay")}
        if source not in allowed:
            raise RuntimeError(f"refusing to unmount unexpected source {source!r}")
        sudo("umount", self.mountpoint, timeout=120)
        self.mounted = False

    def remove_mapping(self) -> None:
        if not self.mapper_created:
            return
        validate_mapper_name(self.mapper_name)
        sudo("dmsetup", "remove", self.mapper_name, timeout=120)
        self.mapper_created = False

    def detach_loop(self, name: str) -> None:
        loop_device = self.loops.pop(name, None)
        if loop_device is None:
            return
        image = {
            "live": self.live_image,
            "replay": self.replay_image,
            "log": self.log_image,
        }[name]
        verify_loop_backing(loop_device, image)
        sudo("losetup", "--detach", loop_device)

    def cleanup(self) -> None:
        failures: list[str] = []
        for operation in (
            self.unmount,
            self.remove_mapping,
            lambda: self.detach_loop("live"),
            lambda: self.detach_loop("replay"),
            lambda: self.detach_loop("log"),
        ):
            try:
                operation()
            except BaseException as error:
                failures.append(str(error))
        if mounted_source(self.mountpoint) is not None:
            failures.append(f"mount remains active: {self.mountpoint}")
        if mapper_exists(self.mapper_name):
            failures.append(f"mapping remains active: {self.mapper_name}")
        if failures:
            raise RuntimeError("isolated cleanup failed: " + "; ".join(failures))


def start_boundary_child(
    binary: Path,
    family: str,
    data_directory: Path,
    boundary: str,
) -> subprocess.Popen[str]:
    return subprocess.Popen(
        (str(binary), "--child", family, str(data_directory), boundary),
        cwd=ROOT,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )


def parse_verification(output: str, family: str, boundary: str) -> dict[str, Any]:
    try:
        observation = json.loads(output)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"native verifier emitted invalid JSON: {output!r}") from error
    if observation.get("family") != family or observation.get("boundary") != boundary:
        raise RuntimeError(f"native verifier identity mismatch: {observation!r}")
    return observation


def run_scenario(
    *,
    index: int,
    run_root: Path,
    binary: Path,
    replay_tool: Path,
    family: str,
    boundary: str,
) -> dict[str, Any]:
    topology = Topology.create(run_root, index)
    child: subprocess.Popen[str] | None = None
    observation: dict[str, Any] | None = None
    try:
        topology.allocate_loops()
        dm_status = topology.create_mapping()
        live_mount_options = topology.format_and_mount_live()
        if family == "checkpoint":
            run((binary, "--seed-checkpoint", topology.data_directory), timeout=120)
            sudo("sync", "-f", topology.mountpoint)
            sudo(
                "dmsetup",
                "message",
                topology.mapper_name,
                "0",
                "mark",
                "checkpoint-seed-ready",
            )

        child = start_boundary_child(binary, family, topology.data_directory, boundary)
        expected_ready = f"{READY_PREFIX}{family}:{boundary}"
        read_child_ready(child, expected_ready)
        mark = f"interrupt-{family}-{boundary}"
        validate_label("interruption mark", mark)
        sudo("dmsetup", "message", topology.mapper_name, "0", "mark", mark)
        termination = terminate_boundary_child(child)
        child = None

        topology.unmount()
        topology.remove_mapping()
        topology.detach_loop("live")
        verify_loop_backing(topology.loops["log"], topology.log_image)
        verify_loop_backing(topology.loops["replay"], topology.replay_image)
        sudo(
            replay_tool,
            "--log",
            topology.loops["log"],
            "--replay",
            topology.loops["replay"],
            "--end-mark",
            mark,
            "--no-discard",
            timeout=120,
        )
        replay_mount_options = topology.mount_replay()
        verification = run(
            (
                binary,
                "--verify-power-loss",
                family,
                topology.data_directory,
                boundary,
            ),
            timeout=120,
        )
        observation = parse_verification(verification.stdout, family, boundary)
        topology.unmount()
        fsck = sudo("e2fsck", "-fn", topology.loops["replay"], check=False, timeout=120)
        if fsck.returncode != 0:
            raise RuntimeError(
                "replayed ext4 image is not clean after normal recovery: "
                f"exit={fsck.returncode}, stdout={fsck.stdout!r}, stderr={fsck.stderr!r}"
            )
        observation.update(
            {
                "termination": termination,
                "interruption_mark": mark,
                "dm_status": dm_status,
                "live_mount_options": live_mount_options,
                "replay_mount_options": replay_mount_options,
                "ext4_post_recovery_check": "e2fsck-fn-clean",
            }
        )
    finally:
        active_error = sys.exception()
        stop_child(child)
        try:
            topology.cleanup()
            shutil.rmtree(topology.scenario_root)
        except Exception as cleanup_error:
            if isinstance(active_error, Exception):
                raise ExceptionGroup(
                    f"scenario {family}:{boundary} and cleanup both failed",
                    (active_error, cleanup_error),
                ) from None
            raise
    if observation is None:
        raise RuntimeError(f"scenario {family}:{boundary} produced no observation")
    observation["cleanup"] = "complete"
    return observation


def kernel_target_version() -> str:
    sudo("modprobe", "dm-log-writes")
    targets = sudo("dmsetup", "targets").stdout.splitlines()
    for line in targets:
        fields = line.split()
        if fields and fields[0] == "log-writes":
            return " ".join(fields)
    raise RuntimeError("dm-log-writes target is unavailable after module load")


def os_release() -> str:
    values: dict[str, str] = {}
    for line in Path("/etc/os-release").read_text(encoding="utf-8").splitlines():
        key, separator, value = line.partition("=")
        if separator:
            values[key] = value.strip('"')
    return values.get("PRETTY_NAME", "unknown")


def write_receipt(receipt: dict[str, Any], output: Path | None) -> None:
    serialized = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
    if output is None:
        print(serialized, end="")
        return
    destination = output.resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.tmp-{os.getpid()}")
    temporary.write_text(serialized, encoding="utf-8")
    os.replace(temporary, destination)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--environment", required=True)
    parser.add_argument("--replay-tool-source", type=Path, required=True)
    parser.add_argument("--temporary-parent", type=Path, default=Path("/var/tmp"))
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    if not sys.platform.startswith("linux"):
        raise RuntimeError("block power-loss replay requires Linux")
    environment = validate_label("environment", arguments.environment)
    source_tree = verify_source_identity(arguments.source_commit)
    replay_tool = verify_replay_tool(arguments.replay_tool_source)
    binary = build_probe()
    temporary_parent = arguments.temporary_parent.resolve(strict=True)
    if not temporary_parent.is_dir():
        raise RuntimeError(f"temporary parent is not a directory: {temporary_parent}")
    require_commands()
    target_version = kernel_target_version()

    run_root = Path(tempfile.mkdtemp(prefix="hyphae-power-loss-", dir=temporary_parent))
    observations: list[dict[str, Any]] = []
    try:
        for index, (family, boundary) in enumerate(BOUNDARIES, start=1):
            observations.append(
                run_scenario(
                    index=index,
                    run_root=run_root,
                    binary=binary,
                    replay_tool=replay_tool,
                    family=family,
                    boundary=boundary,
                )
            )
        if any(run_root.iterdir()):
            raise RuntimeError(f"scenario cleanup left entries below {run_root}")
    finally:
        if run_root.exists() and not any(run_root.iterdir()):
            run_root.rmdir()

    receipt = {
        "schema": RECEIPT_SCHEMA,
        "status": RECEIPT_STATUS,
        "source_commit": arguments.source_commit,
        "source_tree": source_tree,
        "environment": environment,
        "target": f"{platform.machine()}-{sys.platform}",
        "os": os_release(),
        "kernel": platform.release(),
        "filesystem": "ext4",
        "device_mapper_target": target_version,
        "replay_tool_commit": REPLAY_TOOL_COMMIT,
        "image_bytes": IMAGE_BYTES,
        "log_bytes": LOG_BYTES,
        "all_engine_csn": 1,
        "scenario_count": len(observations),
        "cleanup": "complete",
        "observations": observations,
    }
    write_receipt(receipt, arguments.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
