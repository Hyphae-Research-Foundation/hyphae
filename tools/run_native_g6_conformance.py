#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Run the real Native G6 product corpus and emit one platform receipt."""

from __future__ import annotations

import argparse
import json
import os
import platform as host_platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

try:
    from tools.check_native_g6_conformance import (
        G6,
        PLATFORMS,
        REQUIRED_LANES,
        canonical_cross_lane,
        corpus_digest,
        digest,
        schema_digest,
        validate_receipt,
        validate_transcript,
    )
except ModuleNotFoundError:
    from check_native_g6_conformance import (  # type: ignore[no-redef]
        G6,
        PLATFORMS,
        REQUIRED_LANES,
        canonical_cross_lane,
        corpus_digest,
        digest,
        schema_digest,
        validate_receipt,
        validate_transcript,
    )


ROOT = Path(__file__).resolve().parents[1]
RUNNER_MANIFEST = G6 / "runners" / "rust" / "Cargo.toml"


class RunFailure(RuntimeError):
    """A real conformance lane could not be completed."""


def detected_platform() -> str:
    value = host_platform.system().lower()
    if value == "darwin":
        return "macos"
    if value == "windows":
        return "windows"
    if value == "linux":
        return "linux"
    raise RunFailure(f"unsupported host platform: {value}")


def source_commit() -> str:
    completed = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def required_executable(name: str) -> str:
    executable = shutil.which(name)
    if executable is None:
        raise RunFailure(f"required executable is unavailable: {name}")
    return executable


def run_command(command: list[str], environment: dict[str, str], timeout: int = 900) -> str:
    executable = Path(command[0]).name.lower()
    if os.name == "nt" and executable == "npm":
        command = ["npm.cmd", *command[1:]]
    elif os.name == "nt" and executable == "node":
        command = ["node.exe", *command[1:]]
    completed = subprocess.run(
        command,
        cwd=ROOT,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if completed.returncode != 0:
        raise RunFailure(
            f"command failed ({completed.returncode}): {' '.join(command)}\n{completed.stderr}"
        )
    return completed.stdout


def parse_transcript(output: str, lane: str) -> dict[str, Any]:
    values = [line for line in output.splitlines() if line.strip()]
    if len(values) != 1:
        raise RunFailure(f"lane {lane} emitted {len(values)} nonempty stdout lines")
    try:
        value = json.loads(values[0])
    except json.JSONDecodeError as error:
        raise RunFailure(f"lane {lane} emitted invalid JSON: {error}") from error
    return validate_transcript(value, lane)


def build_environment(work: Path, lane: str) -> dict[str, str]:
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    suffix = ".exe" if os.name == "nt" else ""
    environment = {
        **os.environ,
        "HYPHAE_G6_LANE": lane,
        "HYPHAE_G6_WORK": str(work),
        "HYPHAE_G6_CORPUS": str(G6 / "fixtures" / "corpus.json"),
        "HYPHAE_G6_SEED": str(G6 / "fixtures" / "seed.json"),
        "HYPHAE_G6_PRODUCT_BIN": str(target / "debug" / f"hyphae{suffix}"),
        "PYTHONPATH": str(ROOT / "sdks" / "python" / "src"),
    }
    return environment


def lane_command(lane: str) -> list[str]:
    cargo = required_executable("cargo")
    if lane.startswith("python-sdk-"):
        return [
            required_executable("python3" if os.name != "nt" else "python"),
            str(G6 / "runners" / "python" / "run.py"),
        ]
    if lane.startswith("typescript-sdk-"):
        return [
            required_executable("node"),
            str(G6 / "runners" / "typescript" / "run.mjs"),
        ]
    return [
        cargo,
        "run",
        "--quiet",
        "--locked",
        "--manifest-path",
        str(RUNNER_MANIFEST),
        "--",
        "cli-lane" if lane == "cli" else "lane",
        lane,
    ]


def evidence_commands() -> list[list[str]]:
    return [
        ["cargo", "test", "--locked", "-p", "hyphae-native-product", "--test", "integrated_search"],
        ["cargo", "test", "--locked", "-p", "hyphae-native-product", "--test", "native_proof_g6"],
        ["cargo", "test", "--locked", "-p", "hyphae-native-product", "--test", "operation_dispatcher"],
        ["cargo", "test", "--locked", "-p", "hyphae-native-product", "--test", "administration_surfaces"],
        ["cargo", "test", "--locked", "-p", "hyphae-native-daemon", "--test", "daemon_uds"],
        ["cargo", "test", "--locked", "-p", "hyphae-server", "--lib", "native_v2::tests"],
    ]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--platform", choices=PLATFORMS, default=detected_platform())
    parser.add_argument("--source-commit", default=source_commit())
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--keep-work", action="store_true")
    args = parser.parse_args()
    if args.platform != detected_platform():
        raise RunFailure(
            f"requested platform {args.platform} differs from host {detected_platform()}"
        )

    temporary = tempfile.TemporaryDirectory(prefix="hyphae-native-g6-")
    work = Path(temporary.name)
    try:
        environment = build_environment(work, "bootstrap")
        typescript_dist = ROOT / "sdks" / "typescript" / "dist"
        if typescript_dist.exists():
            shutil.rmtree(typescript_dist)
        run_command(
            [required_executable("npm"), "run", "build", "--prefix", "sdks/typescript"],
            environment,
        )
        if not (typescript_dist / "v2" / "index.js").is_file():
            raise RunFailure("TypeScript clean build did not produce dist/v2/index.js")
        run_command(
            [required_executable("cargo"), "build", "--locked", "-p", "hyphae-cli"],
            environment,
        )
        for command in evidence_commands():
            if command[-1] == "daemon_uds":
                command = command[:-1] + (["daemon_windows"] if os.name == "nt" else ["daemon_uds"])
            run_command(command, environment)
        bootstrap = run_command(
            [
                required_executable("cargo"),
                "run",
                "--quiet",
                "--locked",
                "--manifest-path",
                str(RUNNER_MANIFEST),
                "--",
                "bootstrap",
                str(work),
            ],
            environment,
        )
        bootstrap_value = json.loads(bootstrap)
        if bootstrap_value.get("status") != "ready":
            raise RunFailure("Rust bootstrap did not create a verified native backup")

        transcripts: list[dict[str, Any]] = []
        for lane in REQUIRED_LANES:
            environment = build_environment(work, lane)
            lane_data = work / f"lane-{lane}"
            if lane_data.exists():
                shutil.rmtree(lane_data)
            output = run_command(lane_command(lane), environment)
            transcripts.append(parse_transcript(output, lane))

        starts = [transcript["start"] for transcript in transcripts]
        if any(start != starts[0] for start in starts[1:]):
            raise RunFailure("restored lane starting lineage/CSN/catalog/object authority differs")
        try:
            comparable = canonical_cross_lane(transcripts)
        except Exception as error:
            raise RunFailure(str(error)) from error
        receipt = {
            "schema": "hyphae-native-g6-conformance-receipt-v1",
            "source_commit": args.source_commit,
            "platform": args.platform,
            "status": "passed",
            "corpus_digest": corpus_digest(),
            "schema_digest": schema_digest(),
            "transcript_digest": digest(comparable),
            "lanes": transcripts,
        }
        validate_receipt(receipt)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return 0
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError, RunFailure) as error:
        print(f"native G6 conformance run failed: {error}", file=sys.stderr)
        return 2
    finally:
        if args.keep_work:
            print(f"retained G6 work directory: {work}", file=sys.stderr)
            temporary._finalizer.detach()  # type: ignore[attr-defined]
        else:
            temporary.cleanup()


if __name__ == "__main__":
    raise SystemExit(main())
