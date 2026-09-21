#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Stage, launch, and validate the fixed offline embedding subject."""

from __future__ import annotations

import os
import sys


def _ensure_source_only_bootstrap() -> None:
    if sys.flags.isolated and sys.dont_write_bytecode and sys.pycache_prefix is not None:
        try:
            if not os.listdir(sys.pycache_prefix):
                return
        except OSError:
            pass
    parent = "/tmp"
    try:
        output_index = sys.argv.index("--output") + 1
        candidate = os.path.dirname(os.path.abspath(sys.argv[output_index]))
        if os.path.isdir(candidate):
            parent = candidate
    except (ValueError, IndexError):
        pass
    prefix = os.path.join(
        parent,
        f".embedding-bootstrap-pycache-{os.getpid()}-{os.urandom(8).hex()}",
    )
    os.mkdir(prefix, 0o700)
    os.execv(
        sys.executable,
        [
            sys.executable,
            "-I",
            "-B",
            "-X",
            f"pycache_prefix={prefix}",
            os.path.abspath(__file__),
            *sys.argv[1:],
        ],
    )


if __name__ == "__main__":
    _ensure_source_only_bootstrap()

_HARNESS_DIR = os.path.dirname(os.path.abspath(__file__))
if _HARNESS_DIR not in sys.path:
    sys.path.insert(0, _HARNESS_DIR)

import argparse
import atexit
import json
import re
import resource
import secrets
import selectors
import shutil
import socket
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

from check_receipt import (
    FORBIDDEN_ENVIRONMENT_HOOKS,
    OFFLINE_ENVIRONMENT,
    PERMITTED_INHERITED_ENVIRONMENT,
    finalize_receipt,
    validate_receipt,
)
from contract import (
    CONTAINMENT_PROFILE,
    ContractError,
    ensure_absolute_executable,
    load_corpus,
    load_json,
    load_plan,
    parse_json_bytes,
    require_string,
    sha256_file,
    validate_manifest,
    verify_snapshot,
)
from staging import (
    build_stage,
    destroy_stage,
    validate_stage,
)

MAX_CHILD_FILE_BYTES = 32 * 1024 * 1024
MAX_MANAGER_ENVIRONMENT_BYTES = 1024 * 1024
MAX_MANAGER_ENVIRONMENT_NAMES = 4096
FROZEN_MANAGER_ENVIRONMENT_ALLOWLIST = {
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "CUDA_VISIBLE_DEVICES",
    "CUDA_DEVICE_ORDER",
    "NVIDIA_VISIBLE_DEVICES",
    "NVIDIA_DRIVER_CAPABILITIES",
}
SYSTEMD_GENERATED_ENVIRONMENT_ALLOWLIST = {
    "HOME",
    "LOGNAME",
    "USER",
    "SHELL",
    "INVOCATION_ID",
    "SYSTEMD_EXEC_PID",
    "MEMORY_PRESSURE_WATCH",
    "MEMORY_PRESSURE_WRITE",
}
SYSTEM_LIBRARY_EXEC_PATHS = tuple(
    str(path) for path in (Path("/lib"), Path("/lib64"), Path("/usr/lib"), Path("/usr/lib64"))
    if path.is_dir()
)
MANDATORY_UNSET_ENVIRONMENT = FORBIDDEN_ENVIRONMENT_HOOKS | {
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
}


def _cleanup_bootstrap_pycache() -> None:
    try:
        prefix = Path(sys.pycache_prefix or "")
        if prefix.name.startswith(".embedding-bootstrap-pycache-"):
            prefix.rmdir()
    except OSError:
        pass


atexit.register(_cleanup_bootstrap_pycache)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python", type=Path, required=True, help="external reference venv Python")
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--nvidia-smi", type=Path, required=True)
    parser.add_argument("--acquisition-record", type=Path, required=True)
    parser.add_argument("--legal-evidence", type=Path, required=True)
    parser.add_argument("--gpu-index", type=int, default=0)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def _offline_environment(pycache_prefix: Path | None = None) -> dict[str, str]:
    environment = {
        name: os.environ[name]
        for name in PERMITTED_INHERITED_ENVIRONMENT
        if name in os.environ and "\x00" not in os.environ[name]
    }
    environment.update(OFFLINE_ENVIRONMENT)
    if pycache_prefix is not None:
        environment["PYTHONPYCACHEPREFIX"] = str(pycache_prefix)
    return environment


def _run_staged_subject(
    python_fd: int,
    stage_fd: int,
    argv: list[str],
    environment: dict[str, str],
    stdout_path: Path,
    stderr_path: Path,
    timeout_seconds: float,
    maximum_file_bytes: int = MAX_CHILD_FILE_BYTES,
) -> None:
    if maximum_file_bytes < 1 or maximum_file_bytes > MAX_CHILD_FILE_BYTES:
        raise ContractError("child file byte limit is outside the fixed bound")
    pid = os.fork()
    if pid == 0:
        try:
            os.setsid()
            os.set_inheritable(stage_fd, True)
            resource.setrlimit(
                resource.RLIMIT_FSIZE,
                (maximum_file_bytes, maximum_file_bytes),
            )
            stdin_fd = os.open(os.devnull, os.O_RDONLY)
            stdout_fd = os.open(stdout_path, os.O_WRONLY | os.O_TRUNC)
            stderr_fd = os.open(stderr_path, os.O_WRONLY | os.O_TRUNC)
            os.dup2(stdin_fd, 0)
            os.dup2(stdout_fd, 1)
            os.dup2(stderr_fd, 2)
            os.execve(python_fd, argv, environment)
        except BaseException:
            os._exit(127)

    deadline = time.monotonic() + timeout_seconds
    status = None
    while status is None:
        waited, observed = os.waitpid(pid, os.WNOHANG)
        if waited == pid:
            status = observed
            break
        if time.monotonic() >= deadline:
            try:
                os.killpg(pid, 9)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
            raise ContractError("reference subject exceeded the fixed timeout")
        time.sleep(0.05)

    if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
        code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else -os.WTERMSIG(status)
        try:
            os.killpg(pid, 9)
        except ProcessLookupError:
            pass
        try:
            with stderr_path.open("rb") as source:
                detail = source.read(4097)
        except OSError:
            detail = b""
        suffix = detail[:4096].decode("utf-8", errors="replace").strip()
        if len(detail) > 4096:
            suffix += " [truncated]"
        message = f"reference subject failed with exit code {code}"
        if suffix:
            message += f": {suffix}"
        raise ContractError(message)


def _verify_open_executable(
    descriptor: int, stage_identity: dict, identity: str, label: str
) -> None:
    expected = next(
        (item["sha256"] for item in stage_identity["files"] if item["identity"] == identity),
        None,
    )
    observed = sha256_file(Path(f"/proc/self/fd/{descriptor}"))
    if expected is None or observed != expected:
        raise ContractError(f"open {label} descriptor differs from the staged identity")


def _containment_tools() -> tuple[Path, Path, dict[str, str]]:
    if not Path("/sys/fs/cgroup/cgroup.controllers").is_file():
        raise ContractError("cgroup v2 is required for measurement containment")
    systemd_run_name = shutil.which("systemd-run")
    systemctl_name = shutil.which("systemctl")
    if systemd_run_name is None or systemctl_name is None:
        raise ContractError("systemd-run and systemctl are required for measurement containment")
    systemd_run = ensure_absolute_executable(Path(systemd_run_name), "systemd-run")
    systemctl = ensure_absolute_executable(Path(systemctl_name), "systemctl")
    for path in (systemd_run, systemctl):
        if path.stat().st_size > 32 * 1024 * 1024:
            raise ContractError(f"containment tool exceeds 32 MiB: {path}")
    return systemd_run, systemctl, {
        "systemd-run": sha256_file(systemd_run),
        "systemctl": sha256_file(systemctl),
    }


def _parse_manager_environment_names(data: bytes) -> set[str]:
    if len(data) > MAX_MANAGER_ENVIRONMENT_BYTES:
        raise ContractError("manager environment exceeded its byte bound")
    names = set()
    for line in data.splitlines():
        raw_name, separator, _ = line.partition(b"=")
        if not separator or len(raw_name) > 256:
            raise ContractError("manager environment contained a malformed name")
        try:
            name = raw_name.decode("ascii")
        except UnicodeError as error:
            raise ContractError("manager environment contained a non-ASCII name") from error
        if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) is None:
            raise ContractError("manager environment contained an invalid name")
        names.add(name)
        if len(names) > MAX_MANAGER_ENVIRONMENT_NAMES:
            raise ContractError("manager environment exceeded its name-count bound")
    return names


def _manager_environment_names(systemctl: Path) -> set[str]:
    process = subprocess.Popen(
        [str(systemctl), "--user", "show-environment"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    if process.stdout is None:
        process.kill()
        process.wait()
        raise ContractError("manager environment query did not create a bounded pipe")
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    output = bytearray()
    deadline = time.monotonic() + 15
    try:
        while True:
            if time.monotonic() >= deadline:
                os.killpg(process.pid, 9)
                process.wait()
                raise ContractError("manager environment query exceeded its timeout")
            events = selector.select(timeout=0.1)
            if not events:
                if process.poll() is not None:
                    break
                continue
            chunk = os.read(process.stdout.fileno(), 4096)
            if not chunk:
                break
            output.extend(chunk)
            if len(output) > MAX_MANAGER_ENVIRONMENT_BYTES:
                os.killpg(process.pid, 9)
                process.wait()
                raise ContractError("manager environment exceeded its byte bound")
        returncode = process.wait(timeout=1)
    finally:
        selector.close()
        process.stdout.close()
    if returncode != 0:
        raise ContractError("manager environment query failed")
    return _parse_manager_environment_names(bytes(output))


def _unset_manager_environment(manager_names: set[str]) -> set[str]:
    if FROZEN_MANAGER_ENVIRONMENT_ALLOWLIST != set(PERMITTED_INHERITED_ENVIRONMENT):
        raise ContractError("manager environment allowlists differ internally")
    return (manager_names - FROZEN_MANAGER_ENVIRONMENT_ALLOWLIST) | MANDATORY_UNSET_ENVIRONMENT


def _verify_first_stage_environment(
    expected_manager_names: set[str],
    *,
    environment: dict[str, str] | None = None,
) -> None:
    observed = set(os.environ if environment is None else environment)
    intended = expected_manager_names | SYSTEMD_GENERATED_ENVIRONMENT_ALLOWLIST
    unexpected = observed - intended
    missing = expected_manager_names - observed
    if unexpected or missing:
        raise ContractError("first staged Python environment names differ from the frozen policy")
    if observed & MANDATORY_UNSET_ENVIRONMENT:
        raise ContractError("first staged Python retained a forbidden environment hook")


SYSTEMD_SHOW_PROPERTIES = [
    "ActiveState",
    "SubState",
    "ControlGroup",
    "MemoryMax",
    "MemorySwapMax",
    "TasksMax",
    "RuntimeMaxUSec",
    "LimitFSIZE",
    "LimitNOFILE",
    "IPAddressDeny",
    "RestrictAddressFamilies",
    "NoNewPrivileges",
    "RestrictSUIDSGID",
    "ProtectSystem",
    "ProtectHome",
    "ProtectControlGroups",
    "KillMode",
    "MemoryAccounting",
    "TasksAccounting",
    "IPAccounting",
    "ReadOnlyPaths",
    "ReadWritePaths",
    "InaccessiblePaths",
    "TemporaryFileSystem",
    "UnsetEnvironment",
    "UMask",
    "SystemCallFilter",
    "SystemCallErrorNumber",
    "NoExecPaths",
    "ExecPaths",
]


def _systemctl(
    systemctl: Path,
    arguments: list[str],
    *,
    temporary_directory: Path,
    check: bool,
) -> tuple[int, str, str]:
    with tempfile.TemporaryFile(dir=temporary_directory) as stdout, tempfile.TemporaryFile(
        dir=temporary_directory
    ) as stderr:
        process = subprocess.Popen(
            [str(systemctl), "--user", *arguments],
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            returncode = process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, 9)
            process.wait()
            raise ContractError("systemctl exceeded its fixed timeout") from None
        if stdout.tell() > 64 * 1024 or stderr.tell() > 64 * 1024:
            raise ContractError("systemctl output exceeded 64 KiB")
        stdout.seek(0)
        stderr.seek(0)
        output = stdout.read(64 * 1024 + 1).decode("utf-8", errors="replace")
        error = stderr.read(64 * 1024 + 1).decode("utf-8", errors="replace")
    if check and returncode != 0:
        raise ContractError(
            f"checked systemctl {' '.join(arguments)} failed: {error.strip()}"
        )
    return returncode, output, error


def _show_unit(
    systemctl: Path,
    unit: str,
    temporary_directory: Path,
) -> dict[str, str] | None:
    arguments = ["show", unit, "--no-pager"]
    arguments.extend(f"--property={name}" for name in SYSTEMD_SHOW_PROPERTIES)
    returncode, output, _ = _systemctl(
        systemctl,
        arguments,
        temporary_directory=temporary_directory,
        check=False,
    )
    if returncode != 0:
        return None
    properties = {}
    for line in output.splitlines():
        if "=" not in line:
            raise ContractError("systemctl show returned a malformed property")
        name, value = line.split("=", 1)
        properties[name] = value
    return properties


def _read_controller(path: Path) -> str:
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        data = os.read(descriptor, 129)
    except OSError as error:
        raise ContractError(f"cannot read cgroup controller {path}: {error}") from error
    finally:
        if "descriptor" in locals():
            os.close(descriptor)
    if len(data) > 128:
        raise ContractError(f"cgroup controller value is oversized: {path}")
    try:
        return data.decode("ascii").strip()
    except UnicodeError as error:
        raise ContractError(f"cgroup controller value is malformed: {path}") from error


def _cgroup_path(control_group: str, cgroup_root: Path = Path("/sys/fs/cgroup")) -> Path:
    value = Path(require_string(control_group, "systemd ControlGroup"))
    if not value.is_absolute() or ".." in value.parts:
        raise ContractError("systemd ControlGroup is not a safe absolute cgroup path")
    return cgroup_root.joinpath(*value.parts[1:])


def _parse_systemd_duration(value: str) -> float:
    multipliers = {
        "us": 0.000001,
        "ms": 0.001,
        "s": 1.0,
        "min": 60.0,
        "h": 3600.0,
        "d": 86_400.0,
    }
    total = 0.0
    position = 0
    pattern = re.compile(r"(\d+(?:\.\d+)?)(us|ms|min|s|h|d)")
    for match in pattern.finditer(value):
        if value[position : match.start()].strip():
            raise ContractError("effective systemd runtime limit is malformed")
        total += float(match.group(1)) * multipliers[match.group(2)]
        position = match.end()
    if position == 0 or value[position:].strip():
        raise ContractError("effective systemd runtime limit is malformed")
    return total


def _verify_user_manager_isolation(
    *,
    environment: dict[str, str] | None = None,
    socket_factory: Any = socket.socket,
) -> None:
    observed_environment = os.environ if environment is None else environment
    for name in ("DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR"):
        if name in observed_environment:
            raise ContractError(f"contained user-manager environment remains available: {name}")
    uid = os.getuid()
    paths = (
        f"/run/user/{uid}/bus",
        f"/run/user/{uid}/systemd/private",
        "/run/systemd/private",
        "/run/dbus/system_bus_socket",
        "/var/run/dbus/system_bus_socket",
    )
    for path in paths:
        try:
            candidate = socket_factory(socket.AF_UNIX, socket.SOCK_STREAM)
        except OSError:
            return
        try:
            try:
                candidate.connect(path)
            except OSError:
                continue
            raise ContractError(f"contained user-manager socket is reachable: {path}")
        finally:
            candidate.close()


def _exact_non_root_identity() -> tuple[int, int]:
    uid = os.getuid()
    gid = os.getgid()
    if (
        uid == 0
        or gid == 0
        or os.getresuid() != (uid, uid, uid)
        or os.getresgid() != (gid, gid, gid)
    ):
        raise ContractError("measurement containment requires one exact non-root uid and gid")
    return uid, gid


def _verify_contained_privilege_contract(
    expected_uid: int,
    expected_gid: int,
    *,
    status_path: Path = Path("/proc/self/status"),
) -> None:
    uid, gid = _exact_non_root_identity()
    if uid != expected_uid or gid != expected_gid:
        raise ContractError("contained process uid or gid differs from the launcher")
    try:
        data = status_path.read_bytes()
    except OSError as error:
        raise ContractError(f"cannot read contained process status: {error}") from error
    if len(data) > 64 * 1024:
        raise ContractError("contained process status exceeds 64 KiB")
    fields: dict[str, str] = {}
    for raw_line in data.splitlines():
        raw_name, separator, raw_value = raw_line.partition(b":")
        if not separator:
            continue
        try:
            name = raw_name.decode("ascii")
            value = raw_value.decode("ascii").strip()
        except UnicodeError as error:
            raise ContractError("contained process status is not ASCII") from error
        if name in fields:
            raise ContractError(f"contained process status field is duplicated: {name}")
        fields[name] = value
    if fields.get("Uid", "").split() != [str(expected_uid)] * 4:
        raise ContractError("contained process status uid set differs")
    if fields.get("Gid", "").split() != [str(expected_gid)] * 4:
        raise ContractError("contained process status gid set differs")
    if fields.get("NoNewPrivs") != "1":
        raise ContractError("contained process NoNewPrivileges is not active")
    for name in ("CapEff", "CapPrm", "CapInh", "CapAmb"):
        value = fields.get(name, "")
        if re.fullmatch(r"[0-9A-Fa-f]+", value) is None:
            raise ContractError(f"contained process capability set is malformed: {name}")
        if int(value, 16) != 0:
            raise ContractError(f"contained process capability set is nonzero: {name}")


def _validate_effective_containment(
    properties: dict[str, str],
    *,
    stage_root: Path,
    publish_path: Path,
    volatile_path: Path,
    expected_unset_environment: set[str] | None = None,
    expected_exec_paths: set[str] | None = None,
    cgroup_root: Path = Path("/sys/fs/cgroup"),
) -> Path:
    expected = {
        "ActiveState": "active",
        "MemoryMax": str(CONTAINMENT_PROFILE["memory_max_bytes"]),
        "MemorySwapMax": str(CONTAINMENT_PROFILE["memory_swap_max_bytes"]),
        "TasksMax": str(CONTAINMENT_PROFILE["tasks_max"]),
        "LimitFSIZE": str(CONTAINMENT_PROFILE["file_size_max_bytes"]),
        "LimitNOFILE": "128",
        "NoNewPrivileges": "yes",
        "RestrictSUIDSGID": "yes",
        "ProtectSystem": "strict",
        "ProtectHome": "read-only",
        "ProtectControlGroups": "yes",
        "KillMode": "control-group",
        "MemoryAccounting": "yes",
        "TasksAccounting": "yes",
        "IPAccounting": "yes",
    }
    for name, value in expected.items():
        if properties.get(name) != value:
            raise ContractError(f"effective systemd property differs: {name}")
    runtime = properties.get("RuntimeMaxUSec")
    if runtime in {None, "", "infinity"} or _parse_systemd_duration(runtime) > float(
        CONTAINMENT_PROFILE["runtime_max_seconds"]
    ):
        raise ContractError("effective systemd runtime limit is not finite")
    denied = properties.get("IPAddressDeny", "").split()
    if denied != ["any"] and set(denied) != {"0.0.0.0/0", "::/0"}:
        raise ContractError("effective systemd IPAddressDeny policy is missing")
    families = properties.get("RestrictAddressFamilies", "").split()
    if families != ["AF_UNIX"]:
        raise ContractError("effective systemd address-family restriction differs")
    syscall_filter = properties.get("SystemCallFilter", "").split()
    required_network_syscalls = {
        "socket",
        "socketpair",
        "connect",
        "bind",
        "listen",
        "accept",
        "accept4",
        "sendto",
        "recvfrom",
    }
    if syscall_filter == ["~@network-io"]:
        denied_syscalls = {"@network-io"}
    elif (
        not syscall_filter
        or not syscall_filter[0].startswith("~")
        or syscall_filter[0] == "~"
        or any(value.startswith(("~", "@")) for value in syscall_filter[1:])
    ):
        raise ContractError("effective systemd network syscall deny polarity differs")
    else:
        denied_syscalls = {syscall_filter[0][1:], *syscall_filter[1:]}
    if "@network-io" not in denied_syscalls and not required_network_syscalls.issubset(
        denied_syscalls
    ):
        raise ContractError("effective systemd network syscall denial differs")
    if properties.get("SystemCallErrorNumber") not in {"EPERM", "1"}:
        raise ContractError("effective systemd syscall error policy differs")
    expected_read_only = {
        "/tmp",
        "/var/tmp",
        "/dev/shm",
        f"/run/user/{os.getuid()}",
        str(stage_root),
    }
    if set(properties.get("ReadOnlyPaths", "").split()) != expected_read_only:
        raise ContractError("effective systemd read-only path set differs")
    if properties.get("ReadWritePaths", "").split() != []:
        raise ContractError("effective systemd writable path set differs")
    uid = os.getuid()
    expected_inaccessible = {
        f"/run/user/{uid}/bus",
        f"/run/user/{uid}/systemd/private",
        "/run/systemd/private",
        "/run/dbus/system_bus_socket",
        "/var/run/dbus/system_bus_socket",
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
    }
    if set(properties.get("InaccessiblePaths", "").split()) != expected_inaccessible:
        raise ContractError("effective systemd inaccessible socket set differs")
    expected_tmpfs = (
        f"{volatile_path}:rw,size={CONTAINMENT_PROFILE['writable_tmpfs_max_bytes']},mode=0700"
    )
    if properties.get("TemporaryFileSystem", "").split() != [expected_tmpfs]:
        raise ContractError("effective systemd writable tmpfs restriction differs")
    expected_unset = MANDATORY_UNSET_ENVIRONMENT if expected_unset_environment is None else expected_unset_environment
    if set(properties.get("UnsetEnvironment", "").split()) != expected_unset:
        raise ContractError("effective systemd environment removal differs")
    if properties.get("UMask") != "0077":
        raise ContractError("effective systemd UMask differs")
    if properties.get("NoExecPaths", "").split() != ["/"]:
        raise ContractError("effective systemd NoExecPaths differs")
    expected_exec = (
        {
            str(stage_root),
            str(stage_root / "executables/python"),
            str(stage_root / "executables/nvidia-smi"),
            *SYSTEM_LIBRARY_EXEC_PATHS,
        }
        if expected_exec_paths is None
        else expected_exec_paths
    )
    if set(properties.get("ExecPaths", "").split()) != expected_exec:
        raise ContractError("effective systemd ExecPaths set differs")
    cgroup = _cgroup_path(properties.get("ControlGroup", ""), cgroup_root)
    controllers = {
        "memory.max": str(CONTAINMENT_PROFILE["memory_max_bytes"]),
        "memory.swap.max": str(CONTAINMENT_PROFILE["memory_swap_max_bytes"]),
        "pids.max": str(CONTAINMENT_PROFILE["tasks_max"]),
    }
    for name, value in controllers.items():
        if _read_controller(cgroup / name) != value:
            raise ContractError(f"effective cgroup v2 controller differs: {name}")
    return cgroup


def _wait_for_effective_containment(
    systemctl: Path,
    unit: str,
    process: subprocess.Popen[bytes],
    *,
    stage_root: Path,
    publish_path: Path,
    volatile_path: Path,
    expected_unset_environment: set[str],
    expected_exec_paths: set[str],
    temporary_directory: Path,
) -> Path:
    deadline = time.monotonic() + 30
    last_error = "unit never became active"
    while time.monotonic() < deadline:
        properties = _show_unit(systemctl, unit, temporary_directory)
        if properties is not None and properties.get("ActiveState") == "active":
            return _validate_effective_containment(
                properties,
                stage_root=stage_root,
                publish_path=publish_path,
                volatile_path=volatile_path,
                expected_unset_environment=expected_unset_environment,
                expected_exec_paths=expected_exec_paths,
            )
        if process.poll() is not None:
            last_error = "unit exited before containment could be validated"
            break
        time.sleep(0.05)
    raise ContractError(last_error)


def _verify_cgroup_empty(cgroup: Path) -> None:
    if not cgroup.exists():
        return
    if _read_controller(cgroup / "cgroup.procs"):
        raise ContractError("containment cgroup still has processes after termination")


def _terminate_and_verify(
    systemctl: Path,
    unit: str,
    control_group: Path | None,
    *,
    temporary_directory: Path,
) -> None:
    stop_returncode, _, stop_error = _systemctl(
        systemctl,
        ["stop", unit],
        temporary_directory=temporary_directory,
        check=False,
    )
    if stop_returncode not in {0, 5}:
        raise ContractError(f"checked systemctl stop failed: {stop_error.strip()}")
    properties = _show_unit(systemctl, unit, temporary_directory)
    if properties is not None and properties.get("ActiveState") == "failed":
        returncode, _, error = _systemctl(
            systemctl,
            ["reset-failed", unit],
            temporary_directory=temporary_directory,
            check=False,
        )
        if returncode != 0 and _show_unit(systemctl, unit, temporary_directory) is not None:
            raise ContractError(f"checked systemctl reset-failed failed: {error.strip()}")
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        properties = _show_unit(systemctl, unit, temporary_directory)
        if properties is None or (
            properties.get("ActiveState") == "inactive"
            and properties.get("SubState") == "dead"
        ):
            break
        time.sleep(0.05)
    else:
        raise ContractError("systemd unit termination could not be proven")
    if control_group is None:
        properties = _show_unit(systemctl, unit, temporary_directory)
        if properties is not None and properties.get("ControlGroup"):
            control_group = _cgroup_path(properties["ControlGroup"])
    if control_group is None:
        raise ContractError("containment ControlGroup was never observed")
    _verify_cgroup_empty(control_group)


def _containment_properties(
    stage_root: Path,
    publish_path: Path,
    volatile_path: Path,
    unset_environment: set[str] | None = None,
    exec_paths: set[str] | None = None,
) -> list[str]:
    uid = os.getuid()
    unset_names = MANDATORY_UNSET_ENVIRONMENT if unset_environment is None else unset_environment
    effective_exec_paths = (
        {
            str(stage_root),
            str(stage_root / "executables/python"),
            str(stage_root / "executables/nvidia-smi"),
            *SYSTEM_LIBRARY_EXEC_PATHS,
        }
        if exec_paths is None
        else exec_paths
    )
    return [
        f"MemoryMax={CONTAINMENT_PROFILE['memory_max_bytes']}",
        f"MemorySwapMax={CONTAINMENT_PROFILE['memory_swap_max_bytes']}",
        f"TasksMax={CONTAINMENT_PROFILE['tasks_max']}",
        f"RuntimeMaxSec={CONTAINMENT_PROFILE['runtime_max_seconds']}",
        f"LimitFSIZE={CONTAINMENT_PROFILE['file_size_max_bytes']}",
        "LimitNOFILE=128",
        "IPAddressDeny=any",
        "RestrictAddressFamilies=AF_UNIX",
        "SystemCallFilter=~@network-io",
        "SystemCallErrorNumber=EPERM",
        "NoNewPrivileges=yes",
        "RestrictSUIDSGID=yes",
        "KillMode=control-group",
        "MemoryAccounting=yes",
        "TasksAccounting=yes",
        "IPAccounting=yes",
        "ProtectSystem=strict",
        "ProtectHome=read-only",
        "ProtectControlGroups=yes",
        "ReadOnlyPaths=/tmp /var/tmp /dev/shm",
        f"ReadOnlyPaths=/run/user/{uid}",
        f"ReadOnlyPaths={stage_root}",
        (
            "InaccessiblePaths="
            f"/run/user/{uid}/bus /run/user/{uid}/systemd/private "
            "/run/systemd/private /run/dbus/system_bus_socket /var/run/dbus/system_bus_socket "
            "/bin /sbin /usr/bin /usr/sbin /usr/local/bin /usr/local/sbin"
        ),
        f"UnsetEnvironment={' '.join(sorted(unset_names))}",
        "NoExecPaths=/",
        f"ExecPaths={' '.join(sorted(effective_exec_paths))}",
        f"TemporaryFileSystem={volatile_path}:rw,size={CONTAINMENT_PROFILE['writable_tmpfs_max_bytes']},mode=0700",
        "UMask=0077",
    ]


def _run_contained(
    command: list[str],
    *,
    parent_challenge: str,
    stage_root: Path,
    publish_path: Path,
    volatile_path: Path,
    timeout_seconds: int,
    os_runtime_loaders: list[dict[str, Any]],
) -> bytes:
    _exact_non_root_identity()
    systemd_run, systemctl, tool_digests = _containment_tools()
    manager_names = _manager_environment_names(systemctl)
    unset_environment = _unset_manager_environment(manager_names)
    retained_manager_names = sorted(
        manager_names & FROZEN_MANAGER_ENVIRONMENT_ALLOWLIST
    )
    exec_paths = {
        str(stage_root),
        str(stage_root / "executables/python"),
        str(stage_root / "executables/nvidia-smi"),
        *SYSTEM_LIBRARY_EXEC_PATHS,
    }
    exec_paths.update(item["path"] for item in os_runtime_loaders)
    contained_command = list(command)
    for name in retained_manager_names:
        contained_command.extend(["--expected-manager-environment-name", name])
    unit = f"hyphae-embedding-{secrets.token_hex(12)}.service"
    properties = _containment_properties(
        stage_root,
        publish_path,
        volatile_path,
        unset_environment,
        exec_paths,
    )
    invocation = [
        str(systemd_run),
        "--user",
        "--wait",
        "--pipe",
        "--quiet",
        "--service-type=exec",
        f"--unit={unit}",
    ]
    invocation.extend(f"--property={value}" for value in properties)
    invocation.extend(contained_command)
    environment = dict(os.environ)
    primary_error: BaseException | None = None
    teardown_error: BaseException | None = None
    process = None
    control_group = None
    provisional = b""
    with tempfile.TemporaryFile(dir=publish_path.parent) as stdout, tempfile.TemporaryFile(
        dir=publish_path.parent
    ) as stderr:
        try:
            process = subprocess.Popen(
                invocation,
                stdin=subprocess.PIPE,
                stdout=stdout,
                stderr=stderr,
                env=environment,
                start_new_session=True,
                preexec_fn=lambda: resource.setrlimit(
                    resource.RLIMIT_FSIZE,
                    (MAX_CHILD_FILE_BYTES, MAX_CHILD_FILE_BYTES),
                ),
            )
            if process.stdin is None:
                raise ContractError("contained parent challenge channel was not created")
            process.stdin.write(parent_challenge.encode("ascii") + b"\n")
            process.stdin.close()
            control_group = _wait_for_effective_containment(
                systemctl,
                unit,
                process,
                stage_root=stage_root,
                publish_path=publish_path,
                volatile_path=volatile_path,
                expected_unset_environment=unset_environment,
                expected_exec_paths=exec_paths,
                temporary_directory=publish_path.parent,
            )
            try:
                returncode = process.wait(timeout=timeout_seconds + 120)
            except subprocess.TimeoutExpired:
                raise ContractError("systemd containment exceeded its fixed timeout") from None
            if returncode != 0:
                stderr.seek(0)
                detail = stderr.read(4097)
                suffix = detail[:4096].decode("utf-8", errors="replace").strip()
                if len(detail) > 4096:
                    suffix += " [truncated]"
                message = f"systemd containment failed with exit code {returncode}"
                if suffix:
                    message += f": {suffix}"
                raise ContractError(message)
        except BaseException as error:
            primary_error = error
        finally:
            try:
                _terminate_and_verify(
                    systemctl,
                    unit,
                    control_group,
                    temporary_directory=publish_path.parent,
                )
            except BaseException as error:
                teardown_error = error
            if process is not None and process.poll() is None:
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, 9)
                    process.wait()
        if primary_error is not None:
            stderr.seek(0)
            detail = stderr.read(4097)
            suffix = detail[:4096].decode("utf-8", errors="replace").strip()
            if len(detail) > 4096:
                suffix += " [truncated]"
            if suffix:
                primary_error = ContractError(f"{primary_error}; systemd stderr: {suffix}")
        if sha256_file(systemd_run) != tool_digests["systemd-run"] or sha256_file(
            systemctl
        ) != tool_digests["systemctl"]:
            teardown_error = ContractError("containment tool bytes changed during execution")
        for item in os_runtime_loaders:
            path = Path(item["path"]).resolve(strict=True)
            if path.stat().st_size != item["size_bytes"] or sha256_file(path) != item["sha256"]:
                teardown_error = ContractError("OS runtime loader bytes changed during execution")
        if stdout.tell() > MAX_CHILD_FILE_BYTES:
            teardown_error = ContractError("provisional result exceeded 32 MiB")
        else:
            stdout.seek(0)
            provisional = stdout.read(MAX_CHILD_FILE_BYTES + 1)
    if teardown_error is not None:
        detail = f"containment teardown could not be proven: {teardown_error}"
        if primary_error is not None:
            detail = f"{primary_error}; {detail}"
        raise ContractError(detail) from primary_error
    if primary_error is not None:
        raise primary_error
    if not provisional:
        raise ContractError("contained unit returned no provisional result")
    return provisional


def _validate_public_arguments(
    arguments: argparse.Namespace, *, check_sources: bool = True
) -> tuple[Path, Path, Path]:
    python = (
        ensure_absolute_executable(arguments.python, "external Python")
        if check_sources
        else arguments.python
    )
    nvidia_smi = (
        ensure_absolute_executable(arguments.nvidia_smi, "nvidia-smi")
        if check_sources
        else arguments.nvidia_smi
    )
    if arguments.gpu_index < 0 or arguments.gpu_index > 15:
        raise ContractError("GPU index must be between 0 and 15")
    for path, label in (
        (arguments.model_dir, "model directory"),
        (arguments.manifest, "manifest"),
        (arguments.plan, "plan"),
        (arguments.corpus, "corpus"),
        (arguments.acquisition_record, "acquisition record"),
        (arguments.legal_evidence, "legal evidence"),
        (arguments.output, "output"),
    ):
        if not path.is_absolute():
            raise ContractError(f"{label} must be absolute")
    output = arguments.output
    if not output.parent.is_dir():
        raise ContractError("output parent must be an existing absolute directory")
    if output.exists():
        raise ContractError("output already exists")
    return python, nvidia_smi, output


def _contained_runner_command(
    *,
    stage: Any,
    stage_identity_sha256: str,
    gpu_index: int,
) -> list[str]:
    uid, gid = _exact_non_root_identity()
    return [
        str(stage.python_executable),
        "-I",
        "-B",
        "-X",
        f"pycache_prefix={stage.pycache}",
        str(stage.harness / "contained_run.py"),
        "--stage-root",
        str(stage.root),
        "--expected-stage-sha256",
        stage_identity_sha256,
        "--gpu-index",
        str(gpu_index),
        "--expected-uid",
        str(uid),
        "--expected-gid",
        str(gid),
    ]


def _stage_and_reexec(arguments: argparse.Namespace) -> int:
    python, nvidia_smi, output = _validate_public_arguments(arguments)
    stage = build_stage(
        output.parent / f".embedding-stage-{secrets.token_hex(16)}",
        harness_dir=Path(__file__).resolve().parent,
        model_dir=arguments.model_dir,
        manifest_path=arguments.manifest,
        plan_path=arguments.plan,
        corpus_path=arguments.corpus,
        acquisition_record_path=arguments.acquisition_record,
        legal_evidence_path=arguments.legal_evidence,
        python_executable=python,
        nvidia_smi=nvidia_smi,
        require_venv=True,
    )
    cleanup_stage = lambda: destroy_stage(stage)
    atexit.register(cleanup_stage)
    stage_document = validate_stage(stage)
    timeout_seconds = load_plan(stage.plan)["measurement"]["subprocess_timeout_seconds"]
    descriptor = os.open(stage.python_executable, os.O_RDONLY)
    try:
        _verify_open_executable(
            descriptor, stage_document, "executables/python", "Python"
        )
    finally:
        os.close(descriptor)
    parent_challenge = secrets.token_hex(32)
    command = _contained_runner_command(
        stage=stage,
        stage_identity_sha256=stage_document["identity_sha256"],
        gpu_index=arguments.gpu_index,
    )
    try:
        provisional_bytes = _run_contained(
            command,
            parent_challenge=parent_challenge,
            stage_root=stage.root,
            publish_path=output,
            volatile_path=stage.root / "volatile",
            timeout_seconds=timeout_seconds,
            os_runtime_loaders=stage_document["os_runtime_loaders"],
        )
        validate_stage(stage)
        provisional = parse_json_bytes(
            provisional_bytes,
            maximum_bytes=MAX_CHILD_FILE_BYTES,
            label="contained provisional result",
        )
        manifest = load_json(stage.model_manifest)
        plan = load_plan(stage.plan)
        _, records = load_corpus(stage.corpus, plan)
        receipt = finalize_receipt(
            provisional,
            parent_challenge,
            manifest,
            plan,
            records,
            harness_dir=stage.harness,
            manifest_path=stage.model_manifest,
            plan_path=stage.plan,
            corpus_path=stage.corpus,
            acquisition_record_path=stage.acquisition_record,
            legal_evidence_path=stage.legal_evidence,
            expected_stage=stage_document,
        )
        validate_receipt(
            receipt,
            manifest,
            plan,
            records,
            harness_dir=stage.harness,
            manifest_path=stage.model_manifest,
            plan_path=stage.plan,
            corpus_path=stage.corpus,
            acquisition_record_path=stage.acquisition_record,
            legal_evidence_path=stage.legal_evidence,
            expected_stage=stage_document,
        )
        with output.open("x", encoding="utf-8") as destination:
            json.dump(receipt, destination, indent=2, ensure_ascii=False)
            destination.write("\n")
    finally:
        try:
            destroy_stage(stage)
        finally:
            atexit.unregister(cleanup_stage)
    print(f"validated non-authoritative receipt written to {output}")
    return 0


def main() -> int:
    arguments = parse_args()
    try:
        return _stage_and_reexec(arguments)
    except (ContractError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
