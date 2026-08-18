#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Generate the canonical disk-format-1 compatibility fixture."""

from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
from pathlib import Path


TRANSACTION_ID = "018f0000-0000-7000-8000-00000000f001"
VALUE = '{"group":"fixture","score":42}'


def run(binary: Path, *arguments: str) -> dict[str, object]:
    completed = subprocess.run(
        [str(binary), *arguments],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return json.loads(completed.stdout)


def generate(binary: Path) -> dict[str, object]:
    with tempfile.TemporaryDirectory(prefix="hyphae-compat-v1-") as temporary:
        data = Path(temporary) / "data"
        committed = run(
            binary,
            "put",
            "--data-dir",
            str(data),
            "--key",
            "alpha",
            "--json",
            VALUE,
            "--transaction-id",
            TRANSACTION_ID,
        )
        compacted = run(binary, "compact", "--data-dir", str(data))
        if committed.get("status") != "committed":
            raise RuntimeError("fixture transaction was not newly committed")
        if compacted.get("status") != "compacted":
            raise RuntimeError("fixture data directory was not compacted")
        if (data / "FORMAT").read_text(encoding="ascii") != "hyphae-disk-format=1\n":
            raise RuntimeError(
                "fixture generation requires a historical binary that emits disk format 1"
            )

        manifests = sorted((data / "manifest").glob("*.hymanifest"))
        if not manifests:
            raise RuntimeError("fixture has no storage manifest")
        manifest = manifests[-1]
        encoded_manifest = manifest.read_bytes()
        if len(encoded_manifest) != 140:
            raise RuntimeError("fixture manifest has an unexpected length")
        active_segment = int.from_bytes(encoded_manifest[24:32], "little")
        base_sequence = int.from_bytes(encoded_manifest[32:40], "little")
        selected = [
            data / "FORMAT",
            manifest,
            data / "log" / f"{active_segment:020}.hylog",
            data / "snapshots" / f"snapshot-{base_sequence:020}.hysnap",
        ]
        files = {
            path.relative_to(data).as_posix(): path.read_bytes().hex()
            for path in selected
        }
        return {
            "disk_format_version": 1,
            "expected": {
                "commit_digest": committed["commit_digest"],
                "commit_sequence": committed["commit_sequence"],
                "key_hex": "616c706861",
                "transaction_digest": committed["transaction_digest"],
                "transaction_id": TRANSACTION_ID,
                "value": {"group": "fixture", "score": 42},
            },
            "files_hex": files,
            "fixture_version": 1,
            "purpose": "Open and recover a compacted disk-format-1 directory without its materialized index.",
        }


def verify(binary: Path, fixture_path: Path) -> None:
    checked_in = fixture_path.read_text(encoding="utf-8")
    fixture = json.loads(checked_in)
    canonical = json.dumps(fixture, indent=2, sort_keys=True) + "\n"
    if checked_in != canonical:
        raise SystemExit(f"compatibility fixture is not canonical JSON: {fixture_path}")
    if fixture.get("disk_format_version") != 1 or fixture.get("fixture_version") != 1:
        raise SystemExit(f"compatibility fixture has an unexpected version: {fixture_path}")

    files = fixture.get("files_hex")
    expected = fixture.get("expected")
    if not isinstance(files, dict) or not isinstance(expected, dict):
        raise SystemExit(f"compatibility fixture is malformed: {fixture_path}")
    if files.get("FORMAT") != b"hyphae-disk-format=1\n".hex():
        raise SystemExit(f"compatibility fixture FORMAT marker is not version 1: {fixture_path}")

    with tempfile.TemporaryDirectory(prefix="hyphae-compat-v1-check-") as temporary:
        data = Path(temporary) / "data"
        for relative, encoded in files.items():
            if not isinstance(relative, str) or not isinstance(encoded, str):
                raise SystemExit(f"compatibility fixture file entry is malformed: {fixture_path}")
            destination = data / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(bytes.fromhex(encoded))

        loaded = run(binary, "get", "--data-dir", str(data), "--key", "alpha")
        record = loaded.get("record")
        if loaded.get("found") is not True or not isinstance(record, dict):
            raise SystemExit(f"compatibility fixture record could not be read: {fixture_path}")
        if record.get("key_hex") != expected.get("key_hex"):
            raise SystemExit(f"compatibility fixture key changed: {fixture_path}")
        if record.get("value") != expected.get("value"):
            raise SystemExit(f"compatibility fixture value changed: {fixture_path}")

        replayed = run(
            binary,
            "put",
            "--data-dir",
            str(data),
            "--key",
            "alpha",
            "--json",
            VALUE,
            "--transaction-id",
            TRANSACTION_ID,
        )
        if replayed.get("status") != "existing":
            raise SystemExit(
                f"compatibility fixture idempotency receipt was not replayed: {fixture_path}"
            )
        for field in (
            "commit_digest",
            "commit_sequence",
            "transaction_digest",
            "transaction_id",
        ):
            if replayed.get(field) != expected.get(field):
                raise SystemExit(
                    f"compatibility fixture {field} changed during replay: {fixture_path}"
                )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--check", type=Path)
    arguments = parser.parse_args()
    if arguments.check is not None:
        verify(arguments.binary, arguments.check)
        return
    generated = json.dumps(generate(arguments.binary), indent=2, sort_keys=True) + "\n"
    print(generated, end="")


if __name__ == "__main__":
    main()
