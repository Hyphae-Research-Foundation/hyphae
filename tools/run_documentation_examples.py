#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Execute the checked-in HTTP documentation examples against one local server."""

from __future__ import annotations

import argparse
import json
import socket
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any


def reserve_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def wait_until_ready(process: subprocess.Popen[str], base_url: str) -> None:
    deadline = time.monotonic() + 15.0
    while time.monotonic() < deadline:
        if process.poll() is not None:
            diagnostic = process.stderr.read() if process.stderr is not None else ""
            raise RuntimeError(f"documented server exited before readiness: {diagnostic}")
        try:
            with urllib.request.urlopen(f"{base_url}/v1/health/ready", timeout=0.5) as response:
                payload = json.load(response)
                if response.status == 200 and payload == {"status": "ready"}:
                    return
        except OSError:
            pass
        time.sleep(0.1)
    raise RuntimeError("documented server did not become ready within 15 seconds")


def remote(binary: Path, root: Path, base_url: str, command: str) -> dict[str, Any]:
    completed = subprocess.run(
        [
            str(binary),
            "remote",
            "--base-url",
            base_url,
            command,
            "--request",
            str(root / f"examples/http/{command}.json"),
        ],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        timeout=30,
    )
    value = json.loads(completed.stdout)
    if not isinstance(value, dict):
        raise RuntimeError(f"documented {command} response is not an object")
    return value


def execute(binary: Path, root: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="hyphae-documentation-") as temporary:
        port = reserve_port()
        base_url = f"http://127.0.0.1:{port}"
        process = subprocess.Popen(
            [
                str(binary),
                "serve",
                "--data-dir",
                str(Path(temporary) / "data"),
                "--bind",
                f"127.0.0.1:{port}",
            ],
            cwd=root,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        try:
            wait_until_ready(process, base_url)
            put = remote(binary, root, base_url, "put")
            get = remote(binary, root, base_url, "get")
            query = remote(binary, root, base_url, "query")
            define_vector_space = remote(binary, root, base_url, "define-vector-space")
            put_vectors = remote(binary, root, base_url, "put-vectors")
            exact = remote(binary, root, base_url, "retrieve-exact")
            define_lexical_index = remote(binary, root, base_url, "define-lexical-index")
            lexical = remote(binary, root, base_url, "retrieve-lexical")
            hybrid = remote(binary, root, base_url, "retrieve-hybrid")
            delete_vectors = remote(binary, root, base_url, "delete-vectors")
            delete = remote(binary, root, base_url, "delete")
            if put.get("status") != "committed":
                raise RuntimeError("documented put did not commit")
            if get.get("found") is not True or not isinstance(get.get("proof"), dict):
                raise RuntimeError("documented get was not a proof-bearing hit")
            if (
                query.get("matched_records") != 2
                or len(query.get("rows", [])) != 2
                or not isinstance(query.get("proof"), dict)
                or not isinstance(query.get("aggregation"), dict)
            ):
                raise RuntimeError("documented query result did not match its explanation")
            if delete.get("status") != "committed":
                raise RuntimeError("documented delete did not commit")
            for name, receipt in (
                ("define-vector-space", define_vector_space),
                ("put-vectors", put_vectors),
                ("define-lexical-index", define_lexical_index),
                ("delete-vectors", delete_vectors),
            ):
                if receipt.get("status") != "committed":
                    raise RuntimeError(f"documented {name} did not commit")
            for name, result in (
                ("exact", exact),
                ("lexical", lexical),
                ("hybrid", hybrid),
            ):
                if (
                    result.get("outcome", {}).get("status") != "matches"
                    or not isinstance(result.get("proof"), dict)
                ):
                    raise RuntimeError(f"documented {name} retrieval was not proof-bearing")
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    binary = arguments.binary.resolve()
    if not binary.is_file():
        parser.error(f"Hyphae binary does not exist: {binary}")
    execute(binary, root)
    print(
        "documentation examples ok: KV/query, vector mutation, "
        "exact/lexical/hybrid retrieval"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
