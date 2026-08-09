#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Run the complete controlled G7 state/concurrency matrix for one platform."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform as platform_module
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
STATES = ("warm",)
CONCURRENCIES = (1, 8, 32)
BACKGROUND_MODES = ("control", "interference")


def verify_source(expected_commit: str) -> str:
    if len(expected_commit) != 40 or any(
        character not in "0123456789abcdef" for character in expected_commit
    ):
        raise ValueError("source commit must be a canonical lowercase SHA-1")
    head = subprocess.run(
        ("git", "rev-parse", "HEAD"), cwd=ROOT, check=True,
        capture_output=True, text=True,
    ).stdout.strip()
    if head != expected_commit:
        raise RuntimeError("source commit differs from checked-out HEAD")
    dirty = subprocess.run(
        ("git", "status", "--porcelain", "--untracked-files=no"), cwd=ROOT,
        check=True, capture_output=True, text=True,
    ).stdout.strip()
    if dirty:
        raise RuntimeError("tracked source worktree must be clean")
    return subprocess.run(
        ("git", "rev-parse", "HEAD^{tree}"), cwd=ROOT, check=True,
        capture_output=True, text=True,
    ).stdout.strip()


def build_metadata(binary: Path, source_tree: str) -> dict[str, str]:
    rustc = subprocess.run(
        ("rustc", "-vV"), check=True, capture_output=True, text=True, timeout=30
    ).stdout.strip()
    cargo = subprocess.run(
        ("cargo", "-V"), check=True, capture_output=True, text=True, timeout=30
    ).stdout.strip()
    host = next(
        (line.removeprefix("host: ") for line in rustc.splitlines() if line.startswith("host: ")),
        "",
    )
    if not host:
        raise RuntimeError("rustc did not disclose its host target")
    return {
        "rustc": rustc,
        "cargo": cargo,
        "profile": "release",
        "target": host,
        "os": platform_module.platform(),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "source_tree": source_tree,
    }


def run_cell(binary: Path, commit: str, platform: str, state: str, concurrency: int, environment: dict[str, str] | None = None) -> dict:
    base_command = [str(binary), commit, platform, state, str(concurrency)]
    command = base_command
    perf_output: Path | None = None
    if (
        sys.platform.startswith("linux")
        and os.environ.get("HYPHAE_G7_PERF") == "1"
        and shutil.which("perf")
    ):
        descriptor = tempfile.NamedTemporaryFile(prefix="hyphae-g7-perf-", suffix=".csv", delete=False)
        descriptor.close()
        perf_output = Path(descriptor.name)
        command = [
            "perf",
            "stat",
            "-x,",
            "-e",
            "cycles,cache-misses,minor-faults,major-faults",
            "-o",
            str(perf_output),
            "--",
            *base_command,
        ]
    started = time.monotonic()
    environment = dict(os.environ if environment is None else environment)
    environment["RUST_BACKTRACE"] = "1"
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    metrics = ProcessMetrics(process.pid)
    while process.poll() is None:
        metrics.sample()
        time.sleep(0.01)
    stdout, stderr = process.communicate()
    metrics.sample()
    completed = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    if completed.returncode != 0:
        if (
            "No permission to enable" in completed.stderr
            or "Permission denied" in completed.stderr
            or "perf_event_paranoid" in completed.stderr
            or "performance monitoring" in completed.stderr
        ):
            command = base_command
            completed = subprocess.run(
                command,
                cwd=ROOT,
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )
        if perf_output is not None:
            perf_output.unlink(missing_ok=True)
        if completed.returncode != 0:
            raise RuntimeError(
                f"G7 cell failed ({state}/{concurrency}): {completed.stderr.strip()}"
            )
    payload = json.loads(completed.stdout)
    if perf_output is None:
        metrics.inject(payload)
    if perf_output is not None:
        augment_perf_counters(payload, perf_output)
        perf_output.unlink(missing_ok=True)
    payload["controller"] = {
        "wall_seconds": round(time.monotonic() - started, 6),
        "host": platform_module.platform(),
        "machine": platform_module.machine(),
    }
    return payload


class ProcessMetrics:
    def __init__(self, process_id: int) -> None:
        self.process_id = process_id
        self.peak_rss: int | None = None
        self.initial_io: dict[str, int] | None = None
        self.final_io: dict[str, int] | None = None
        self.page_faults: int | None = None

    def _status(self) -> dict[str, int]:
        if os.name == "nt":
            return self._windows_status()
        if not sys.platform.startswith("linux"):
            completed = subprocess.run(
                ("ps", "-o", "rss=", "-p", str(self.process_id)),
                capture_output=True,
                text=True,
                check=False,
            )
            value = completed.stdout.strip()
            return {"rss": int(value) * 1024} if value.isdigit() else {}
        path = Path(f"/proc/{self.process_id}/status")
        if not path.is_file():
            return {}
        values: dict[str, int] = {}
        for line in path.read_text(encoding="ascii", errors="ignore").splitlines():
            if line.startswith("VmHWM:"):
                values["rss"] = int(line.split()[1]) * 1024
        return values

    def _io(self) -> dict[str, int]:
        if os.name != "posix" or not sys.platform.startswith("linux"):
            return {}
        path = Path(f"/proc/{self.process_id}/io")
        if not path.is_file():
            return {}
        values: dict[str, int] = {}
        for line in path.read_text(encoding="ascii", errors="ignore").splitlines():
            name, _, value = line.partition(":")
            if name in {"read_bytes", "write_bytes"}:
                values[name] = int(value.strip())
        return values

    def _faults(self) -> int | None:
        if os.name != "posix" or not sys.platform.startswith("linux"):
            return None
        path = Path(f"/proc/{self.process_id}/stat")
        if not path.is_file():
            return None
        fields = path.read_text(encoding="ascii", errors="ignore").split()
        if len(fields) <= 14:
            return None
        return int(fields[9]) + int(fields[11])

    def sample(self) -> None:
        status = self._status()
        rss = status.get("rss")
        if rss is not None:
            self.peak_rss = max(self.peak_rss or 0, rss)
        io = self._io()
        if io and self.initial_io is None:
            self.initial_io = io
        if io:
            self.final_io = io
        faults = self._faults()
        if faults is not None:
            self.page_faults = faults

    def inject(self, payload: dict) -> None:
        counters = payload["counters"]
        if self.peak_rss is not None:
            counters["rss"] = {
                "status": "measured", "value": self.peak_rss, "unit": "bytes",
                "provider": "linux-proc-vmhwm",
            }
        if self.page_faults is not None:
            counters["page_faults"] = {
                "status": "measured", "value": self.page_faults, "unit": "count",
                "provider": "linux-proc-stat",
            }
        if self.initial_io is not None and self.final_io is not None:
            for source, target in (("read_bytes", "bytes_read"), ("write_bytes", "bytes_written")):
                if source in self.initial_io and source in self.final_io:
                    counters[target] = {
                        "status": "measured",
                        "value": max(0, self.final_io[source] - self.initial_io[source]),
                        "unit": "bytes",
                        "provider": "linux-proc-io",
                    }

    def _windows_status(self) -> dict[str, int]:
        if os.name != "nt":
            return {}
        import ctypes

        class Counters(ctypes.Structure):
            _fields_ = [
                ("cb", ctypes.c_ulong),
                ("PageFaultCount", ctypes.c_ulong),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        psapi = ctypes.WinDLL("psapi", use_last_error=True)
        handle = kernel32.OpenProcess(0x1000 | 0x0010, False, self.process_id)
        if not handle:
            return {}
        try:
            counters = Counters()
            counters.cb = ctypes.sizeof(counters)
            if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
                return {}
            self.page_faults = int(counters.PageFaultCount)
            return {"rss": int(counters.PeakWorkingSetSize)}
        finally:
            kernel32.CloseHandle(handle)


def augment_perf_counters(payload: dict, path: Path) -> None:
    names = {
        "cycles": "cpu_cycles",
        "cache-misses": "cache_misses",
        "minor-faults": "page_faults",
        "major-faults": "page_faults",
    }
    values: dict[str, int] = {}
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        fields = line.split(",")
        if len(fields) < 3:
            continue
        raw, event = fields[0].strip(), fields[2].strip()
        if raw.startswith("<") or event not in names:
            continue
        try:
            value = int(raw.replace(".", ""))
        except ValueError:
            continue
        target = names[event]
        values[target] = values.get(target, 0) + value
    for name in ("cpu_cycles", "cache_misses"):
        if name in values:
            payload["counters"][name] = {
                "status": "measured",
                "value": values[name],
                "unit": "cycles" if name == "cpu_cycles" else "count",
                "provider": "linux-perf-stat",
            }
    if "page_faults" in values:
        payload["counters"]["page_faults"] = {
            "status": "measured",
            "value": values["page_faults"],
            "unit": "count",
            "provider": "linux-perf-stat",
        }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--platform", default=sys.platform)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--observations", type=int, default=1_000_000)
    parser.add_argument("--warmup", type=int, default=100_000)
    parser.add_argument("--background", action="store_true")
    parser.add_argument("--hardware-file", type=Path)
    arguments = parser.parse_args()
    source_tree = verify_source(arguments.source_commit)
    hardware_path = arguments.hardware_file or (
        Path(os.environ["HYPHAE_G7_HARDWARE_FILE"])
        if "HYPHAE_G7_HARDWARE_FILE" in os.environ else None
    )
    if arguments.background and hardware_path is None:
        raise RuntimeError("complete G7 matrix requires --hardware-file")
    hardware = (
        json.loads(hardware_path.read_text(encoding="utf-8"))
        if hardware_path is not None else {
            "dedicated": False,
            "cpu": platform_module.processor() or "undisclosed",
            "topology": "undisclosed",
            "ram_bytes": 0,
            "storage": "undisclosed",
            "filesystem": "undisclosed",
            "governor": "undisclosed",
            "affinity": "uncontrolled",
            "priority": "normal",
            "background_services": "uncontrolled",
            "virtualization": "unknown",
        }
    )
    binary = ROOT / "conformance" / "g7" / "runners" / "rust" / "target" / "release" / "hyphae-native-g7-runner"
    if os.name == "nt":
        binary = binary.with_suffix(".exe")
    if not arguments.skip_build:
        subprocess.run(
            [
                "cargo",
                "build",
                "--manifest-path",
                "conformance/g7/runners/rust/Cargo.toml",
                "--locked",
                "--release",
            ],
            cwd=ROOT,
            check=True,
        )
    if not binary.is_file():
        raise RuntimeError(f"G7 runner not found: {binary}")
    build = build_metadata(binary, source_tree)
    environment = os.environ.copy()
    environment["HYPHAE_G7_OBSERVATIONS"] = str(arguments.observations)
    environment["HYPHAE_G7_WARMUP"] = str(arguments.warmup)
    receipts = []
    for state in STATES:
        for background_mode in (BACKGROUND_MODES if arguments.background else ("control",)):
            for concurrency in CONCURRENCIES:
                cell_environment = environment.copy()
                if background_mode == "interference":
                    cell_environment["HYPHAE_G7_BACKGROUND"] = "1"
                else:
                    cell_environment.pop("HYPHAE_G7_BACKGROUND", None)
                receipt = run_cell(
                    binary,
                    arguments.source_commit,
                    arguments.platform,
                    state,
                    concurrency,
                    cell_environment,
                )
                receipt["background_mode"] = background_mode
                receipt["hardware"] = hardware
                receipt["build"] = build
                receipts.append(receipt)
    for state in STATES:
        for background_mode in ({value["background_mode"] for value in receipts}):
            sweep = {
                str(concurrency): next(
                    value for value in receipts
                    if value["state"] == state
                    and value["background_mode"] == background_mode
                    and value["concurrency"] == concurrency
                )
                for concurrency in CONCURRENCIES
            }
            throughput = {
                name: {
                    level: receipt["cells"][name]["throughput_per_second"]
                    for level, receipt in sweep.items()
                }
                for name in sweep["1"]["cells"]
            }
            for receipt in sweep.values():
                receipt["saturation"] = {
                    "status": "measured",
                    "levels": list(CONCURRENCIES),
                    "method": "executed-concurrency-sweep",
                    "throughput_per_second": throughput,
                }
    if arguments.background:
        for receipt in receipts:
            if receipt["background_mode"] != "interference":
                continue
            control = next(
                value for value in receipts
                if value["state"] == receipt["state"]
                and value["concurrency"] == receipt["concurrency"]
                and value["background_mode"] == "control"
            )
            receipt["background_interference"]["p99_ratio_by_cell"] = {
                name: receipt["cells"][name]["p99"] / control["cells"][name]["p99"]
                for name in receipt["cells"]
            }
    for receipt in receipts:
        receipt.pop("controller", None)
    result = {
        "schema": "hyphae-native-g7-matrix-v2",
        "gate": "G7",
        "status": "closure-candidate",
        "source_commit": arguments.source_commit,
        "platform": arguments.platform,
        "states": list(STATES),
        "concurrency": list(CONCURRENCIES),
        "background_modes": list(BACKGROUND_MODES if arguments.background else ("control",)),
        "receipts": receipts,
        "claims": [],
        "closure_declared": False,
    }
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"status": "ok", "output": str(arguments.output), "cells": len(receipts)}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
