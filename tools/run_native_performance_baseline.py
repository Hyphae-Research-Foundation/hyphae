#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Produce one P0 diagnostic Native performance baseline."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import subprocess
import sys
import tempfile
import time
from pathlib import Path

if os.name == "posix":
    import resource

try:
    from tools.check_native_performance_receipt import (
        profile_digest,
        suite_profile_digest,
        validate_receipt,
        validate_suite,
    )
except ModuleNotFoundError:
    from check_native_performance_receipt import (
        profile_digest,
        suite_profile_digest,
        validate_receipt,
        validate_suite,
    )


ROOT = Path(__file__).resolve().parents[1]
EXAMPLE = "performance_baseline"


def _run_git(*arguments: str, environment: dict[str, str] | None = None) -> str:
    return subprocess.run(
        ("git", *arguments),
        cwd=ROOT,
        env=environment,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def worktree_identity(expected_commit: str) -> tuple[str, bool]:
    head = _run_git("rev-parse", "HEAD")
    if head != expected_commit:
        raise RuntimeError("performance baseline source commit differs from HEAD")
    clean = not bool(_run_git("status", "--porcelain"))
    if clean:
        return _run_git("rev-parse", "HEAD^{tree}"), True
    with tempfile.TemporaryDirectory(prefix="hyphae-performance-index-") as directory:
        index = Path(directory) / "index"
        environment = os.environ.copy()
        environment["GIT_INDEX_FILE"] = str(index)
        _run_git("read-tree", "HEAD", environment=environment)
        _run_git("add", "--all", environment=environment)
        return _run_git("write-tree", environment=environment), False


def build_metadata(binary: Path) -> dict[str, str]:
    rustc = subprocess.run(
        ("rustc", "-vV"),
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    ).stdout.strip()
    target = next(
        (
            line.removeprefix("host: ")
            for line in rustc.splitlines()
            if line.startswith("host: ")
        ),
        "",
    )
    if not target:
        raise RuntimeError("rustc did not disclose its host target")
    return {
        "target": target,
        "compiler": rustc,
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
    }


def hardware_identity() -> tuple[str, str]:
    topology = (
        f"logical_cpus={os.cpu_count() or 0};machine={platform.machine()};"
        f"processor={platform.processor() or 'undisclosed'}"
    )
    fingerprint_source = {
        "machine": platform.machine(),
        "processor": platform.processor(),
        "logical_cpus": os.cpu_count(),
        "platform": platform.platform(),
    }
    encoded = json.dumps(
        fingerprint_source,
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return topology, hashlib.sha256(encoded).hexdigest()


def unsupported_counter(unit: str, reason: str) -> dict:
    return {
        "status": "unsupported",
        "value": None,
        "unit": unit,
        "provider": "none",
        "reason": reason,
    }


def measured_counter(unit: str, value: int, provider: str) -> dict:
    return {
        "status": "measured",
        "value": max(0, value),
        "unit": unit,
        "provider": provider,
        "reason": None,
    }


class ProcessMetrics:
    def __init__(self, process_id: int) -> None:
        self.process_id = process_id
        self.peak_rss = 0
        self.initial_io: dict[str, int] | None = None
        self.final_io: dict[str, int] | None = None

    def sample(self) -> None:
        if sys.platform.startswith("linux"):
            status = Path(f"/proc/{self.process_id}/status")
            if status.is_file():
                for line in status.read_text(encoding="ascii", errors="ignore").splitlines():
                    if line.startswith("VmHWM:"):
                        self.peak_rss = max(self.peak_rss, int(line.split()[1]) * 1_024)
            io_path = Path(f"/proc/{self.process_id}/io")
            if io_path.is_file():
                values = {}
                for line in io_path.read_text(encoding="ascii", errors="ignore").splitlines():
                    name, _, value = line.partition(":")
                    if name in {"read_bytes", "write_bytes"}:
                        values[name] = int(value.strip())
                if self.initial_io is None:
                    self.initial_io = values
                self.final_io = values

    def io_counter(self, source: str) -> dict:
        if (
            self.initial_io is not None
            and self.final_io is not None
            and source in self.initial_io
            and source in self.final_io
        ):
            return measured_counter(
                "bytes",
                self.final_io[source] - self.initial_io[source],
                "linux-proc-io",
            )
        return unsupported_counter("bytes", "process I/O counters are unavailable")


def run_sample(
    binary: Path,
    source_commit: str,
    observations: int,
    warmup: int,
) -> tuple[dict, dict[str, dict]]:
    before = resource.getrusage(resource.RUSAGE_CHILDREN) if os.name == "posix" else None
    process = subprocess.Popen(
        (str(binary), source_commit, str(observations), str(warmup)),
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    metrics = ProcessMetrics(process.pid)
    while process.poll() is None:
        metrics.sample()
        time.sleep(0.005)
    stdout, stderr = process.communicate()
    metrics.sample()
    if process.returncode != 0:
        raise RuntimeError(f"performance baseline failed: {stderr.strip()}")
    counters = {
        "cpu_time": unsupported_counter("nanoseconds", "process CPU time is unavailable"),
        "cpu_cycles": unsupported_counter("cycles", "hardware counter provider was not attached"),
        "instructions": unsupported_counter("count", "hardware counter provider was not attached"),
        "cache_misses": unsupported_counter("count", "hardware counter provider was not attached"),
        "context_switches": unsupported_counter("count", "process context switches are unavailable"),
        "page_faults": unsupported_counter("count", "process page faults are unavailable"),
        "allocations": unsupported_counter("count", "allocator instrumentation was not attached"),
        "peak_rss": (
            measured_counter("bytes", metrics.peak_rss, "process-rss-sampler")
            if metrics.peak_rss > 0
            else unsupported_counter("bytes", "process RSS is unavailable")
        ),
        "bytes_read": metrics.io_counter("read_bytes"),
        "bytes_written": metrics.io_counter("write_bytes"),
    }
    if before is not None:
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        cpu_nanos = int(
            ((after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime))
            * 1_000_000_000
        )
        counters["cpu_time"] = measured_counter("nanoseconds", cpu_nanos, "getrusage-children")
        counters["context_switches"] = measured_counter(
            "count",
            int(
                after.ru_nvcsw
                - before.ru_nvcsw
                + after.ru_nivcsw
                - before.ru_nivcsw
            ),
            "getrusage-children",
        )
        counters["page_faults"] = measured_counter(
            "count",
            int(
                after.ru_minflt
                - before.ru_minflt
                + after.ru_majflt
                - before.ru_majflt
            ),
            "getrusage-children",
        )
    return json.loads(stdout), counters


def assemble_receipt(
    sample: dict,
    counters: dict[str, dict],
    source_tree: str,
    clean: bool,
    build: dict[str, str],
) -> dict:
    if sample.get("schema") != "hyphae-native-performance-sample-v1":
        raise RuntimeError("performance baseline sample schema is invalid")
    workload = dict(sample["workload"])
    parameters = workload.pop("parameters")
    parameters_sha256 = hashlib.sha256(
        json.dumps(parameters, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    measurement = dict(sample["measurement"])
    elapsed = measurement["elapsed_nanos"]
    engine = measurement.pop("engine_execution_nanos")
    if not isinstance(elapsed, int) or not isinstance(engine, int) or engine > elapsed:
        raise RuntimeError("performance baseline sample clocks are invalid")
    topology, hardware_fingerprint = hardware_identity()
    return {
        "schema": "hyphae-native-performance-receipt-v1",
        "status": "passed",
        "evidence_class": "diagnostic-baseline",
        "source": {
            "commit": sample["source_commit"],
            "tree": source_tree,
            "binary_sha256": build["binary_sha256"],
            "profile_sha256": profile_digest(),
            "clean": clean,
        },
        "environment": {
            "platform": sys.platform,
            "target": build["target"],
            "os": platform.platform(),
            "compiler": build["compiler"],
            "build_profile": "release",
            "hardware_fingerprint": hardware_fingerprint,
            "dedicated": False,
            "virtualization": "unknown",
            "topology": topology,
            "affinity": "uncontrolled",
        },
        "workload": {
            **workload,
            "parameters_sha256": parameters_sha256,
        },
        "dataset": {
            "source_commit": sample["source_commit"],
            **sample["dataset"],
        },
        "measurement": {
            **measurement,
            "clock_totals_nanos": {
                "admission": 0,
                "queueing": 0,
                "parse_bind_plan": 0,
                "engine_execution": engine,
                "cross_engine_fusion": 0,
                "wal_append": 0,
                "physical_synchronization": 0,
                "transport": 0,
                "result_proof_encoding": 0,
                "unattributed": elapsed - engine,
            },
        },
        "counters": counters,
        "correctness": sample["correctness"],
        "claims": [],
        "closure_declared": False,
    }


def assemble_suite(receipt: dict) -> dict:
    source = receipt["source"]
    environment = receipt["environment"]
    return {
        "schema": "hyphae-native-performance-suite-v1",
        "status": "passed",
        "suite_profile_sha256": suite_profile_digest(),
        "source_commit": source["commit"],
        "source_tree": source["tree"],
        "binary_sha256": source["binary_sha256"],
        "clean": source["clean"],
        "hardware_fingerprint": environment["hardware_fingerprint"],
        "receipts": [receipt],
        "claims": [],
        "closure_declared": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--audit-output", type=Path, required=True)
    parser.add_argument("--suite-output", type=Path)
    parser.add_argument("--suite-audit-output", type=Path)
    parser.add_argument("--observations", type=int, default=100_000)
    parser.add_argument("--warmup", type=int, default=10_000)
    parser.add_argument("--skip-build", action="store_true")
    arguments = parser.parse_args()
    if arguments.observations <= 0 or arguments.warmup <= 0:
        raise RuntimeError("performance baseline observation counts must be positive")
    source_tree, clean = worktree_identity(arguments.source_commit)
    if not arguments.skip_build:
        subprocess.run(
            (
                "cargo",
                "build",
                "--locked",
                "--release",
                "-p",
                "hyphae-native-runtime",
                "--example",
                EXAMPLE,
            ),
            cwd=ROOT,
            check=True,
        )
    binary = ROOT / "target" / "release" / "examples" / EXAMPLE
    if os.name == "nt":
        binary = binary.with_suffix(".exe")
    if not binary.is_file():
        raise RuntimeError(f"performance baseline binary is missing: {binary}")
    sample, counters = run_sample(
        binary,
        arguments.source_commit,
        arguments.observations,
        arguments.warmup,
    )
    receipt = assemble_receipt(
        sample,
        counters,
        source_tree,
        clean,
        build_metadata(binary),
    )
    audit = validate_receipt(receipt, arguments.source_commit)
    suite = assemble_suite(receipt)
    suite_audit = validate_suite(suite, arguments.source_commit)
    suite_output = arguments.suite_output or arguments.output.with_name(
        f"{arguments.output.stem}.suite.json"
    )
    suite_audit_output = arguments.suite_audit_output or arguments.audit_output.with_name(
        f"{arguments.audit_output.stem}.suite.json"
    )
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.audit_output.parent.mkdir(parents=True, exist_ok=True)
    suite_output.parent.mkdir(parents=True, exist_ok=True)
    suite_audit_output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    arguments.audit_output.write_text(json.dumps(audit, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    suite_output.write_text(json.dumps(suite, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    suite_audit_output.write_text(
        json.dumps(suite_audit, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(
        json.dumps(
            {
                "status": "ok",
                "output": str(arguments.output),
                "audit": str(arguments.audit_output),
                "suite_output": str(suite_output),
                "suite_audit": str(suite_audit_output),
                "clean_source": clean,
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
