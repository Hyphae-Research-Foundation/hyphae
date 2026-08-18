#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Run every authorized G6 suite and emit one open receipt per requirement."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform as host_platform
import subprocess
import sys
from pathlib import Path

from tools.check_native_g6_manifests import MANIFEST_NAMES


ROOT = Path(__file__).resolve().parents[1]
FILES = {
    "profile": "native-g6-readiness-profile.json",
    "evidence": "native-g6-readiness-evidence.json",
    "inventory": "native-g6-inventory.json",
    "authority": "native-g6-authority-manifest.json",
    "workload": "native-g6-workload-manifest.json",
    "suite": "native-g6-suite-manifest.json",
    "predecessor": "native-g6-predecessor-manifest.json",
}


def host_command(command: list[str]) -> list[str]:
    if os.name == "nt" and command[0] == "npm":
        return ["npm.cmd", *command[1:]]
    if os.name == "nt" and command[0] == "node":
        return ["node.exe", *command[1:]]
    return command


def version(command: list[str]) -> str:
    completed = subprocess.run(
        host_command(command),
        check=True,
        capture_output=True,
        text=True,
    )
    return (completed.stdout or completed.stderr).strip()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--platform", choices=("linux", "macos", "windows"), required=True)
    parser.add_argument("--python-command", choices=("python", "python3"), required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()

    paths = {name: ROOT / "config" / FILES[name] for name in MANIFEST_NAMES}
    raw = {name: path.read_bytes() for name, path in paths.items()}
    digests = {name: hashlib.sha256(raw[name]).hexdigest() for name in MANIFEST_NAMES}
    suite = json.loads(raw["suite"])
    output = args.output_dir
    logs = output / "logs"
    receipts = output / "receipts"
    audits = output / "audits"
    for directory in (logs, receipts, audits):
        directory.mkdir(parents=True, exist_ok=True)

    tools = {
        "cargo": version(["cargo", "--version"]),
        args.python_command: version([args.python_command, "--version"]),
        "npm": version(["npm", "--version"]),
        "node": version(["node", "--version"]),
    }
    environment = dict(os.environ)
    environment["PYTHONPATH"] = str(ROOT / "sdks" / "python" / "src") + os.pathsep + str(ROOT)
    platform_requirements: list[dict[str, object]] = []

    typescript_dist = ROOT / "sdks" / "typescript" / "dist"
    if typescript_dist.exists():
        import shutil
        shutil.rmtree(typescript_dist)
    subprocess.run(host_command(["npm", "run", "build", "--prefix", "sdks/typescript"]), cwd=ROOT, env=environment, check=True)
    if not (typescript_dist / "v2" / "index.js").is_file():
        raise RuntimeError("TypeScript clean build did not produce dist/v2/index.js")

    conformance_receipt = output / "native-g6-conformance-receipt.json"
    subprocess.run(
        [sys.executable, str(ROOT / "tools" / "run_native_g6_conformance.py"), "--platform", args.platform, "--source-commit", args.source_commit, "--output", str(conformance_receipt)],
        cwd=ROOT,
        env=environment,
        check=True,
    )
    subprocess.run(
        [sys.executable, str(ROOT / "tools" / "check_native_g6_conformance.py"), "receipt", "--receipt", str(conformance_receipt), "--output", str(output / "native-g6-conformance-audit.json")],
        cwd=ROOT,
        env=environment,
        check=True,
    )

    for requirement in suite["requirements"]:
        receipt_args: list[str] = []
        for item in requirement["suites"]:
            if args.platform not in item.get("platforms", ("linux", "macos", "windows")):
                continue
            authorized = item.get("platform_commands", {}).get(args.platform, item["command"])
            command = [args.python_command if part == "python" else part for part in authorized]
            log = logs / f"{requirement['id']}--{item['name']}.log"
            with log.open("w", encoding="utf-8", newline="\n") as stream:
                stream.write("G6_COMMAND: " + json.dumps(authorized, separators=(",", ":")) + "\n")
                stream.flush()
                completed = subprocess.run(host_command(command), cwd=ROOT, env=environment, stdout=stream, stderr=subprocess.STDOUT, check=False)
                stream.write(f"G6_EXIT_CODE: {completed.returncode}\n")
            if completed.returncode != 0:
                print(log.read_text(encoding="utf-8"), file=sys.stderr, end="")
                return completed.returncode
            receipt_args += ["--suite-log", item["name"], str(log)]

        command = [
            sys.executable, str(ROOT / "tools" / "produce_native_g6_receipt.py"),
            "--source-commit", args.source_commit, "--requirement", requirement["id"],
            "--platform", args.platform,
        ]
        for name in MANIFEST_NAMES:
            command += [f"--{name}", str(paths[name]), f"--{name}-sha256", digests[name]]
        executables = {
            item.get("platform_commands", {}).get(args.platform, item["command"])[0]
            for item in requirement["suites"]
            if args.platform in item.get("platforms", ("linux", "macos", "windows"))
        }
        for executable in sorted(executables):
            actual = args.python_command if executable == "python" else executable
            command += ["--tool-version", executable, tools[actual]]
        receipt = receipts / f"{requirement['id']}.json"
        command += receipt_args + ["--output", str(receipt)]
        subprocess.run(command, cwd=ROOT, env=environment, check=True)

        audit = audits / f"{requirement['id']}.json"
        command = [
            sys.executable, str(ROOT / "tools" / "check_native_g6_receipt.py"),
            "--receipt", str(receipt), "--expected-commit", args.source_commit,
            "--expected-requirement", requirement["id"],
            "--authority-manifest", str(paths["authority"]),
            "--workload-manifest", str(paths["workload"]),
            "--suite-manifest", str(paths["suite"]),
            "--inventory-manifest", str(paths["inventory"]),
        ]
        for name in MANIFEST_NAMES:
            command += [f"--{name}-sha256", digests[name]]
        command += ["--output", str(audit)]
        subprocess.run(command, cwd=ROOT, env=environment, check=True)
        platform_requirements.append(
            {
                "id": requirement["id"],
                "implementation_status": requirement["status"],
                "uncovered_acceptance": requirement["uncovered_acceptance"],
                "receipt_sha256": hashlib.sha256(receipt.read_bytes()).hexdigest(),
                "audit_sha256": hashlib.sha256(audit.read_bytes()).hexdigest(),
            }
        )

    summary = {
        "schema": "hyphae-native-g6-platform-candidate-v1",
        "gate": "G6",
        "status": "passed",
        "evidence_class": "supporting-not-closure",
        "source_commit": args.source_commit,
        "platform": args.platform,
        "host": host_platform.platform(),
        "manifest_sha256": digests,
        "conformance_receipt_sha256": hashlib.sha256(conformance_receipt.read_bytes()).hexdigest(),
        "conformance_audit_sha256": hashlib.sha256((output / "native-g6-conformance-audit.json").read_bytes()).hexdigest(),
        "requirements": len(suite["requirements"]),
        "requirement_receipts": platform_requirements,
        "claims": [],
        "closure_declared": False,
    }
    (output / "platform-summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
