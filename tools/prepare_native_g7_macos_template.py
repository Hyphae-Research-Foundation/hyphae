#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Derive the Native G7 L1D counter template from installed Instruments."""

from __future__ import annotations

import argparse
import json
import plistlib
from pathlib import Path


SETTINGS_PREFIX = b'{"allEventsAndFormulas"'
COUNTING_MODE = "l1d_miss_sampling"
DISPLAY_NAME = "L1D Miss Sampling"


def prepare(source: Path, output: Path) -> None:
    archive = plistlib.loads(source.read_bytes())
    objects = archive.get("$objects")
    if not isinstance(objects, list):
        raise ValueError("Instruments template is not an NSKeyedArchiver object list")
    settings_indexes = [
        index
        for index, value in enumerate(objects)
        if isinstance(value, bytes) and value.startswith(SETTINGS_PREFIX)
    ]
    if len(settings_indexes) != 1:
        raise ValueError("Instruments template does not contain one counter setting")
    index = settings_indexes[0]
    settings = json.loads(objects[index].decode("utf-8"))
    if settings.get("selectedCountingMode", {}).get("analysisMode") != "bottleneck":
        raise ValueError("Instruments counter analysis mode changed")
    settings["selectedCountingMode"] = {
        "analysisMode": "bottleneck",
        "countingMode": COUNTING_MODE,
    }
    settings["selectedCountingModeDisplayName"] = DISPLAY_NAME
    objects[index] = json.dumps(
        settings, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(plistlib.dumps(archive, fmt=plistlib.FMT_BINARY, sort_keys=False))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        prepare(arguments.source, arguments.output)
    except (OSError, ValueError, json.JSONDecodeError, plistlib.InvalidFileException) as error:
        print(f"native G7 macOS template failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
