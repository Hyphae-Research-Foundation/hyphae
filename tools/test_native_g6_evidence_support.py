#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import copy
import hashlib
import json
from pathlib import Path

from tools.check_native_g6_manifests import MANIFEST_NAMES

ROOT = Path(__file__).resolve().parents[1]
COMMIT = "a" * 40
FILES = {
    "profile": "native-g6-readiness-profile.json",
    "evidence": "native-g6-readiness-evidence.json",
    "inventory": "native-g6-inventory.json",
    "authority": "native-g6-authority-manifest.json",
    "workload": "native-g6-workload-manifest.json",
    "suite": "native-g6-suite-manifest.json",
    "predecessor": "native-g6-predecessor-manifest.json",
}


def checked_raw() -> dict[str, bytes]:
    return {name: (ROOT / "config" / FILES[name]).read_bytes() for name in MANIFEST_NAMES}


def digests(raw: dict[str, bytes]) -> dict[str, str]:
    return {name: hashlib.sha256(raw[name]).hexdigest() for name in MANIFEST_NAMES}


def payloads(raw: dict[str, bytes]) -> dict[str, dict]:
    return {name: json.loads(raw[name]) for name in MANIFEST_NAMES}


def implemented_raw(requirement: str = "shared-contracts-and-errors") -> dict[str, bytes]:
    documents = payloads(checked_raw())
    inventory_row = next(row for row in documents["inventory"]["requirements"] if row["id"] == requirement)
    suite_row = next(row for row in documents["suite"]["requirements"] if row["id"] == requirement)
    inventory_row["status"] = "implemented-unhosted"
    inventory_row["gaps"] = []
    suite_row["status"] = "implemented-unhosted"
    suite_row["uncovered_acceptance"] = []
    return {name: json.dumps(documents[name], sort_keys=True).encode("utf-8") for name in MANIFEST_NAMES}


def suite_logs(raw: dict[str, bytes], requirement: str = "shared-contracts-and-errors", platform: str = "linux") -> list[tuple[str, bytes]]:
    suite = json.loads(raw["suite"])
    row = next(value for value in suite["requirements"] if value["id"] == requirement)
    logs = []
    for item in row["suites"]:
        if platform not in item.get("platforms", ("linux", "macos", "windows")):
            continue
        command = item.get("platform_commands", {}).get(platform, item["command"])
        if command[0] in {"python", "python3"}:
            result = "Ran 1 test in 0.1s\n\nOK\n"
        elif command[0] in {"node", "npm"}:
            result = "# tests 1\n# pass 1\n# fail 0\n"
        else:
            if "--test" in command:
                target = command[command.index("--test") + 1]
                result = f"Running tests/{target}.rs (target/debug/deps/{target})\n"
            else:
                result = "Running unittests src/lib.rs (target/debug/deps/library)\n"
            result += "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        logs.append(
            (
                item["name"],
                (
                    "G6_COMMAND: "
                    + json.dumps(command, separators=(",", ":"))
                    + "\n"
                    + result
                    + "G6_EXIT_CODE: 0\n"
                ).encode("utf-8"),
            )
        )
    return logs


def copy_payloads(raw: dict[str, bytes]) -> dict[str, dict]:
    return copy.deepcopy(payloads(raw))
