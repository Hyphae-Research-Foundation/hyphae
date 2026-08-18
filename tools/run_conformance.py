#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run format-2 `/v1` clients against the same versioned live fixture."""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class ClientCase:
    name: str
    command: tuple[str, ...]
    environment: dict[str, str]


def executable(name: str) -> str:
    found = shutil.which(name)
    if found is None:
        raise RuntimeError(f"required executable not found: {name}")
    return found


def client_cases() -> list[ClientCase]:
    target = Path(os.environ.get("HYPHAE_TARGET_DIR", ROOT / "target"))
    suffix = ".exe" if os.name == "nt" else ""
    rust_runner = Path(
        os.environ.get(
            "HYPHAE_RUST_CONFORMANCE_BIN",
            target / "debug" / f"hyphae-conformance-rust{suffix}",
        )
    )
    python_path = str(ROOT / "sdks" / "python" / "src")
    cli_binary = Path(
        os.environ.get("HYPHAE_CLI_BIN", target / "debug" / f"hyphae{suffix}")
    )
    return [
        ClientCase("rust", (str(rust_runner),), {}),
        ClientCase(
            "typescript",
            (
                executable("node"),
                str(ROOT / "sdks" / "typescript" / "conformance" / "run.mjs"),
            ),
            {},
        ),
        ClientCase(
            "python",
            (
                executable("python3" if os.name != "nt" else "python"),
                str(ROOT / "sdks" / "python" / "conformance" / "run.py"),
            ),
            {"PYTHONPATH": python_path},
        ),
        ClientCase(
            "cli",
            (
                executable("python3" if os.name != "nt" else "python"),
                str(ROOT / "conformance" / "cli" / "run.py"),
            ),
            {"HYPHAE_CLI_BIN": str(cli_binary)},
        ),
    ]


def free_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def wait_until_live(base_url: str, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Hyphae server exited early with {process.returncode}")
        try:
            with urllib.request.urlopen(
                f"{base_url}/v1/health/live", timeout=0.5
            ) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            time.sleep(0.05)
    raise RuntimeError("Hyphae server did not become live within 10 seconds")


def run_case(server_binary: Path, case: ClientCase) -> dict[str, str]:
    port = free_loopback_port()
    base_url = f"http://127.0.0.1:{port}"
    with tempfile.TemporaryDirectory(prefix=f"hyphae-conformance-{case.name}-") as data:
        with tempfile.TemporaryFile() as server_stdout, tempfile.TemporaryFile() as server_stderr:
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
                stdout=server_stdout,
                stderr=server_stderr,
            )
            try:
                wait_until_live(base_url, process)
                environment = {
                    **os.environ,
                    **case.environment,
                    "HYPHAE_BASE_URL": base_url,
                }
                completed = subprocess.run(
                    case.command,
                    cwd=ROOT,
                    env=environment,
                    check=True,
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
                result = json.loads(completed.stdout.strip())
                if result != {"client": case.name, "status": "ok"}:
                    raise RuntimeError(
                        f"{case.name} emitted unexpected conformance result: {result!r}"
                    )
                return result
            except Exception:
                server_stderr.seek(0)
                diagnostic = server_stderr.read().decode("utf-8", errors="replace")
                if diagnostic:
                    print(diagnostic, file=sys.stderr)
                raise
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


def main() -> int:
    target = Path(os.environ.get("HYPHAE_TARGET_DIR", ROOT / "target"))
    suffix = ".exe" if os.name == "nt" else ""
    server_binary = Path(
        os.environ.get("HYPHAE_BIN", target / "debug" / f"hyphae{suffix}")
    )
    if not server_binary.is_file():
        raise RuntimeError(f"Hyphae server executable not found: {server_binary}")
    results = [run_case(server_binary, case) for case in client_cases()]
    print(json.dumps({"version": 1, "results": results}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
