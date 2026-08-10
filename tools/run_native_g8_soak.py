#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only

"""Bounded all-engine Native soak, crash/reopen, backup, and restore gate."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
MINIMUM_CYCLES = 4
MINIMUM_WRITES_PER_CYCLE = 32


def run_json(binary: Path, *arguments: str, timeout: int = 120) -> dict[str, Any]:
    completed = subprocess.run(
        (str(binary), *arguments),
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"Hyphae command failed ({' '.join(arguments)}): {completed.stderr.strip()}"
        )
    value = json.loads(completed.stdout)
    if not isinstance(value, dict):
        raise RuntimeError("Hyphae command did not emit one JSON object")
    return value


def free_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def kill_and_reopen(binary: Path, data: Path) -> None:
    port = free_port()
    endpoint = (
        f"hyphae-g8-{os.getpid()}-{port}"
        if os.name == "nt"
        else str(Path(tempfile.gettempdir()) / f"h-g8-{os.getpid()}-{port}.sock")
    )
    process = subprocess.Popen(
        (
            str(binary),
            "serve",
            "--data-dir",
            str(data),
            "--http-bind",
            f"127.0.0.1:{port}",
            "--endpoint",
            endpoint,
        ),
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    deadline = time.monotonic() + 10
    try:
        while time.monotonic() < deadline:
            if process.poll() is not None:
                stderr = process.stderr.read().decode("utf-8", errors="replace")
                raise RuntimeError(f"Native daemon exited before kill: {stderr}")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                    break
            except OSError:
                time.sleep(0.02)
        else:
            raise RuntimeError("Native daemon did not become reachable")
        process.kill()
        process.wait(timeout=5)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        if os.name != "nt":
            Path(endpoint).unlink(missing_ok=True)
    doctor = run_json(binary, "doctor", "--data-dir", str(data))
    if doctor.get("status") != "healthy":
        raise RuntimeError("Native directory was unhealthy after forced daemon termination")


def create_surfaces(binary: Path, data: Path) -> None:
    run_json(binary, "init", "--data-dir", str(data))
    run_json(
        binary,
        "sql",
        "--data-dir",
        str(data),
        "execute",
        "--statement",
        "CREATE TABLE soak_items (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
    )
    run_json(
        binary,
        "catalog",
        "--data-dir",
        str(data),
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.soak",
    )
    run_json(
        binary,
        "catalog",
        "--data-dir",
        str(data),
        "create-keyspace",
        "--id",
        "20",
        "--parent",
        "11",
        "--name",
        "main.public.soak_values",
        "--family",
        "string",
    )
    run_json(
        binary,
        "search",
        "--data-dir",
        str(data),
        "provision",
        "--collection",
        "13",
    )


def write_cycle(binary: Path, data: Path, cycle: int, writes: int) -> int:
    transaction_steps = []
    documents = []
    for offset in range(writes):
        identifier = cycle * writes + offset + 1
        transaction_steps.extend([
            {
                "operation": "stage_sql",
                "statement": "INSERT INTO soak_items (id, body) VALUES (?, ?)",
                "parameters": [identifier, f"durable native item {identifier}"],
            },
            {
                "operation": "stage_structure",
                "mutation": {
                    "operation": "string_set",
                    "keyspace": 20,
                    "key": f"soak-{identifier}",
                    "value": f"value-{identifier}",
                    "expires_at_micros": None,
                },
            },
        ])
        documents.append({
            "id": 1_000_000 + identifier,
            "text": f"durable native search soakid{identifier}x",
            "doc_values": {"category": f"cycle-{cycle}", "price": identifier},
            "vectors": {
                "exact": [1.0, float(identifier)],
                "ann": [1.0, float(identifier)],
            },
        })
    transaction_steps.append({"operation": "commit"})
    transaction = run_json(
        binary, "transaction", "--data-dir", str(data), "execute",
        "--steps-json", json.dumps(transaction_steps, separators=(",", ":")),
    )
    transaction_results = transaction.get("steps")
    if (
        not isinstance(transaction_results, list)
        or not transaction_results
        or not isinstance(transaction_results[-1], dict)
        or transaction_results[-1].get("status") != "committed"
    ):
        raise RuntimeError("SQL/structure soak transaction did not commit")
    run_json(
        binary, "search", "--data-dir", str(data), "ingest",
        "--collection", "13", "--idempotency-id", str(cycle + 1),
        "--documents-json", json.dumps(documents, separators=(",", ":")),
    )
    run_json(binary, "checkpoint", "--data-dir", str(data))
    return cycle * writes + writes


def state_digest(binary: Path, data: Path, final_identifier: int) -> tuple[str, str, int]:
    sql = run_json(
        binary,
        "sql",
        "--data-dir",
        str(data),
        "execute",
        "--statement",
        f"SELECT id, body FROM soak_items ORDER BY id LIMIT {final_identifier}",
    )
    rows = sql.get("result", {}).get("rows", [])
    expected_rows = [
        [identifier, f"durable native item {identifier}"]
        for identifier in range(1, final_identifier + 1)
    ]
    if rows != expected_rows:
        raise RuntimeError("SQL state does not contain the complete expected corpus")
    status = run_json(binary, "status", "--data-dir", str(data))
    snapshot = status.get("snapshot", {})
    root_digest = snapshot.get("root_digest")
    if (
        status.get("status") != "ready"
        or not isinstance(root_digest, str)
        or len(root_digest) != 64
        or any(character not in "0123456789abcdef" for character in root_digest)
        or not isinstance(snapshot.get("visible_csn"), int)
        or snapshot["visible_csn"] <= 0
        or not isinstance(snapshot.get("catalog_version"), int)
        or snapshot["catalog_version"] <= 0
    ):
        raise RuntimeError("Native all-engine root identity is incomplete")
    sample_identifiers = sorted({1, final_identifier // 2, final_identifier})
    structures = []
    search_hits = []
    for identifier in sample_identifiers:
        value = structure_value(binary, data, identifier)
        if value != f"value-{identifier}":
            raise RuntimeError(f"structure state differs for record {identifier}")
        structures.append([f"soak-{identifier}", value])
        expected_object = str(1_000_000 + identifier)
        if expected_object not in search_hit_ids(binary, data, identifier):
            raise RuntimeError(f"search state differs for record {identifier}")
        search_hits.append([identifier, expected_object])
    canonical = json.dumps(
        {
            "catalog_version": snapshot["catalog_version"],
            "root_digest": root_digest,
            "search_samples": search_hits,
            "sql": rows,
            "structure_samples": structures,
            "visible_csn": snapshot["visible_csn"],
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(canonical).hexdigest(), root_digest, len(sample_identifiers)


def structure_value(binary: Path, data: Path, identifier: int) -> Any:
    response = run_json(
        binary, "structure", "--data-dir", str(data), "read",
        "--request-json", json.dumps({
            "operation": "string_get", "keyspace": 20,
            "key": f"soak-{identifier}",
        }, separators=(",", ":")),
    )
    return response.get("result", {}).get("value")


def search_hit_ids(binary: Path, data: Path, identifier: int) -> list[str]:
    response = run_json(
        binary, "search", "--data-dir", str(data), "integrated",
        "--collection", "13", "--lexical", f"soakid{identifier}x",
        "--vector-target", "exact", "--vector", "1",
        "--vector", str(float(identifier)),
    )
    return sorted(
        str(hit.get("object_id"))
        for hit in response.get("hits", [])
        if isinstance(hit, dict)
    )


def assert_latest_state(binary: Path, data: Path, identifier: int) -> None:
    sql = run_json(
        binary, "sql", "--data-dir", str(data), "execute",
        "--statement", "SELECT id, body FROM soak_items WHERE id = ?",
        "--parameter", str(identifier),
    )
    if sql.get("result", {}).get("rows") != [
        [identifier, f"durable native item {identifier}"]
    ]:
        raise RuntimeError("latest SQL soak record differs after reopen")
    if structure_value(binary, data, identifier) != f"value-{identifier}":
        raise RuntimeError("latest structure soak record differs after reopen")
    if str(1_000_000 + identifier) not in search_hit_ids(binary, data, identifier):
        raise RuntimeError("latest search soak record differs after reopen")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def source_tree(expected_commit: str) -> str:
    head = subprocess.run(
        ("git", "rev-parse", "HEAD"), cwd=ROOT, check=True,
        capture_output=True, text=True,
    ).stdout.strip()
    if head != expected_commit:
        raise RuntimeError("source commit differs from checked-out HEAD")
    dirty = subprocess.run(
        ("git", "status", "--porcelain", "--untracked-files=no"), cwd=ROOT,
        check=True, capture_output=True, text=True,
    ).stdout.strip()
    if dirty:
        raise RuntimeError("tracked source worktree must be clean")
    return subprocess.run(
        ("git", "rev-parse", "HEAD^{tree}"), cwd=ROOT, check=True,
        capture_output=True, text=True,
    ).stdout.strip()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--platform", required=True, choices=("linux", "macos", "windows"))
    parser.add_argument("--cycles", type=int, default=4)
    parser.add_argument("--writes-per-cycle", type=int, default=32)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--independent-verifier", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    if (
        arguments.cycles < MINIMUM_CYCLES
        or arguments.writes_per_cycle < MINIMUM_WRITES_PER_CYCLE
    ):
        raise ValueError(
            f"G8 soak requires at least {MINIMUM_CYCLES} cycles and "
            f"{MINIMUM_WRITES_PER_CYCLE} writes per cycle"
        )
    if len(arguments.source_commit) != 40 or any(
        character not in "0123456789abcdef" for character in arguments.source_commit
    ):
        raise ValueError("source commit must be a canonical lowercase SHA-1")
    tree = source_tree(arguments.source_commit)
    binary = arguments.binary or Path(
        os.environ.get(
            "HYPHAE_BIN",
            ROOT / "target" / "debug" / ("hyphae.exe" if os.name == "nt" else "hyphae"),
        )
    )
    verifier = arguments.independent_verifier
    if not binary.is_file() or not verifier.is_file():
        raise RuntimeError("Hyphae and independent verifier binaries must already exist")

    with tempfile.TemporaryDirectory(prefix="hyphae-native-g8-soak-") as temporary:
        root = Path(temporary)
        data = root / "data"
        backup = root / "backup"
        restored = root / "restored"
        create_surfaces(binary, data)
        final_identifier = 0
        for cycle in range(arguments.cycles):
            final_identifier = write_cycle(
                binary, data, cycle, arguments.writes_per_cycle
            )
            kill_and_reopen(binary, data)
            assert_latest_state(binary, data, final_identifier)
        original_state_digest, original_root_digest, semantic_samples = state_digest(
            binary, data, final_identifier
        )
        created = run_json(
            binary,
            "backup",
            "create",
            "--data-dir",
            str(data),
            "--out",
            str(backup),
        )
        if created.get("status") != "created":
            raise RuntimeError("Native backup was not created")
        independent = subprocess.run(
            (str(verifier), str(backup)),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
            timeout=120,
        )
        independent_receipt = json.loads(independent.stdout)
        if independent_receipt.get("status") != "passed":
            raise RuntimeError("independent backup verification did not pass")
        restored_result = run_json(
            binary,
            "restore",
            "--backup",
            str(backup),
            "--data-dir",
            str(restored),
        )
        if restored_result.get("status") != "restored":
            raise RuntimeError("Native backup was not restored")
        restored_state_digest, restored_root_digest, restored_samples = state_digest(
            binary, restored, final_identifier
        )
        if restored_state_digest != original_state_digest:
            raise RuntimeError("restored all-engine state digest differs")
        if restored_root_digest != original_root_digest or restored_samples != semantic_samples:
            raise RuntimeError("restored all-engine root or semantic sample coverage differs")
        doctor = run_json(binary, "doctor", "--data-dir", str(restored))
        if doctor.get("status") != "healthy":
            raise RuntimeError("restored Native directory is unhealthy")
        manifest = backup / "NATIVE_BACKUP.json"
        receipt = {
            "schema": "hyphae-native-g8-soak-v2",
            "status": "passed",
            "source_commit": arguments.source_commit,
            "source_tree": tree,
            "platform": arguments.platform,
            "host": {"system": sys.platform, "machine": os.uname().machine if hasattr(os, "uname") else "windows"},
            "cycles": arguments.cycles,
            "writes_per_cycle": arguments.writes_per_cycle,
            "records": final_identifier,
            "forced_daemon_terminations": arguments.cycles,
            "engines": ["sql", "structures", "search"],
            "backup_manifest_sha256": sha256(manifest),
            "binary_sha256": sha256(binary),
            "independent_verifier_sha256": sha256(verifier),
            "independent_verification": independent_receipt,
            "state_digest_sha256": original_state_digest,
            "state_root_digest": original_root_digest,
            "state_equivalence_method": "all-engine-root-complete-sql-semantic-samples",
            "semantic_sample_records": semantic_samples,
            "restore_state_equivalent": True,
            "doctor_after_restore": "healthy",
        }
        arguments.output.write_text(
            json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
