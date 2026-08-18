#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Generate the native runtime's deterministic third-party license bundle."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from collections import defaultdict
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
POLICY = Path("config/native-dependency-policy.json")
OUTPUT = Path("THIRD_PARTY_LICENSES.txt")
LICENSE_PREFIXES = ("LICENSE", "COPYING", "COPYRIGHT", "NOTICE")


def generate(root: Path = ROOT) -> str:
    policy = json.loads((root / POLICY).read_text(encoding="utf-8"))
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1"],
            cwd=root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=120,
        ).stdout
    )
    packages = {
        (package["name"], package["version"], package["source"]): package
        for package in metadata["packages"]
        if package["source"] is not None
    }
    texts: dict[str, bytes] = {}
    owners: defaultdict[str, list[str]] = defaultdict(list)
    for reviewed in policy["external_packages"]:
        identity = (reviewed["name"], reviewed["version"], reviewed["source"])
        package = packages.get(identity)
        if package is None:
            raise RuntimeError(f"reviewed package is absent from metadata: {identity!r}")
        package_root = Path(package["manifest_path"]).parent
        candidates = sorted(
            path
            for path in package_root.iterdir()
            if path.is_file()
            and path.name.upper().startswith(LICENSE_PREFIXES)
            and path.stat().st_size <= 256 * 1024
        )
        if not candidates:
            owners[f"expression:{reviewed['license']}"].append(
                f"{identity[0]} {identity[1]} ({identity[2]}) [declared SPDX expression; upstream package archive contains no standalone license file]"
            )
            continue
        label = f"{identity[0]} {identity[1]} ({identity[2]})"
        for path in candidates:
            encoded = path.read_bytes().replace(b"\r\n", b"\n").rstrip() + b"\n"
            digest = hashlib.sha256(encoded).hexdigest()
            texts.setdefault(digest, encoded)
            owners[digest].append(f"{label} [{path.name}]")

    lines = [
        "Hyphae native runtime third-party license bundle\n",
        "Generated from config/native-dependency-policy.json and Cargo.lock.\n",
        "Third-party works retain their original terms.\n\n",
    ]
    for digest in sorted(texts):
        lines.append("=" * 78 + "\n")
        lines.append(f"SHA-256: {digest}\n")
        lines.append("Packages:\n")
        lines.extend(f"- {owner}\n" for owner in sorted(owners[digest]))
        lines.append("\n")
        lines.append(texts[digest].decode("utf-8", errors="strict"))
        lines.append("\n")
    expression_keys = sorted(key for key in owners if key.startswith("expression:"))
    if expression_keys:
        lines.append("=" * 78 + "\n")
        lines.append("Packages whose upstream archives contain no standalone license file:\n")
        for key in expression_keys:
            lines.append(f"Declared license: {key.removeprefix('expression:')}\n")
            lines.extend(f"- {owner}\n" for owner in sorted(owners[key]))
        lines.append("\n")
    return "".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--output", type=Path, default=OUTPUT)
    arguments = parser.parse_args()
    output = arguments.output if arguments.output.is_absolute() else ROOT / arguments.output
    try:
        generated = generate()
    except (OSError, UnicodeError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    if arguments.check:
        if not output.is_file() or output.read_text(encoding="utf-8") != generated:
            print(f"error: {output}: third-party license bundle is stale", file=sys.stderr)
            return 1
    else:
        output.write_text(generated, encoding="utf-8", newline="\n")
    print("third-party license bundle passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
