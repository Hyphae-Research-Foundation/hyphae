#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only

"""Run the complete controlled G7 state/concurrency matrix for one platform."""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import platform as platform_module
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ElementTree
from pathlib import Path

if os.name == "posix":
    import resource

try:
    from tools.prepare_native_g7_macos_template import prepare as prepare_macos_template
    from tools.check_native_g7_receipt import validate_ann_read_view_cell
    from tools.check_native_performance_receipt import validate_progress
except ModuleNotFoundError:
    from prepare_native_g7_macos_template import prepare as prepare_macos_template
    from check_native_g7_receipt import validate_ann_read_view_cell
    from check_native_performance_receipt import validate_progress


ROOT = Path(__file__).resolve().parents[1]
STATES = ("warm",)
CONCURRENCIES = (1, 8, 32)
BACKGROUND_MODES = ("control", "interference")
ACTIVE_PROCESS: subprocess.Popen[str] | None = None
MAX_INITIAL_ANN_BULK_PARTITIONS = 111
G7_LOGICAL_ANN_PARTITIONS = 64
G7_PREFERRED_ANN_PARTITIONS = 32
G7_ANN_PARTITION_POLICY = "g7-fixed-64-logical-partitions-v1"


class ProgressWatchdog:
    def __init__(self, path: Path, timeout_seconds: float, started: float) -> None:
        self.path = path
        self.timeout_seconds = timeout_seconds
        self.last_activity = started
        self.last_sequence: int | None = None
        self.last_progress_summary = "no progress payload observed"
        self.completed = False

    def observe(self, now: float) -> None:
        if self.path.is_file():
            payload = json.loads(self.path.read_text(encoding="utf-8"))
            sequence = payload.get("sequence")
            if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence <= 0:
                raise RuntimeError("G7 runner progress has an invalid sequence")
            if self.last_sequence is None or sequence > self.last_sequence:
                self.last_sequence = sequence
                self.last_activity = now
            elif sequence < self.last_sequence:
                raise RuntimeError("G7 runner progress sequence regressed")
            details = payload.get("details")
            eta = details.get("eta") if isinstance(details, dict) else None
            self.last_progress_summary = (
                f"sequence={sequence}, stage={payload.get('stage')!r}, "
                f"completed={payload.get('completed_units')!r}/"
                f"{payload.get('total_units')!r}, eta="
                f"{json.dumps(eta, sort_keys=True, separators=(',', ':'))}"
            )
            self.completed = payload.get("status") == "completed"
        if not self.completed and now - self.last_activity >= self.timeout_seconds:
            raise RuntimeError(
                f"G7 runner progress stalled for {self.timeout_seconds:.0f}s; "
                f"last progress: {self.last_progress_summary}"
            )


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


def write_matrix_progress(
    path: Path,
    source_commit: str,
    platform: str,
    completed: list[dict[str, object]],
    total_cells: int,
    current: dict[str, object] | None,
    status: str,
    started_unix_nanos: int,
) -> None:
    write_json_atomic(path, {
        "schema": "hyphae-native-g7-matrix-progress-v1",
        "source_commit": source_commit,
        "platform": platform,
        "status": status,
        "completed_cells": completed,
        "completed_count": len(completed),
        "total_cells": total_cells,
        "current_cell": current,
        "started_unix_nanos": started_unix_nanos,
        "updated_unix_nanos": time.time_ns(),
    })


def stop_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
    else:
        process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                return
        else:
            process.kill()
        process.wait(timeout=5)


def handle_controller_signal(signum: int, _frame: object) -> None:
    process = ACTIVE_PROCESS
    if process is not None:
        stop_process(process)
    raise SystemExit(128 + signum)


class MacRusageInfoV4(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_ubyte * 16)] + [
        (name, ctypes.c_uint64)
        for name in (
            "user_time", "system_time", "package_idle_wakeups", "interrupt_wakeups",
            "pageins", "wired_size", "resident_size", "physical_footprint",
            "process_start", "process_exit", "child_user_time", "child_system_time",
            "child_package_idle_wakeups", "child_interrupt_wakeups", "child_pageins",
            "child_elapsed", "disk_bytes_read", "disk_bytes_written", "qos_default",
            "qos_maintenance", "qos_background", "qos_utility", "qos_legacy",
            "qos_user_initiated", "qos_user_interactive", "billed_system_time",
            "serviced_system_time", "logical_writes", "lifetime_max_footprint",
            "instructions", "cycles", "billed_energy", "serviced_energy",
            "interval_max_footprint", "runnable_time",
        )
    ]


def macos_rusage(process_id: int) -> MacRusageInfoV4 | None:
    if sys.platform != "darwin":
        return None
    library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    library.proc_pid_rusage.argtypes = (
        ctypes.c_int,
        ctypes.c_int,
        ctypes.POINTER(MacRusageInfoV4),
    )
    library.proc_pid_rusage.restype = ctypes.c_int
    usage = MacRusageInfoV4()
    return usage if library.proc_pid_rusage(process_id, 4, ctypes.byref(usage)) == 0 else None


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
        ("git", "status", "--porcelain", "--untracked-files=all"), cwd=ROOT,
        check=True, capture_output=True, text=True,
    ).stdout.strip()
    if dirty:
        raise RuntimeError("source worktree, including untracked files, must be clean")
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


def run_cell(
    binary: Path,
    commit: str,
    platform: str,
    state: str,
    concurrency: int,
    environment: dict[str, str] | None = None,
    macos_counter_template: Path | None = None,
    timeout_seconds: float | None = None,
    progress_path: Path | None = None,
    stall_timeout_seconds: float | None = None,
) -> dict:
    global ACTIVE_PROCESS
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
    watchdog = (
        ProgressWatchdog(progress_path, stall_timeout_seconds, started)
        if progress_path is not None and stall_timeout_seconds is not None
        else None
    )
    next_progress_check = started
    environment = dict(os.environ if environment is None else environment)
    environment["RUST_BACKTRACE"] = "1"
    child_usage_before = (
        resource.getrusage(resource.RUSAGE_CHILDREN) if os.name == "posix" else None
    )
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=os.name == "posix",
    )
    ACTIVE_PROCESS = process
    metrics = ProcessMetrics(process.pid)
    while process.poll() is None:
        metrics.sample()
        now = time.monotonic()
        if watchdog is not None and now >= next_progress_check:
            try:
                watchdog.observe(now)
            except (OSError, UnicodeError, json.JSONDecodeError, RuntimeError) as error:
                stop_process(process)
                stdout, stderr = process.communicate()
                if perf_output is not None:
                    perf_output.unlink(missing_ok=True)
                raise RuntimeError(
                    f"G7 cell progress watchdog failed ({state}/{concurrency}): {error}; "
                    f"runner stderr: {stderr.strip()}"
                ) from error
            next_progress_check = now + 1.0
        if timeout_seconds is not None and now - started >= timeout_seconds:
            stop_process(process)
            stdout, stderr = process.communicate()
            if perf_output is not None:
                perf_output.unlink(missing_ok=True)
            raise RuntimeError(
                f"G7 cell timed out after {timeout_seconds:.0f}s "
                f"({state}/{concurrency}): {stderr.strip()}"
            )
        time.sleep(0.01)
    stdout, stderr = process.communicate()
    ACTIVE_PROCESS = None
    metrics.sample()
    if child_usage_before is not None:
        metrics.record_child_usage(
            child_usage_before,
            resource.getrusage(resource.RUSAGE_CHILDREN),
        )
    completed = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    if completed.returncode != 0:
        if perf_output is not None:
            perf_output.unlink(missing_ok=True)
        raise RuntimeError(
            f"G7 cell failed ({state}/{concurrency}): {completed.stderr.strip()}"
        )
    payload = json.loads(completed.stdout)
    if perf_output is None:
        metrics.inject(payload)
    if perf_output is not None:
        augment_perf_counters(payload, perf_output)
        perf_output.unlink(missing_ok=True)
    if macos_counter_template is not None:
        cache_misses = measure_macos_cache_misses(
            binary,
            [commit, platform, state, str(concurrency)],
            environment,
            macos_counter_template,
        )
        payload["counters"]["cache_misses"] = {
            "status": "measured",
            "value": cache_misses,
            "unit": "count",
            "provider": "macos-instruments-l1d-paired-hot-path-pass",
            "pairing": "same-build-corpus-workload-after-uninstrumented-seed",
        }
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
        self.cpu_cycles: int | None = None
        self.macos_bytes_read: int | None = None
        self.macos_bytes_written: int | None = None

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
        usage = macos_rusage(self.process_id)
        if usage is not None:
            self.peak_rss = max(self.peak_rss or 0, usage.resident_size)
            self.cpu_cycles = usage.cycles
            self.macos_bytes_read = usage.disk_bytes_read
            self.macos_bytes_written = usage.disk_bytes_written

    def record_child_usage(self, before: object, after: object) -> None:
        self.page_faults = max(
            0,
            int(after.ru_minflt - before.ru_minflt + after.ru_majflt - before.ru_majflt),
        )

    def inject(self, payload: dict) -> None:
        counters = payload["counters"]
        if self.peak_rss is not None:
            counters["rss"] = {
                "status": "measured", "value": self.peak_rss, "unit": "bytes",
                "provider": "macos-proc-pid-rusage-v4" if sys.platform == "darwin" else "linux-proc-vmhwm",
            }
        if self.page_faults is not None:
            counters["page_faults"] = {
                "status": "measured", "value": self.page_faults, "unit": "count",
                "provider": "macos-getrusage-child-faults" if sys.platform == "darwin" else "linux-proc-stat",
            }
        if self.cpu_cycles is not None:
            counters["cpu_cycles"] = {
                "status": "measured", "value": self.cpu_cycles, "unit": "cycles",
                "provider": "macos-proc-pid-rusage-v4",
            }
        for name, value in (
            ("bytes_read", self.macos_bytes_read),
            ("bytes_written", self.macos_bytes_written),
        ):
            if value is not None:
                counters[name] = {
                    "status": "measured", "value": value, "unit": "bytes",
                    "provider": "macos-proc-pid-rusage-v4",
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


def parse_macos_counter_export(path: Path) -> dict[str, int]:
    root = ElementTree.parse(path).getroot()
    references: dict[str, str] = {}
    totals = {"cpu_cycles": 0, "cache_misses": 0}
    metric_names = {
        "Cycles": "cpu_cycles",
        "L1D Cache Load Misses": "cache_misses",
        "L1D Cache Store Misses": "cache_misses",
    }
    for row in root.iter("row"):
        strings: list[str] = []
        metric_value: int | None = None
        for field in row:
            reference = field.get("ref")
            value = references.get(reference, "") if reference is not None else (field.text or "")
            identity = field.get("id")
            if identity is not None:
                references[identity] = value
            if field.tag == "string":
                strings.append(value)
            elif field.tag == "fixed-decimal" and value:
                metric_value = int(float(value))
        if metric_value is None:
            continue
        metric = next((metric_names[name] for name in strings if name in metric_names), None)
        if metric is not None:
            totals[metric] += metric_value
    if totals["cpu_cycles"] <= 0 or totals["cache_misses"] < 0:
        raise RuntimeError("Instruments export did not contain Native G7 counters")
    return totals


def validate_completed_ann_progress(
    progress: dict[str, object], expected_commit: str
) -> None:
    validate_progress(progress, expected_commit)
    if (
        progress.get("operation") not in {"ann-bulk-build", "ann-seed-verify"}
        or progress.get("stage") != "ann-published"
        or progress.get("status") != "completed"
        or progress.get("unit") != "vectors"
    ):
        raise RuntimeError("G7 ANN progress did not reach durable publication")
    details = progress.get("details")
    if not isinstance(details, dict):
        raise RuntimeError("G7 ANN progress omitted its details")
    validate_progress_eta(details.get("eta"), completed=True)
    evidence = {name: value for name, value in details.items() if name != "eta"}
    validate_initial_ann_bulk_evidence(evidence, expected_commit)
    if evidence["dataset_digest"] != progress["dataset_digest"]:
        raise RuntimeError("G7 ANN progress details target another dataset")


def validate_progress_eta(value: object, *, completed: bool) -> None:
    if not isinstance(value, dict) or set(value) != {
        "status", "estimated_remaining_nanos"
    }:
        raise RuntimeError("G7 runner progress ETA fields mismatch")
    status = value["status"]
    remaining = value["estimated_remaining_nanos"]
    if completed:
        if status != "completed" or remaining != 0:
            raise RuntimeError("completed G7 progress has an invalid ETA")
        return
    if status not in {"pending", "estimated"}:
        raise RuntimeError("running G7 progress has an invalid ETA status")
    if status == "pending" and remaining is not None:
        raise RuntimeError("pending G7 progress ETA must be unknown")
    if status == "estimated" and (
        not isinstance(remaining, int) or isinstance(remaining, bool) or remaining < 0
    ):
        raise RuntimeError("estimated G7 progress ETA is invalid")


def validate_initial_ann_bulk_evidence(
    evidence: object, expected_commit: str
) -> None:
    fields = {
        "schema", "source_commit", "dataset_digest", "builder", "partition_policy", "input_identity",
        "aggregate_identity", "planned_vectors", "planned_partitions", "planned_workers",
        "planned_memory_bytes", "worker_batches", "total_time_nanos",
        "hardware_profile_fingerprint", "governor_policy_schema", "governor_mode",
        "calibration_cache_key", "topology_digest", "topology_workers", "hard_affinity",
        "governor_execution",
    }
    if not isinstance(evidence, dict) or set(evidence) != fields:
        raise RuntimeError("G7 initial ANN bulk evidence fields mismatch")
    if (
        evidence["schema"] != "hyphae-native-g7-initial-ann-bulk-v1"
        or evidence["source_commit"] != expected_commit
        or evidence["builder"] != "partitioned-hnsw-v1"
        or evidence["partition_policy"] != G7_ANN_PARTITION_POLICY
        or evidence["governor_mode"] not in {"bulk", "mixed"}
        or not isinstance(evidence["hard_affinity"], bool)
    ):
        raise RuntimeError("G7 initial ANN bulk evidence identity mismatch")
    for name in (
        "dataset_digest", "input_identity", "aggregate_identity",
        "hardware_profile_fingerprint", "topology_digest",
    ):
        value = evidence[name]
        if not isinstance(value, str) or len(value) != 64 or any(
            character not in "0123456789abcdef" for character in value
        ):
            raise RuntimeError(f"G7 initial ANN bulk {name} is not a canonical digest")
    for name in (
        "planned_vectors", "planned_partitions", "planned_workers",
        "planned_memory_bytes", "worker_batches", "topology_workers",
    ):
        value = evidence[name]
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise RuntimeError(f"G7 initial ANN bulk {name} is invalid")
    if evidence["planned_partitions"] > evidence["planned_vectors"]:
        raise RuntimeError("G7 initial ANN bulk partition plan exceeds its vectors")
    if evidence["planned_partitions"] != min(
        G7_LOGICAL_ANN_PARTITIONS, evidence["planned_vectors"]
    ):
        raise RuntimeError("G7 initial ANN bulk partition plan depends on hardware")
    if evidence["planned_partitions"] > MAX_INITIAL_ANN_BULK_PARTITIONS:
        raise RuntimeError("G7 initial ANN bulk exceeds its durable partition limit")
    if evidence["planned_workers"] > evidence["topology_workers"]:
        raise RuntimeError("G7 initial ANN bulk worker plan exceeds its topology")
    if evidence["planned_workers"] > evidence["planned_partitions"]:
        raise RuntimeError("G7 initial ANN bulk worker plan exceeds logical partitions")
    if (
        min(evidence["topology_workers"], evidence["planned_partitions"]) > 1
        and evidence["planned_workers"] <= 1
    ):
        raise RuntimeError("G7 initial ANN bulk ignored its multi-worker topology")
    if evidence["planned_workers"] > 1 and evidence["worker_batches"] <= 1:
        raise RuntimeError("G7 initial ANN bulk did not prove parallel worker batches")
    execution = evidence["governor_execution"]
    execution_fields = {
        "class", "compute_threads", "io_slots", "memory_bytes", "queue_ticket",
        "initial_queue_depth", "queue_time_nanos", "execution_time_nanos",
    }
    if (
        not isinstance(execution, dict)
        or set(execution) != execution_fields
        or execution["class"] != "bulk"
        or not isinstance(execution["compute_threads"], int)
        or isinstance(execution["compute_threads"], bool)
        or execution["compute_threads"] <= 0
        or execution["compute_threads"] != evidence["planned_workers"]
        or not isinstance(execution["memory_bytes"], int)
        or isinstance(execution["memory_bytes"], bool)
        or execution["memory_bytes"] <= 0
        or execution["memory_bytes"] != evidence["planned_memory_bytes"]
        or not isinstance(execution["io_slots"], int)
        or isinstance(execution["io_slots"], bool)
        or execution["io_slots"] != 0
        or not isinstance(execution["initial_queue_depth"], int)
        or isinstance(execution["initial_queue_depth"], bool)
        or execution["initial_queue_depth"] < 0
        or not isinstance(execution["queue_time_nanos"], int)
        or isinstance(execution["queue_time_nanos"], bool)
        or execution["queue_time_nanos"] < 0
        or not isinstance(execution["execution_time_nanos"], int)
        or isinstance(execution["execution_time_nanos"], bool)
        or execution["execution_time_nanos"] <= 0
        or (
            execution["queue_ticket"] is not None
            and (
                not isinstance(execution["queue_ticket"], int)
                or isinstance(execution["queue_ticket"], bool)
                or execution["queue_ticket"] < 0
            )
        )
    ):
        raise RuntimeError("G7 initial ANN bulk governor execution evidence mismatch")


def validate_execution_authority_evidence(
    evidence: object,
    *,
    calibration_executable_blake3: str,
    topology_digest: str,
    background: bool,
) -> None:
    fields = {
        "status", "topology_digest", "runner_executable_blake3",
        "calibration_executable_blake3", "installations", "installed_surfaces",
        "registered_pools", "local_dispatches", "stolen_dispatches",
        "completed_jobs", "numa_steal_status",
    }
    if not isinstance(evidence, dict) or set(evidence) != fields:
        raise RuntimeError("G7 execution authority evidence fields mismatch")
    if evidence["status"] != "measured":
        raise RuntimeError("G7 execution authority was not measured")
    if (
        evidence["runner_executable_blake3"] != calibration_executable_blake3
        or evidence["calibration_executable_blake3"] != calibration_executable_blake3
    ):
        raise RuntimeError("G7 execution authority targets another runner executable")
    if evidence["topology_digest"] != topology_digest:
        raise RuntimeError("G7 execution authority targets another topology")
    if evidence["numa_steal_status"] not in {"calibrated", "disabled", "not-applicable"}:
        raise RuntimeError("G7 execution authority has an invalid NUMA steal status")
    surfaces = evidence["installed_surfaces"]
    if not isinstance(surfaces, list) or surfaces != sorted(set(surfaces)):
        raise RuntimeError("G7 execution authority surfaces are not canonical")
    required = {
        "search-fixture", "embedded-structure", "embedded-sql",
        "local-structure-seed", "local-structure-migration",
        "local-structure-daemon", "local-sql-daemon", "indexed-sql",
        "join-sql", "group-commit", "physical-observation",
    }
    if background:
        required.add("background-maintenance")
    allowed = required | {"search-seed-builder"}
    if not required.issubset(surfaces) or not set(surfaces).issubset(allowed):
        raise RuntimeError("G7 execution authority omitted or invented a measured surface")
    if (
        evidence["installations"] != len(surfaces)
        or evidence["registered_pools"] != 1
    ):
        raise RuntimeError("G7 execution authority installation counts mismatch")
    counters = [
        evidence["local_dispatches"], evidence["stolen_dispatches"],
        evidence["completed_jobs"],
    ]
    if any(
        not isinstance(value, int) or isinstance(value, bool) or value < 0
        for value in counters
    ):
        raise RuntimeError("G7 execution authority counters are invalid")
    if evidence["completed_jobs"] != (
        evidence["local_dispatches"] + evidence["stolen_dispatches"]
    ):
        raise RuntimeError("G7 execution authority dispatch counters do not reconcile")
    if evidence["numa_steal_status"] != "calibrated" and evidence["stolen_dispatches"] != 0:
        raise RuntimeError("G7 execution authority stole work while NUMA stealing was disabled")


def load_contract_path(path: Path, label: str) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise RuntimeError(f"{label} must be a regular non-symlink file")
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise RuntimeError(f"{label} must contain one JSON object")
    return payload


def prepare_macos_counter_template(directory: Path) -> Path:
    bootstrap = directory / "bootstrap.trace"
    completed = subprocess.run(
        (
            "xcrun", "xctrace", "record", "--quiet", "--no-prompt",
            "--template", "CPU Counters", "--time-limit", "1s",
            "--output", str(bootstrap), "--launch", "--", "/usr/bin/yes",
        ),
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    source = bootstrap / "form.template"
    if not source.is_file():
        raise RuntimeError(f"failed to bootstrap Instruments CPU Counters: {completed.stderr.strip()}")
    template = directory / "L1D.tracetemplate"
    prepare_macos_template(source, template)
    return template


def measure_macos_cache_misses(
    binary: Path,
    arguments: list[str],
    environment: dict[str, str],
    template: Path,
) -> int:
    with tempfile.TemporaryDirectory(prefix="hyphae-g7-xctrace-") as directory:
        temporary = Path(directory)
        trace = temporary / "counters.trace"
        ready = temporary / "ready"
        start = temporary / "start"
        profiled_environment = environment.copy()
        profiled_environment["HYPHAE_G7_PROFILE_READY_FILE"] = str(ready)
        profiled_environment["HYPHAE_G7_PROFILE_START_FILE"] = str(start)
        runner = subprocess.Popen(
            [str(binary), *arguments],
            cwd=ROOT,
            env=profiled_environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        while not ready.is_file():
            if runner.poll() is not None:
                stdout, stderr = runner.communicate()
                raise RuntimeError(f"macOS counter runner failed before profiling: {stderr.strip()} {stdout.strip()}")
            time.sleep(0.05)
        if ready.read_text(encoding="ascii").strip() != str(runner.pid):
            runner.terminate()
            raise RuntimeError("macOS counter runner disclosed a different process ID")
        profiler = subprocess.Popen(
            [
                "xcrun", "xctrace", "record", "--quiet", "--no-prompt",
                "--template", str(template), "--output", str(trace),
                "--attach", str(runner.pid),
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        profiler_deadline = time.monotonic() + 30
        while not trace.is_dir():
            if profiler.poll() is not None:
                _, profiler_stderr = profiler.communicate()
                start.touch()
                runner.communicate()
                raise RuntimeError(f"macOS profiler failed to attach: {profiler_stderr.strip()}")
            if time.monotonic() >= profiler_deadline:
                profiler.terminate()
                start.touch()
                runner.communicate()
                raise RuntimeError("macOS profiler did not start within 30 seconds")
            time.sleep(0.05)
        time.sleep(0.5)
        start.touch()
        stdout, stderr = runner.communicate()
        _, profiler_stderr = profiler.communicate()
        if runner.returncode != 0:
            raise RuntimeError(f"macOS counter runner failed: {stderr.strip()}")
        if profiler.returncode != 0:
            raise RuntimeError(f"macOS profiler failed: {profiler_stderr.strip()}")
        json.loads(stdout)
        export = temporary / "metrics.xml"
        subprocess.run(
            (
                "xcrun", "xctrace", "export", "--input", str(trace),
                "--xpath", '/trace-toc/run/data/table[@schema="MetricTable"]',
                "--output", str(export),
            ),
            capture_output=True,
            text=True,
            check=True,
        )
        return parse_macos_counter_export(export)["cache_misses"]


def main() -> int:
    if os.name == "posix":
        signal.signal(signal.SIGTERM, handle_controller_signal)
        signal.signal(signal.SIGINT, handle_controller_signal)
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--platform", default=sys.platform)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--observations", type=int, default=1_000_000)
    parser.add_argument("--warmup", type=int, default=100_000)
    parser.add_argument("--background", action="store_true")
    parser.add_argument("--hardware-file", type=Path)
    parser.add_argument("--hardware-profile", type=Path)
    parser.add_argument("--governor-policy", type=Path)
    parser.add_argument("--hardware-calibration", type=Path)
    parser.add_argument("--execution-topology", type=Path)
    parser.add_argument("--cell-timeout-seconds", type=int, default=7_200)
    parser.add_argument("--matrix-timeout-seconds", type=int, default=39_600)
    parser.add_argument("--stall-timeout-seconds", type=int, default=1_800)
    arguments = parser.parse_args()
    if (
        arguments.cell_timeout_seconds <= 0
        or arguments.matrix_timeout_seconds <= 0
        or arguments.stall_timeout_seconds <= 0
    ):
        raise ValueError("G7 timeout bounds must be positive")
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
    contract_arguments = (
        (
            "hardware profile",
            arguments.hardware_profile,
            "HYPHAE_G7_HARDWARE_PROFILE_FILE",
        ),
        (
            "hardware calibration",
            arguments.hardware_calibration,
            "HYPHAE_G7_HARDWARE_CALIBRATION_FILE",
        ),
        (
            "governor policy",
            arguments.governor_policy,
            "HYPHAE_G7_GOVERNOR_POLICY_FILE",
        ),
        (
            "execution topology",
            arguments.execution_topology,
            "HYPHAE_G7_EXECUTION_TOPOLOGY_FILE",
        ),
    )
    contract_paths: dict[str, Path] = {}
    contract_values: dict[str, dict[str, object]] = {}
    for label, argument_path, environment_name in contract_arguments:
        path = argument_path or (
            Path(os.environ[environment_name])
            if environment_name in os.environ else None
        )
        if path is None:
            raise RuntimeError(f"G7 runner requires --{label.replace(' ', '-')}")
        contract_values[environment_name] = load_contract_path(path, label)
        contract_paths[environment_name] = path.resolve()
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
    macos_counter_workspace: Path | None = None
    macos_counter_template: Path | None = None
    if sys.platform == "darwin":
        macos_counter_workspace = Path(tempfile.mkdtemp(prefix="hyphae-g7-template-"))
        macos_counter_template = prepare_macos_counter_template(macos_counter_workspace)
    environment = os.environ.copy()
    environment["HYPHAE_G7_OBSERVATIONS"] = str(arguments.observations)
    environment["HYPHAE_G7_WARMUP"] = str(arguments.warmup)
    environment["HYPHAE_G7_SOURCE_TREE"] = source_tree
    for environment_name, path in contract_paths.items():
        environment[environment_name] = str(path)
    progress_path = arguments.output.with_name(
        f"{arguments.output.stem}.progress.json"
    )
    runner_progress_path = arguments.output.with_name(
        f"{arguments.output.stem}.runner-progress.json"
    )
    progress_path.unlink(missing_ok=True)
    runner_progress_path.unlink(missing_ok=True)
    environment["HYPHAE_G7_PROGRESS_FILE"] = str(runner_progress_path.resolve())
    transient_seed_workspace: Path | None = None
    if arguments.background:
        data_root_value = environment.get("HYPHAE_G7_DATA_ROOT")
        if data_root_value is None:
            raise RuntimeError("complete G7 matrix requires HYPHAE_G7_DATA_ROOT")
        data_root = Path(data_root_value)
        if not data_root.is_absolute() or data_root.is_symlink() or not data_root.is_dir():
            raise RuntimeError("G7 data root must be an existing absolute real directory")
    if "HYPHAE_G7_SEARCH_SEED_ROOT" not in environment:
        data_root = environment.get("HYPHAE_G7_DATA_ROOT")
        if data_root is None:
            transient_seed_workspace = Path(
                tempfile.mkdtemp(prefix="hyphae-g7-search-seeds-")
            )
            seed_root = transient_seed_workspace
        else:
            seed_root = Path(data_root) / "shared-search-seeds"
        environment["HYPHAE_G7_SEARCH_SEED_ROOT"] = str(seed_root)
    receipts = []
    completed_cells: list[dict[str, object]] = []
    matrix_started = time.monotonic()
    started_unix_nanos = time.time_ns()
    background_modes = BACKGROUND_MODES if arguments.background else ("control",)
    total_cells = len(STATES) * len(background_modes) * len(CONCURRENCIES)
    write_matrix_progress(
        progress_path,
        arguments.source_commit,
        arguments.platform,
        completed_cells,
        total_cells,
        None,
        "running",
        started_unix_nanos,
    )
    for state in STATES:
        for background_mode in background_modes:
            for concurrency in CONCURRENCIES:
                current_cell = {
                    "state": state,
                    "background_mode": background_mode,
                    "concurrency": concurrency,
                }
                write_matrix_progress(
                    progress_path,
                    arguments.source_commit,
                    arguments.platform,
                    completed_cells,
                    total_cells,
                    current_cell,
                    "running",
                    started_unix_nanos,
                )
                remaining_seconds = (
                    arguments.matrix_timeout_seconds
                    - (time.monotonic() - matrix_started)
                )
                if remaining_seconds <= 0:
                    write_matrix_progress(
                        progress_path,
                        arguments.source_commit,
                        arguments.platform,
                        completed_cells,
                        total_cells,
                        current_cell,
                        "failed",
                        started_unix_nanos,
                    )
                    raise RuntimeError("G7 matrix exceeded its controller deadline")
                cell_environment = environment.copy()
                if background_mode == "interference":
                    cell_environment["HYPHAE_G7_BACKGROUND"] = "1"
                else:
                    cell_environment.pop("HYPHAE_G7_BACKGROUND", None)
                try:
                    runner_progress_path.unlink(missing_ok=True)
                    receipt = run_cell(
                        binary,
                        arguments.source_commit,
                        arguments.platform,
                        state,
                        concurrency,
                        cell_environment,
                        macos_counter_template,
                        min(float(arguments.cell_timeout_seconds), remaining_seconds),
                        runner_progress_path,
                        float(arguments.stall_timeout_seconds),
                    )
                except BaseException:
                    write_matrix_progress(
                        progress_path,
                        arguments.source_commit,
                        arguments.platform,
                        completed_cells,
                        total_cells,
                        current_cell,
                        "failed",
                        started_unix_nanos,
                    )
                    raise
                try:
                    if not runner_progress_path.is_file():
                        raise RuntimeError("G7 runner did not produce mandatory progress")
                    runner_progress = json.loads(
                        runner_progress_path.read_text(encoding="utf-8")
                    )
                    validate_completed_ann_progress(
                        runner_progress, arguments.source_commit
                    )
                except BaseException:
                    write_matrix_progress(
                        progress_path,
                        arguments.source_commit,
                        arguments.platform,
                        completed_cells,
                        total_cells,
                        current_cell,
                        "failed",
                        started_unix_nanos,
                    )
                    raise
                receipt["background_mode"] = background_mode
                receipt["hardware"] = hardware
                receipt["build"] = build
                validate_initial_ann_bulk_evidence(
                    receipt.get("initial_ann_bulk"), arguments.source_commit
                )
                receipt_cells = receipt.get("cells")
                if not isinstance(receipt_cells, dict):
                    raise RuntimeError("G7 runner omitted its measured cells")
                validate_ann_read_view_cell(
                    receipt_cells.get("ann-top10-recall-095"),
                    receipt["initial_ann_bulk"],
                    receipt["dataset"]["observations"],
                )
                calibration_identity = contract_values[
                    "HYPHAE_G7_HARDWARE_CALIBRATION_FILE"
                ].get("identity")
                if not isinstance(calibration_identity, dict):
                    raise RuntimeError("G7 hardware calibration omitted its identity")
                calibration_executable = calibration_identity.get("executable_blake3")
                if not isinstance(calibration_executable, str):
                    raise RuntimeError("G7 hardware calibration omitted its executable digest")
                validate_execution_authority_evidence(
                    receipt.get("execution_authority"),
                    calibration_executable_blake3=calibration_executable,
                    topology_digest=receipt["initial_ann_bulk"]["topology_digest"],
                    background=background_mode == "interference",
                )
                if (
                    receipt["initial_ann_bulk"]["dataset_digest"]
                    != receipt["dataset"]["digest"]
                ):
                    raise RuntimeError("G7 ANN evidence targets another receipt dataset")
                receipts.append(receipt)
                completed_cells.append(current_cell)
                write_matrix_progress(
                    progress_path,
                    arguments.source_commit,
                    arguments.platform,
                    completed_cells,
                    total_cells,
                    None,
                    "running",
                    started_unix_nanos,
                )
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
        "schema": "hyphae-native-g7-matrix-v3",
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
    if not runner_progress_path.is_file():
        raise RuntimeError("G7 runner progress disappeared before matrix completion")
    runner_progress = json.loads(runner_progress_path.read_text(encoding="utf-8"))
    validate_completed_ann_progress(runner_progress, arguments.source_commit)
    runner_progress_status = runner_progress["status"]
    write_matrix_progress(
        progress_path,
        arguments.source_commit,
        arguments.platform,
        completed_cells,
        total_cells,
        None,
        "completed",
        started_unix_nanos,
    )
    if macos_counter_workspace is not None:
        shutil.rmtree(macos_counter_workspace)
    if transient_seed_workspace is not None:
        shutil.rmtree(transient_seed_workspace)
    print(json.dumps({
        "status": "ok",
        "output": str(arguments.output),
        "cells": len(receipts),
        "progress": str(progress_path),
        "progress_status": "completed",
        "runner_progress": str(runner_progress_path),
        "runner_progress_status": runner_progress_status,
    }))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
