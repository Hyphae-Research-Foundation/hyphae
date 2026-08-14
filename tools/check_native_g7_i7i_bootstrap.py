#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Fail-closed validation for the ephemeral AWS i7i G7 bootstrap."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


RUNNER_ARCHIVE_SHA256 = (
    "04cf0be1aff4c3ec3554466c39124ca250e3effd8873bb7e8d68535aa9505d5d"
)


class I7iBootstrapError(ValueError):
    """The bootstrap does not establish the dedicated G7 host contract."""


def require_once(source: str, fragment: str, label: str) -> int:
    if source.count(fragment) != 1:
        raise I7iBootstrapError(f"bootstrap must contain exactly one {label}")
    return source.index(fragment)


def validate_i7i_bootstrap(source: str) -> dict[str, object]:
    if not source.startswith("#!/usr/bin/env bash\nset -euo pipefail\n"):
        raise I7iBootstrapError("bootstrap must use fail-closed Bash execution")

    persisted = require_once(
        source,
        "kernel.perf_event_paranoid = -1",
        "persistent perf_event_paranoid setting",
    )
    applied = require_once(
        source,
        "sysctl -w kernel.perf_event_paranoid=-1",
        "sysctl application",
    )
    verified = require_once(
        source,
        'test "$(sysctl -n kernel.perf_event_paranoid)" = "-1"',
        "perf_event_paranoid verification",
    )
    canary = require_once(
        source,
        "sudo -u ubuntu perf stat --no-big-num",
        "unprivileged perf canary",
    )
    registration = require_once(
        source,
        "/opt/actions-runner/config.sh",
        "runner registration",
    )
    if not persisted < applied < verified < canary < registration:
        raise I7iBootstrapError(
            "perf authority must be persisted, applied, verified, and exercised before registration"
        )

    exact_fragments = {
        "runner token placeholder": "__RUNNER_TOKEN__",
        "runner name placeholder": "__RUNNER_NAME__",
    }
    for label, fragment in exact_fragments.items():
        require_once(source, fragment, label)

    required_fragments = {
        "ephemeral runner": "--ephemeral",
        "G7 label": "--labels hyphae-g7,dedicated",
        "instance NVMe model": "AmazonEC2NVMeInstanceStorage",
        "G7 data root": "/mnt/hyphae-g7",
        "hardware evidence": "/etc/hyphae/g7-hardware.json",
        "integer memory source": (
            "ram_kib=\"$(awk '$1 == \"MemTotal:\" {print $2}' /proc/meminfo)\""
        ),
        "integer memory validation": '[[ "$ram_kib" =~ ^[0-9]+$ ]]',
        "integer byte conversion": 'ram_bytes="$((ram_kib * 1024))"',
        "positive memory evidence": "(( ram_bytes > 0 ))",
        "performance governor": "printf '%s\\n' performance",
        "required perf events": "cycles,cache-misses,minor-faults,major-faults",
        "unsupported-event rejection": 'if raw.startswith("<"):',
        "complete perf evidence": (
            'if set(measured) != expected or measured["cycles"] <= 0:'
        ),
        "pinned runner archive": RUNNER_ARCHIVE_SHA256,
        "pinned runner archive digest": "sha256sum --check --strict",
    }
    for label, fragment in required_fragments.items():
        if fragment not in source:
            raise I7iBootstrapError(f"bootstrap must contain {label}")

    return {
        "schema": "hyphae-native-g7-i7i-bootstrap-audit-v1",
        "status": "passed",
        "bootstrap_sha256": hashlib.sha256(source.encode("utf-8")).hexdigest(),
        "perf_event_paranoid": -1,
        "perf_canary_before_registration": True,
        "ephemeral_runner": True,
        "runner_archive_sha256": RUNNER_ARCHIVE_SHA256,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bootstrap", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    try:
        audit = validate_i7i_bootstrap(
            arguments.bootstrap.read_text(encoding="utf-8")
        )
        if arguments.output is not None:
            arguments.output.write_text(
                json.dumps(audit, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
    except (OSError, UnicodeError, I7iBootstrapError) as error:
        print(f"native G7 i7i bootstrap failed: {error}")
        return 1
    print(f"native G7 i7i bootstrap passed: {arguments.bootstrap}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
