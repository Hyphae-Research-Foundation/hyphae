#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fail when optional integrations cross Hyphae's public boundary."""

from __future__ import annotations

import json
import re
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
INTEGRATIONS = ROOT / "integrations"
PRIVATE_RUST_CRATES = {
    "hyphae-core",
    "hyphae-engine",
    "hyphae-query",
    "hyphae-retrieval",
    "hyphae-server",
    "hyphae-storage",
}


def main() -> int:
    errors: list[str] = []
    cargo_path = INTEGRATIONS / "pliegors" / "Cargo.toml"
    cargo = tomllib.loads(cargo_path.read_text("utf-8"))
    dependencies = set(cargo.get("dependencies", {}))
    forbidden = sorted(dependencies & PRIVATE_RUST_CRATES)
    if forbidden:
        errors.append(f"PliegoRS adapter imports private Rust crates: {forbidden}")
    if "hyphae-client" not in dependencies:
        errors.append("PliegoRS adapter must consume hyphae-client")

    javascript_path = INTEGRATIONS / "javascript" / "package.json"
    if javascript_path.is_file():
        package = json.loads(javascript_path.read_text("utf-8"))
        peers = package.get("peerDependencies", {})
        metadata = package.get("peerDependenciesMeta", {})
        if "@hyphae_/hyphae" not in peers:
            errors.append("JavaScript integrations must peer-depend on @hyphae_/hyphae")
        for host in ("astro", "next", "vite"):
            if host not in peers or metadata.get(host, {}).get("optional") is not True:
                errors.append(f"{host} must be an optional peer dependency")

    host_smoke = INTEGRATIONS / "host-smoke"
    for relative in (
        "package.json",
        "package-lock.json",
        "fixtures/astro/astro.config.mjs",
        "fixtures/astro/src/pages/index.astro",
        "fixtures/next/app/layout.js",
        "fixtures/next/app/page.js",
        "fixtures/vite/index.html",
        "fixtures/vite/src/main.js",
    ):
        path = host_smoke / relative
        if not path.is_file():
            errors.append(f"missing framework host smoke fixture: {relative}")
            continue
        if "hyphae" in path.read_text("utf-8").lower():
            errors.append(f"framework host smoke fixture depends on Hyphae: {relative}")

    forbidden_source = re.compile(
        r"hyphae-(?:core|engine|query|retrieval|server|storage)|"
        r"@hyphae_/hyphae/(?:src|internal)|(?:\.\./){2,}crates/"
    )
    for suffix in ("*.rs", "*.ts", "*.js", "*.mjs", "*.json"):
        for path in INTEGRATIONS.rglob(suffix):
            if "node_modules" in path.parts or path.name == "package-lock.json":
                continue
            match = forbidden_source.search(path.read_text("utf-8"))
            if match:
                errors.append(
                    f"private integration reference in {path.relative_to(ROOT)}: {match.group(0)}"
                )

    if errors:
        raise SystemExit("\n".join(errors))
    print("integration-boundaries-ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
