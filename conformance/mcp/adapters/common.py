#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Shared fail-closed primitives for the pinned real-host MCP adapters."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import queue
import re
import subprocess
import threading
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[3]
HOSTS = ROOT / "conformance/mcp/hosts"
INSTALL_LOCK = HOSTS / "install-lock.json"
PACKAGE_LOCK = HOSTS / "package-lock.json"
CREDENTIAL = re.compile(rb"hyp1_[A-Za-z0-9_-]{16,}")


class AdapterFailure(RuntimeError):
    """The real host could not produce complete deterministic evidence."""


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_object(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise AdapterFailure(f"expected one JSON object: {path}")
    return value


def platform_key() -> str:
    systems = {"Darwin": "darwin", "Linux": "linux"}
    machines = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x64", "AMD64": "x64"}
    try:
        return f"{systems[platform.system()]}-{machines[platform.machine()]}"
    except KeyError as error:
        raise AdapterFailure("the installed host platform is not locked") from error


def _node_modules(executable: Path) -> Path:
    for parent in executable.parents:
        if parent.name == ".bin" and parent.parent.name == "node_modules":
            return parent.parent
    raise AdapterFailure("host executable is not from the repository npm installation")


def _native_executable(host: str, node_modules: Path, lane: str) -> Path:
    if host == "codex":
        targets = {
            "darwin-arm64": "aarch64-apple-darwin",
            "darwin-x64": "x86_64-apple-darwin",
            "linux-arm64": "aarch64-unknown-linux-musl",
            "linux-x64": "x86_64-unknown-linux-musl",
        }
        package = node_modules / f"@openai/codex-{lane}"
        return package / "vendor" / targets[lane] / "bin/codex"
    package = node_modules / f"@anthropic-ai/claude-code-{lane}"
    return package / "claude"


def verify_host(host: str, supplied: str) -> dict[str, str]:
    lock = load_object(INSTALL_LOCK)
    host_lock = lock.get("hosts", {}).get(host)
    if not isinstance(host_lock, dict):
        raise AdapterFailure("host is absent from the install lock")
    supplied_path = Path(supplied)
    if not supplied_path.is_absolute() or supplied_path.name != host_lock.get("executable"):
        raise AdapterFailure("wrapper supplied an unexpected host executable")
    node_modules = _node_modules(supplied_path)
    lane = platform_key()
    try:
        native = _native_executable(host, node_modules, lane).resolve(strict=True)
        expected_digest = host_lock["sha256"][lane]
    except (KeyError, FileNotFoundError) as error:
        raise AdapterFailure("platform-native host executable is missing or unlocked") from error
    observed_digest = sha256(native)
    if observed_digest != expected_digest:
        raise AdapterFailure("platform-native host executable digest differs from install lock")

    package_name = host_lock.get("package")
    package_path = node_modules / str(package_name) / "package.json"
    package = load_object(package_path)
    package_lock = load_object(PACKAGE_LOCK)
    locked_package = package_lock.get("packages", {}).get(f"node_modules/{package_name}")
    if not isinstance(locked_package, dict):
        raise AdapterFailure("host package is absent from package-lock.json")
    version = host_lock.get("version")
    if package.get("name") != package_name or package.get("version") != version:
        raise AdapterFailure("installed host package identity differs from install lock")
    if locked_package.get("version") != version or not isinstance(locked_package.get("integrity"), str):
        raise AdapterFailure("host package lock identity is invalid")

    completed = subprocess.run(
        [str(native), "--version"],
        check=False,
        capture_output=True,
        timeout=15,
        env=safe_environment(),
    )
    canary = credential_canary()
    require_secret_free(completed.stdout + completed.stderr, canary)
    version_output = completed.stdout.decode("utf-8", errors="replace").strip()
    if completed.returncode != 0 or version_output != host_lock.get("version_output"):
        raise AdapterFailure("host version output differs from install lock")
    return {
        "executable": str(native),
        "executable_sha256": observed_digest,
        "host_version": version_output,
        "package_name": str(package_name),
        "package_version": str(version),
        "package_integrity": locked_package["integrity"],
        "platform": lane,
    }


def safe_environment(**updates: str) -> dict[str, str]:
    environment: dict[str, str] = {}
    for name, value in os.environ.items():
        upper = name.upper()
        secret_name = (
            "API_KEY" in upper
            or upper.endswith("_TOKEN")
            or upper in {"OPENAI_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"}
        )
        if secret_name and name != "HYPHAE_NATIVE_API_KEY_FILE":
            continue
        environment[name] = value
    environment.update(updates)
    return environment


def credential_canary() -> bytes | None:
    path = os.environ.get("HYPHAE_NATIVE_API_KEY_FILE")
    if not path:
        return None
    value = Path(path).read_bytes().strip()
    if not value or CREDENTIAL.fullmatch(value) is None:
        raise AdapterFailure("HYPHAE_NATIVE_API_KEY_FILE is missing or invalid")
    return value


def require_secret_free(data: bytes, canary: bytes | None) -> None:
    if (canary and canary in data) or CREDENTIAL.search(data):
        raise AdapterFailure("host output contained credential material")


def run_json(command: list[str], environment: dict[str, str], canary: bytes | None) -> dict[str, Any]:
    completed = subprocess.run(
        command,
        cwd=ROOT,
        env=environment,
        capture_output=True,
        timeout=60,
    )
    require_secret_free(completed.stdout + completed.stderr, canary)
    if completed.returncode != 0:
        raise AdapterFailure(f"host setup command failed with status {completed.returncode}")
    try:
        value = json.loads(completed.stdout)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise AdapterFailure("host setup command did not return JSON") from error
    if not isinstance(value, dict):
        raise AdapterFailure("host setup command returned a non-object")
    return value


class JsonlControlPlane:
    """Bounded request/response driver for host JSONL control planes."""

    def __init__(
        self,
        command: list[str],
        environment: dict[str, str],
        canary: bytes | None,
    ) -> None:
        self.canary = canary
        self.process = subprocess.Popen(
            command,
            cwd=ROOT,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        if self.process.stdin is None or self.process.stdout is None or self.process.stderr is None:
            raise AdapterFailure("host control-plane pipes are unavailable")
        self.responses: queue.Queue[dict[str, Any] | BaseException] = queue.Queue(maxsize=256)
        self.stderr = bytearray()
        self.notifications: list[dict[str, Any]] = []
        self.frames: list[dict[str, Any]] = []
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        try:
            for line in self.process.stdout:
                encoded = line.encode("utf-8")
                require_secret_free(encoded, self.canary)
                value = json.loads(line)
                if not isinstance(value, dict):
                    raise AdapterFailure("host emitted a non-object JSONL frame")
                self.responses.put(value, timeout=1)
        except BaseException as error:
            try:
                self.responses.put(error, timeout=1)
            except queue.Full:
                pass

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        for block in iter(lambda: self.process.stderr.buffer.read(8192), b""):
            require_secret_free(block, self.canary)
            if len(self.stderr) < 65536:
                self.stderr.extend(block[: 65536 - len(self.stderr)])

    def send(self, value: dict[str, Any]) -> None:
        if self.process.stdin is None:
            raise AdapterFailure("host control-plane input is closed")
        self.process.stdin.write(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def request(self, identifier: str, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.send({"id": identifier, "method": method, "params": params})
        return self.wait(identifier)

    def control(self, identifier: str, subtype: str, **fields: Any) -> dict[str, Any]:
        self.send(
            {
                "type": "control_request",
                "request_id": identifier,
                "request": {"subtype": subtype, **fields},
            }
        )
        return self.wait(identifier)

    def wait(self, identifier: str, timeout: float = 30) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        while True:
            try:
                frame = self.responses.get(timeout=max(0.0, deadline - time.monotonic()))
            except queue.Empty as error:
                raise AdapterFailure(f"host control request timed out: {identifier}") from error
            if isinstance(frame, BaseException):
                raise AdapterFailure("host emitted invalid JSONL evidence") from frame
            if len(self.frames) >= 4096:
                raise AdapterFailure("host emitted too many control-plane frames")
            self.frames.append(frame)
            frame_id = frame.get("id")
            response = frame.get("response")
            response_id = response.get("request_id") if isinstance(response, dict) else None
            if frame_id == identifier:
                if "error" in frame:
                    raise AdapterFailure(
                        f"host request failed: {identifier}: {frame['error']!r}"
                    )
                result = frame.get("result")
                if not isinstance(result, dict):
                    raise AdapterFailure("host response result is not an object")
                return result
            if response_id == identifier:
                if response.get("subtype") != "success":
                    raise AdapterFailure(
                        f"host control request failed: {identifier}: {response!r}"
                    )
                result = response.get("response", {})
                if not isinstance(result, dict):
                    raise AdapterFailure("host control response is not an object")
                return result
            if len(self.notifications) >= 1024:
                raise AdapterFailure("host emitted too many control-plane notifications")
            self.notifications.append(frame)

    def close(self) -> None:
        if self.process.stdin is not None and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        require_secret_free(bytes(self.stderr), self.canary)


def structured_case(result: dict[str, Any], expected: dict[str, Any]) -> dict[str, Any]:
    if "structuredContent" not in result or not isinstance(result["structuredContent"], dict):
        raise AdapterFailure("host tool call omitted structuredContent")
    structured = result["structuredContent"]
    expected_outcome = expected["expect"]
    is_error = result.get("isError") is True or "error" in structured
    outcome = "invalid_request" if is_error and structured.get("error", {}).get("code") == "invalid_request" else "success"
    if outcome != expected_outcome:
        raise AdapterFailure(f"host tool outcome differs for {expected['id']}")
    return {
        "id": expected["id"],
        "tool": expected["tool"],
        "arguments": expected["arguments"],
        "outcome": outcome,
        "result": structured,
    }


def write_transcript(host: str, provenance: dict[str, str], tools: list[str], cases: list[dict[str, Any]]) -> None:
    output = Path(os.environ["HYPHAE_MCP_TRANSCRIPT"])
    value = {
        "schema": "hyphae-mcp-host-transcript-v1",
        "host": host,
        "host_version": provenance["host_version"],
        "host_platform": provenance["platform"],
        "host_executable_sha256": provenance["executable_sha256"],
        "installed_mcp_config_sha256": provenance["installed_mcp_config_sha256"],
        "tools": tools,
        "cases": cases,
    }
    encoded = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
    require_secret_free(encoded, credential_canary())
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(encoded)
