#!/usr/bin/env python3
"""Extract and exercise one native Hyphae release archive without a network."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import shutil
import socket
import stat
import subprocess
import tarfile
import tempfile
import time
import tomllib
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath
from typing import Any


ROOT = Path(__file__).resolve().parents[1]


def validate_member(name: str) -> PurePosixPath:
    path = PurePosixPath(name)
    if path.is_absolute() or not path.parts or ".." in path.parts:
        raise RuntimeError(f"unsafe archive member: {name}")
    return path


def extract_archive(archive: Path, destination: Path) -> None:
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                validate_member(member.filename)
                mode = member.external_attr >> 16
                if stat.S_ISLNK(mode):
                    raise RuntimeError(f"archive symlink is forbidden: {member.filename}")
            bundle.extractall(destination)
        return
    if archive.name.endswith(".tar.gz"):
        with tarfile.open(archive, "r:gz") as bundle:
            for member in bundle.getmembers():
                relative = validate_member(member.name)
                target = destination.joinpath(*relative.parts)
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                    continue
                if not member.isfile():
                    raise RuntimeError(f"non-file archive member is forbidden: {member.name}")
                target.parent.mkdir(parents=True, exist_ok=True)
                source = bundle.extractfile(member)
                if source is None:
                    raise RuntimeError(f"archive member cannot be read: {member.name}")
                with source, target.open("wb") as output:
                    shutil.copyfileobj(source, output)
                target.chmod(member.mode & 0o777)
        return
    raise RuntimeError(f"unsupported release archive: {archive}")


def run_json(binary: Path, arguments: list[str], environment: dict[str, str]) -> Any:
    result = subprocess.run(
        (str(binary), *arguments),
        check=False,
        capture_output=True,
        text=True,
        env=environment,
        timeout=60,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"installed command failed ({' '.join(arguments)}): {result.stderr.strip()}"
        )
    return json.loads(result.stdout)


def require_command_failure(
    binary: Path,
    arguments: list[str],
    environment: dict[str, str],
) -> None:
    result = subprocess.run(
        (str(binary), *arguments),
        check=False,
        capture_output=True,
        text=True,
        env=environment,
        timeout=60,
    )
    if result.returncode == 0:
        raise RuntimeError("installed verifier accepted the tampered proof")


def workspace_version() -> str:
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    return manifest["workspace"]["package"]["version"]


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, sort_keys=True), encoding="utf-8", newline="\n")


def reserve_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def wait_until_live(base_url: str, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"installed server exited early with status {process.returncode}")
        try:
            with urllib.request.urlopen(f"{base_url}/v1/health/live", timeout=1) as response:
                if response.status == 200:
                    return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("installed server did not become live")


def exercise_retrieval(
    binary: Path,
    live: Path,
    root: Path,
    environment: dict[str, str],
    *,
    phase: str,
    initialize: bool,
    expected_outcomes: dict[str, Any] | None = None,
    remove_origin_before_verification: bool = False,
) -> dict[str, Any]:
    phase_root = root / phase
    phase_root.mkdir()
    port = reserve_loopback_port()
    base_url = f"http://127.0.0.1:{port}"
    process = subprocess.Popen(
        (str(binary), "serve", "--data-dir", str(live), "--bind", f"127.0.0.1:{port}"),
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        wait_until_live(base_url, process)
        requests = {
            "vector-space": {
                "vector_space": {
                    "name": "semantic",
                    "dimension": 2,
                    "metric": "cosine_q15_nanos",
                }
            },
            "vectors": {
                "vector_space": "semantic",
                "vectors": [
                    {"key_hex": "616c706861", "values": [32767, 0]},
                    {"key_hex": "62657461", "values": [0, 32767]},
                ],
            },
            "lexical-index": {
                "lexical_index": {
                    "name": "content",
                    "fields": [
                        {"path": ["title"], "weight_micros": 2_000_000},
                        {"path": ["body"], "weight_micros": 1_000_000},
                    ],
                }
            },
            "exact": {
                "vector_space": "semantic",
                "query": [32767, 0],
                "limit": 2,
                "minimum_score_nanos": -1_000_000_000,
                "minimum_margin_nanos": 0,
                "timeout_ms": 5000,
            },
            "lexical": {
                "lexical_index": "content",
                "query": "durable memory",
                "limit": 2,
                "timeout_ms": 5000,
            },
        }
        requests["hybrid"] = {
            "lexical": requests["lexical"],
            "vector": requests["exact"],
            "lexical_weight": 1,
            "vector_weight": 1,
            "limit": 2,
        }
        request_paths: dict[str, Path] = {}
        for name, value in requests.items():
            path = phase_root / f"{name}.json"
            write_json(path, value)
            request_paths[name] = path

        remote = ["remote", "--base-url", base_url]
        if initialize:
            for command, request_name in (
                ("define-vector-space", "vector-space"),
                ("put-vectors", "vectors"),
                ("define-lexical-index", "lexical-index"),
            ):
                run_json(
                    binary,
                    [*remote, command, "--request", str(request_paths[request_name])],
                    environment,
                )

        outcomes: dict[str, Any] = {}
        verification_inputs: list[tuple[str, Path, Path, str]] = []
        for kind in ("exact", "lexical", "hybrid"):
            response = run_json(
                binary,
                [*remote, f"retrieve-{kind}", "--request", str(request_paths[kind])],
                environment,
            )
            outcome = response.get("outcome")
            if not isinstance(outcome, dict):
                raise RuntimeError(f"installed {kind} retrieval omitted its outcome")
            outcomes[kind] = outcome
            proof = response.get("proof")
            if not isinstance(proof, dict) or "data" not in proof or "anchor_digest" not in proof:
                raise RuntimeError(f"installed {kind} retrieval omitted its proof")
            proof_json = phase_root / f"{kind}-proof.json"
            write_json(proof_json, proof)
            proof_file = phase_root / f"{kind}.hyrproof"
            proof_file.write_bytes(base64.b64decode(str(proof["data"]), validate=True))
            witness = phase_root / f"{kind}.hysnap"
            run_json(
                binary,
                [
                    *remote,
                    "witness",
                    "--proof",
                    str(proof_json),
                    "--out",
                    str(witness),
                ],
                environment,
            )
            verification_inputs.append(
                (kind, proof_file, witness, str(proof["anchor_digest"]))
            )
        if expected_outcomes is not None and outcomes != expected_outcomes:
            raise RuntimeError(
                f"installed retrieval changed during {phase}: "
                f"expected {expected_outcomes!r}, got {outcomes!r}"
            )
    finally:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=10)

    # Verification happens only after the serving process is gone. The install
    # smoke also removes the origin so only the downloaded witnesses remain.
    if remove_origin_before_verification:
        shutil.rmtree(live)
        if live.exists():
            raise RuntimeError("retrieval origin still exists before offline verification")
    for kind, proof_file, witness, anchor_digest in verification_inputs:
        verified = run_json(
            binary,
            [
                "verify-retrieval",
                "--kind",
                kind,
                "--proof",
                str(proof_file),
                "--snapshot",
                str(witness),
                "--anchor",
                anchor_digest,
            ],
            environment,
        )
        if verified.get("status") != "verified" or verified.get("operation") != kind:
            raise RuntimeError(f"installed {kind} retrieval verifier did not verify the proof")
    return outcomes


def verify_install(directory: Path) -> dict[str, Any]:
    archives = sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and (path.name.endswith(".tar.gz") or path.name.endswith(".zip"))
    )
    if len(archives) != 1:
        raise RuntimeError(f"expected exactly one native archive, found {len(archives)}")
    archive = archives[0]
    with tempfile.TemporaryDirectory(prefix="hyphae-installed-") as temporary:
        root = Path(temporary)
        installed = root / "installed"
        installed.mkdir()
        extract_archive(archive, installed)
        binaries = [
            path
            for path in installed.rglob("*")
            if path.is_file() and path.name in {"hyphae", "hyphae.exe"}
        ]
        if len(binaries) != 1:
            raise RuntimeError(f"expected exactly one installed binary, found {len(binaries)}")
        binary = binaries[0]
        environment = os.environ.copy()
        live = root / "hyphae-data"
        environment.pop("HYPHAE_DATA_DIR", None)

        version = run_json(binary, ["version", "--json"], environment)
        expected_version = workspace_version()
        expected = {
            "api_version": "v1",
            "disk_format_version": 2,
            "engine_version": expected_version,
            "native_directory_format": 1,
            "product": "hyphae",
            "product_api_version": 1,
        }
        if version != expected:
            raise RuntimeError(f"installed version mismatch: {version!r}")

        initialized = run_json(binary, ["init", "--data-dir", str(live)], environment)
        if initialized.get("status") != "initialized":
            raise RuntimeError("installed binary did not initialize native state")
        run_json(
            binary,
            [
                "structure",
                "--data-dir",
                str(live),
                "set",
                "--key",
                "alpha",
                "--value",
                "durable",
            ],
            environment,
        )
        read = run_json(
            binary,
            ["structure", "--data-dir", str(live), "get", "--key", "alpha"],
            environment,
        )
        if read.get("value") != "durable":
            raise RuntimeError("installed binary returned the wrong native structure value")
        run_json(
            binary,
            [
                "sql",
                "--data-dir",
                str(live),
                "execute",
                "--statement",
                "CREATE TABLE items (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
            ],
            environment,
        )
        run_json(
            binary,
            [
                "sql",
                "--data-dir",
                str(live),
                "execute",
                "--statement",
                "INSERT INTO items (id, name) VALUES (?, ?)",
                "--parameter",
                "1",
                "--parameter",
                '"alpha"',
            ],
            environment,
        )
        selected = run_json(
            binary,
            [
                "sql",
                "--data-dir",
                str(live),
                "execute",
                "--statement",
                "SELECT id, name FROM items WHERE id = ?",
                "--parameter",
                "1",
            ],
            environment,
        )
        if selected.get("result", {}).get("rows") != [[1, "alpha"]]:
            raise RuntimeError("installed binary returned the wrong native SQL row")
        run_json(
            binary,
            [
                "catalog", "--data-dir", str(live), "create-search-collection",
                "--database", "10", "--schema", "11", "--collection", "13",
                "--analyzer", "12", "--name", "main.public.installed",
            ],
            environment,
        )
        run_json(
            binary,
            ["search", "--data-dir", str(live), "provision", "--collection", "13"],
            environment,
        )
        run_json(
            binary,
            [
                "search", "--data-dir", str(live), "ingest", "--collection", "13",
                "--idempotency-id", "1", "--documents-json",
                json.dumps([
                    {
                        "id": 101,
                        "text": "installed native lexical vector search",
                        "doc_values": {"category": "smoke", "price": 1},
                        "vectors": {"exact": [0.0, 0.0], "ann": [0.0, 0.0]},
                    }
                ], separators=(",", ":")),
            ],
            environment,
        )
        searched = run_json(
            binary,
            [
                "search", "--data-dir", str(live), "integrated", "--collection", "13",
                "--lexical", "installed", "--vector-target", "exact",
                "--vector", "0", "--vector", "0",
            ],
            environment,
        )
        if not searched.get("hits"):
            raise RuntimeError("installed binary returned no Native search hit")
        if run_json(binary, ["status", "--data-dir", str(live)], environment).get("status") != "ready":
            raise RuntimeError("installed native status is not ready")
        run_json(binary, ["checkpoint", "--data-dir", str(live)], environment)
        run_json(binary, ["compact", "--data-dir", str(live)], environment)

        compatibility_origin = root / "format-2-origin"
        alpha = {
            "body": "offline agent memory",
            "group": "x",
            "score": 10,
            "title": "Durable memory",
        }
        beta = {
            "body": "exact vector retrieval",
            "group": "x",
            "score": 20,
            "title": "Fast search",
        }
        for key, value in (("alpha", alpha), ("beta", beta)):
            run_json(
                binary,
                [
                    "put",
                    "--data-dir",
                    str(compatibility_origin),
                    "--key",
                    key,
                    "--json",
                    json.dumps(value),
                ],
                environment,
            )

        proof = root / "result.hyproof"
        proven = run_json(
            binary,
            [
                "query",
                "--data-dir",
                str(compatibility_origin),
                "--sort",
                "score",
                "--descending",
                "--limit",
                "2",
                "--proof-out",
                str(proof),
            ],
            environment,
        )
        proof_metadata = proven.get("proof")
        if not isinstance(proof_metadata, dict):
            raise RuntimeError("installed compatibility query omitted its proof")
        anchor_digest = proof_metadata.get("anchor_digest")
        snapshot_path = proof_metadata.get("snapshot_path")
        if not isinstance(anchor_digest, str) or not isinstance(snapshot_path, str):
            raise RuntimeError("installed compatibility query returned incomplete proof metadata")
        snapshot = root / "result.hysnap"
        shutil.copyfile(snapshot_path, snapshot)

        exercise_retrieval(
            binary,
            compatibility_origin,
            root,
            environment,
            phase="compatibility-retrieval",
            initialize=True,
            remove_origin_before_verification=True,
        )
        if compatibility_origin.exists():
            raise RuntimeError("compatibility origin exists during offline verification")

        verified_query = run_json(
            binary,
            [
                "verify",
                "--proof",
                str(proof),
                "--snapshot",
                str(snapshot),
                "--anchor",
                anchor_digest,
            ],
            environment,
        )
        verified_rows = (
            verified_query.get("result", {}).get("result", {}).get("rows", [])
        )
        if verified_query.get("status") != "verified" or [
            row.get("key_hex") for row in verified_rows
        ] != ["62657461", "616c706861"]:
            raise RuntimeError("installed compatibility query proof was not verified")

        tampered = root / "tampered.hyproof"
        tampered_bytes = bytearray(proof.read_bytes())
        if not tampered_bytes:
            raise RuntimeError("installed compatibility query wrote an empty proof")
        tampered_bytes[-1] ^= 1
        tampered.write_bytes(tampered_bytes)
        require_command_failure(
            binary,
            [
                "verify",
                "--proof",
                str(tampered),
                "--snapshot",
                str(snapshot),
                "--anchor",
                anchor_digest,
            ],
            environment,
        )

        backup = root / "hyphae-backup"
        restored = root / "hyphae-restored"
        run_json(
            binary,
            ["backup", "create", "--data-dir", str(live), "--out", str(backup)],
            environment,
        )
        run_json(binary, ["backup", "verify", "--backup", str(backup)], environment)
        run_json(
            binary,
            ["restore", "--backup", str(backup), "--data-dir", str(restored)],
            environment,
        )
        run_json(binary, ["doctor", "--data-dir", str(restored)], environment)
        restored_value = run_json(
            binary,
            ["structure", "--data-dir", str(restored), "get", "--key", "alpha"],
            environment,
        )
        if restored_value.get("value") != "durable":
            raise RuntimeError("installed restore did not preserve the durable value")
        return {
            "archive": archive.name,
            "engine_version": expected_version,
            "negative_control": "rejected",
            "native_engines": ["sql", "structures", "search"],
            "proofs_verified": 4,
            "status": "ok",
        }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--source-commit")
    parser.add_argument("--platform")
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    result = verify_install(arguments.directory)
    if (arguments.source_commit is None) != (arguments.platform is None):
        raise ValueError("source-commit and platform must be supplied together")
    if arguments.source_commit is not None:
        if len(arguments.source_commit) != 40 or any(
            character not in "0123456789abcdef" for character in arguments.source_commit
        ):
            raise ValueError("source commit must be a canonical lowercase SHA-1")
        head = subprocess.run(
            ("git", "rev-parse", "HEAD"), cwd=ROOT, check=True,
            capture_output=True, text=True,
        ).stdout.strip()
        if head != arguments.source_commit:
            raise RuntimeError("source commit differs from checked-out HEAD")
        dirty = subprocess.run(
            ("git", "status", "--porcelain", "--untracked-files=no"), cwd=ROOT,
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        if dirty:
            raise RuntimeError("tracked source worktree must be clean")
        source_tree = subprocess.run(
            ("git", "rev-parse", "HEAD^{tree}"), cwd=ROOT, check=True,
            capture_output=True, text=True,
        ).stdout.strip()
        archive = arguments.directory / result["archive"]
        result.update({
            "schema": "hyphae-native-installed-package-v1",
            "source_commit": arguments.source_commit,
            "source_tree": source_tree,
            "platform": arguments.platform,
            "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
            "installed_smoke": "passed",
        })
    encoded = json.dumps(result, indent=2 if arguments.output else None, sort_keys=True) + "\n"
    if arguments.output:
        arguments.output.write_text(encoded, encoding="utf-8")
    else:
        print(encoded, end="")


if __name__ == "__main__":
    main()
