#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Build, run, and capture Hyphae 0.2 retrieval benchmark evidence."""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=7)
    parser.add_argument(
        "--scenario",
        action="append",
        default=[],
        help="repeatable CORPUS:DIMENSIONS:TOP_K scenario",
    )
    parser.add_argument("--skip-build", action="store_true")
    return parser.parse_args()


def windows_rss(process_id: int) -> int | None:
    class ProcessMemoryCounters(ctypes.Structure):
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
    handle = kernel32.OpenProcess(0x1000 | 0x0010, False, process_id)
    if not handle:
        return None
    try:
        counters = ProcessMemoryCounters()
        counters.cb = ctypes.sizeof(counters)
        if not psapi.GetProcessMemoryInfo(
            handle, ctypes.byref(counters), counters.cb
        ):
            return None
        return int(counters.WorkingSetSize)
    finally:
        kernel32.CloseHandle(handle)


def linux_rss(process_id: int) -> int | None:
    status = Path(f"/proc/{process_id}/status")
    if not status.is_file():
        return None
    for line in status.read_text("ascii").splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) * 1024
    return None


def process_rss(process_id: int) -> int | None:
    if os.name == "nt":
        return windows_rss(process_id)
    if sys.platform.startswith("linux"):
        return linux_rss(process_id)
    completed = subprocess.run(
        ("ps", "-o", "rss=", "-p", str(process_id)),
        check=False,
        capture_output=True,
        text=True,
    )
    value = completed.stdout.strip()
    return int(value) * 1024 if value else None


def windows_descendants(process_id: int) -> set[int]:
    class ProcessEntry32(ctypes.Structure):
        _fields_ = [
            ("dwSize", ctypes.c_ulong),
            ("cntUsage", ctypes.c_ulong),
            ("th32ProcessID", ctypes.c_ulong),
            ("th32DefaultHeapID", ctypes.c_size_t),
            ("th32ModuleID", ctypes.c_ulong),
            ("cntThreads", ctypes.c_ulong),
            ("th32ParentProcessID", ctypes.c_ulong),
            ("pcPriClassBase", ctypes.c_long),
            ("dwFlags", ctypes.c_ulong),
            ("szExeFile", ctypes.c_wchar * 260),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    snapshot = kernel32.CreateToolhelp32Snapshot(0x00000002, 0)
    if snapshot in (0, ctypes.c_void_p(-1).value):
        return set()
    parents: dict[int, int] = {}
    try:
        entry = ProcessEntry32()
        entry.dwSize = ctypes.sizeof(entry)
        success = kernel32.Process32FirstW(snapshot, ctypes.byref(entry))
        while success:
            parents[int(entry.th32ProcessID)] = int(entry.th32ParentProcessID)
            success = kernel32.Process32NextW(snapshot, ctypes.byref(entry))
    finally:
        kernel32.CloseHandle(snapshot)
    descendants = {process_id}
    changed = True
    while changed:
        changed = False
        for child, parent in parents.items():
            if parent in descendants and child not in descendants:
                descendants.add(child)
                changed = True
    return descendants


def process_tree_rss(process_id: int) -> int | None:
    if os.name != "nt":
        return process_rss(process_id)
    values = [
        value
        for descendant in windows_descendants(process_id)
        if (value := windows_rss(descendant)) is not None
    ]
    return sum(values) if values else None


def main() -> int:
    arguments = parse_args()
    if arguments.iterations <= 0:
        raise SystemExit("iterations must be positive")
    output = arguments.output.resolve()
    if not arguments.skip_build:
        subprocess.run(
            (
                "cargo",
                "test",
                "--no-run",
                "--profile",
                "release",
                "-p",
                "hyphae-engine",
                "--example",
                "benchmark_retrieval_0_2",
                "--locked",
            ),
            cwd=ROOT,
            check=True,
        )
    raw_output = output.with_suffix(output.suffix + ".raw")
    raw_output.parent.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment["HYPHAE_RETRIEVAL_BENCHMARK_OUTPUT"] = str(raw_output)
    environment["HYPHAE_RETRIEVAL_BENCHMARK_ITERATIONS"] = str(arguments.iterations)
    if arguments.scenario:
        environment["HYPHAE_RETRIEVAL_BENCHMARK_SCENARIOS"] = ",".join(
            arguments.scenario
        )
    command = [
        "cargo",
        "test",
        "--profile",
        "release",
        "-p",
        "hyphae-engine",
        "--example",
        "benchmark_retrieval_0_2",
        "--locked",
        "--",
        "--ignored",
        "--exact",
        "tests::write_gate_evidence",
        "--nocapture",
    ]

    started = time.perf_counter()
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    peak_rss = 0
    while process.poll() is None:
        current = process_tree_rss(process.pid)
        if current is not None:
            peak_rss = max(peak_rss, current)
        time.sleep(0.01)
    stdout, stderr = process.communicate()
    if process.returncode != 0:
        if stderr:
            print(stderr, file=sys.stderr)
        return process.returncode
    report = json.loads(raw_output.read_text(encoding="utf-8"))
    raw_output.unlink()
    report["environment"] = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "rustc": subprocess.run(
            ("rustc", "--version"),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip(),
        "cargo": subprocess.run(
            ("cargo", "--version"),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip(),
    }
    report["process"] = {
        "wall_seconds": round(time.perf_counter() - started, 6),
        "peak_rss_bytes": peak_rss or None,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(
        json.dumps(
            {
                "status": "ok",
                "output": str(output),
                "scenarios": len(report["scenarios"]),
                "peak_rss_bytes": peak_rss or None,
            },
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
