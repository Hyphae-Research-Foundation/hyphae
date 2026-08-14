#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Run optional JavaScript adapters against a live public Hyphae v1 server."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
from pathlib import Path

from run_conformance import executable, free_loopback_port, wait_until_live


ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    target = Path(os.environ.get("HYPHAE_TARGET_DIR", ROOT / "target"))
    suffix = ".exe" if os.name == "nt" else ""
    server_binary = Path(
        os.environ.get("HYPHAE_BIN", target / "debug" / f"hyphae{suffix}")
    )
    if not server_binary.is_file():
        raise RuntimeError(f"Hyphae server executable not found: {server_binary}")

    port = free_loopback_port()
    base_url = f"http://127.0.0.1:{port}"
    with tempfile.TemporaryDirectory(prefix="hyphae-integration-conformance-") as data:
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            process = subprocess.Popen(
                (
                    str(server_binary),
                    "serve",
                    "--data-dir",
                    data,
                    "--bind",
                    f"127.0.0.1:{port}",
                ),
                cwd=ROOT,
                stdout=stdout,
                stderr=stderr,
            )
            try:
                wait_until_live(base_url, process)
                completed = subprocess.run(
                    (
                        executable("node"),
                        str(ROOT / "integrations" / "javascript" / "conformance" / "run.mjs"),
                    ),
                    cwd=ROOT,
                    env={**os.environ, "HYPHAE_BASE_URL": base_url},
                    check=True,
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
                result = json.loads(completed.stdout.strip())
                expected = {
                    "version": 1,
                    "adapters": ["astro", "next", "vite"],
                    "status": "ok",
                }
                if result != expected:
                    raise RuntimeError(f"unexpected adapter result: {result!r}")
                print(json.dumps(result, separators=(",", ":")))
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
