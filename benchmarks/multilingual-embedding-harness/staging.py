#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Content-addressed read-only staging for embedding measurements."""

from __future__ import annotations

import csv
import errno
import hashlib
import io
import json
import os
import re
import shutil
import stat
import struct
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any

from contract import (
    ContractError,
    MAX_ACQUISITION_BODY_BYTES,
    MAX_ACQUISITION_RESPONSES,
    MAX_ACQUISITION_TOTAL_BODY_BYTES,
    MAX_SNAPSHOT_BYTES,
    MAX_SNAPSHOT_FILES,
    artifact_inventory_digest,
    load_json,
    require_fields,
    require_sha256,
    require_string,
    sha256_file,
    validate_manifest,
)

STAGE_SCHEMA = "hyphae-embedding-execution-stage-v2"
EXECUTED_HARNESS_FILES = [
    "check_receipt.py",
    "contained_run.py",
    "contract.py",
    "reference_subject.py",
    "run.py",
    "staging.py",
]
STAGED_HARNESS_FILES = [
    "acquire_evidence.py",
    "check_receipt.py",
    "contained_run.py",
    "contract.py",
    "populate_manifest.py",
    "reference_subject.py",
    "run.py",
    "staging.py",
]
MAX_HARNESS_FILE_BYTES = 4 * 1024 * 1024
MAX_INPUT_FILE_BYTES = 32 * 1024 * 1024
MAX_EVIDENCE_FILE_BYTES = 16 * 1024 * 1024
MAX_EXECUTABLE_BYTES = 256 * 1024 * 1024
MAX_MODEL_FILE_BYTES = 16 * 1024 * 1024 * 1024
MAX_VOLATILE_FILE_BYTES = 32 * 1024 * 1024
MAX_VOLATILE_BYTES = 128 * 1024 * 1024
MAX_PYTHON_ENVIRONMENT_FILES = 90_000
MAX_PYTHON_ENVIRONMENT_BYTES = 64 * 1024 * 1024 * 1024
MAX_PYVENV_BYTES = 64 * 1024
MAX_RUNTIME_LOADER_BYTES = 32 * 1024 * 1024
MAX_PYTHON_PATH_COMPONENTS = 256
MAX_PYTHON_SYMLINKS = 16
MAX_PYTHON_SYMLINK_BYTES = 1024
MAX_PYTHON_SOURCE_LINKS = MAX_PYTHON_ENVIRONMENT_FILES


@dataclass(frozen=True)
class StagePaths:
    root: Path
    manifest: Path
    harness: Path
    model: Path
    model_manifest: Path
    plan: Path
    corpus: Path
    acquisition_record: Path
    legal_evidence: Path
    python_executable: Path
    nvidia_smi: Path
    pycache: Path
    receipt: Path
    stdout: Path
    stderr: Path


@dataclass(frozen=True)
class _PythonLink:
    source_relative: Path
    link_text: str
    identity: tuple[int, int]


@dataclass(frozen=True)
class _PythonSource:
    approved_root: Path
    root_identity: str
    root_file_identity: tuple[int, int]
    source_relative: Path
    resolved_relative: Path
    staged: Path
    size_bytes: int
    file_identity: tuple[int, int]
    sha256: str
    links: tuple[_PythonLink, ...]


def _file_identity(observed: os.stat_result) -> tuple[int, int]:
    return observed.st_dev, observed.st_ino


def _source_fingerprint(observed: os.stat_result) -> tuple[int, int, int, int, int, int]:
    return (
        observed.st_dev,
        observed.st_ino,
        observed.st_mode,
        observed.st_size,
        observed.st_mtime_ns,
        observed.st_ctime_ns,
    )


def _relative_path(value: Any, label: str) -> str:
    text = require_string(value, label)
    path = PurePosixPath(text)
    if (
        path.is_absolute()
        or "\\" in text
        or not path.parts
        or any(part in {"", ".", ".."} for part in path.parts)
    ):
        raise ContractError(f"{label} is not a safe relative path")
    return text


def _copy_file(
    source: Path,
    destination: Path,
    *,
    maximum_bytes: int,
    executable: bool = False,
    expected_size: int | None = None,
    expected_identity: tuple[int, int] | None = None,
) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        source_lstat = source.lstat()
        if not stat.S_ISREG(source_lstat.st_mode):
            raise ContractError(f"stage source is not a regular file: {source}")
        source_fd = os.open(source, flags)
        source_stat = os.fstat(source_fd)
        if (
            not stat.S_ISREG(source_stat.st_mode)
            or _file_identity(source_stat) != _file_identity(source_lstat)
        ):
            raise ContractError(f"stage source is not a regular file: {source}")
        if source_stat.st_size <= 0 or source_stat.st_size > maximum_bytes:
            raise ContractError(f"stage source exceeds its byte bounds: {source}")
        if expected_size is not None and source_stat.st_size != expected_size:
            raise ContractError(f"stage source changed after preflight: {source}")
        if expected_identity is not None and (
            source_stat.st_dev,
            source_stat.st_ino,
        ) != expected_identity:
            raise ContractError(f"stage source identity changed after preflight: {source}")
        destination_fd = os.open(
            destination,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o500 if executable else 0o400,
        )
        try:
            copied = 0
            while chunk := os.read(source_fd, 1024 * 1024):
                copied += len(chunk)
                if copied > maximum_bytes:
                    raise ContractError(f"stage source grew beyond its byte bound: {source}")
                view = memoryview(chunk)
                while view:
                    written = os.write(destination_fd, view)
                    view = view[written:]
            final_stat = os.fstat(source_fd)
            try:
                final_lstat = source.lstat()
            except OSError as error:
                raise ContractError(f"stage source changed while being copied: {source}") from error
            if (
                copied != source_stat.st_size
                or _source_fingerprint(final_stat) != _source_fingerprint(source_stat)
                or _source_fingerprint(final_lstat) != _source_fingerprint(source_lstat)
            ):
                raise ContractError(f"stage source changed size while being copied: {source}")
            os.fsync(destination_fd)
        finally:
            os.close(destination_fd)
    except OSError as error:
        raise ContractError(f"cannot stage {source}: {error}") from error
    finally:
        if "source_fd" in locals():
            os.close(source_fd)


def _elf_interpreter(path: Path) -> Path:
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        header = os.pread(descriptor, 64, 0)
        if len(header) < 52 or header[:4] != b"\x7fELF" or header[5] not in {1, 2}:
            raise ContractError(f"staged executable is not a supported ELF file: {path}")
        byte_order = "<" if header[5] == 1 else ">"
        if header[4] == 2:
            program_offset = struct.unpack_from(f"{byte_order}Q", header, 32)[0]
            entry_size = struct.unpack_from(f"{byte_order}H", header, 54)[0]
            entry_count = struct.unpack_from(f"{byte_order}H", header, 56)[0]
            offset_index, size_index = 8, 32
            integer_format = "Q"
        elif header[4] == 1:
            program_offset = struct.unpack_from(f"{byte_order}I", header, 28)[0]
            entry_size = struct.unpack_from(f"{byte_order}H", header, 42)[0]
            entry_count = struct.unpack_from(f"{byte_order}H", header, 44)[0]
            offset_index, size_index = 4, 16
            integer_format = "I"
        else:
            raise ContractError(f"staged executable ELF class is unsupported: {path}")
        if entry_count > 256 or entry_size < size_index + struct.calcsize(integer_format):
            raise ContractError(f"staged executable program headers are malformed: {path}")
        for index in range(entry_count):
            entry = os.pread(descriptor, entry_size, program_offset + index * entry_size)
            if len(entry) != entry_size:
                raise ContractError(f"staged executable program header is truncated: {path}")
            if struct.unpack_from(f"{byte_order}I", entry, 0)[0] != 3:
                continue
            interpreter_offset = struct.unpack_from(
                f"{byte_order}{integer_format}", entry, offset_index
            )[0]
            interpreter_size = struct.unpack_from(
                f"{byte_order}{integer_format}", entry, size_index
            )[0]
            if interpreter_size < 2 or interpreter_size > 4096:
                raise ContractError(f"staged executable interpreter path is malformed: {path}")
            raw = os.pread(descriptor, interpreter_size, interpreter_offset)
            if len(raw) != interpreter_size or raw[-1:] != b"\0":
                raise ContractError(f"staged executable interpreter path is truncated: {path}")
            try:
                interpreter = Path(raw[:-1].decode("utf-8"))
            except UnicodeError as error:
                raise ContractError("staged executable interpreter path is not UTF-8") from error
            if not interpreter.is_absolute():
                raise ContractError("staged executable interpreter path is not absolute")
            return interpreter
    except OSError as error:
        raise ContractError(f"cannot inspect staged executable ELF headers: {path}: {error}") from error
    finally:
        if "descriptor" in locals():
            os.close(descriptor)
    raise ContractError(f"staged executable has no ELF interpreter: {path}")


def _runtime_loaders(paths: StagePaths) -> list[dict[str, Any]]:
    loaders = {_elf_interpreter(paths.python_executable), _elf_interpreter(paths.nvidia_smi)}
    records = []
    for path in sorted(loaders, key=str):
        resolved = path.resolve(strict=True)
        observed = _source_stat(resolved, maximum_bytes=MAX_RUNTIME_LOADER_BYTES)
        records.append(
            {
                "path": str(path),
                "size_bytes": observed.st_size,
                "sha256": sha256_file(resolved),
            }
        )
    return records


def _stage_identity(
    files: list[dict[str, Any]],
    runtime_loaders: list[dict[str, Any]],
    python_source_links: list[dict[str, Any]],
) -> str:
    records = list(files)
    records.extend(
        {
            "identity": f"os-runtime-loader:{item['path']}",
            "size_bytes": item["size_bytes"],
            "sha256": item["sha256"],
        }
        for item in runtime_loaders
    )
    records.sort(key=lambda item: item["identity"])
    inventory_digest = artifact_inventory_digest(
        records, "execution stage and OS runtime loader closure"
    )
    digest = hashlib.sha256(b"hyphae-embedding-execution-stage-identity-v2\0")
    digest.update(bytes.fromhex(inventory_digest))
    for item in python_source_links:
        encoded = json.dumps(
            item, ensure_ascii=False, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        digest.update(len(encoded).to_bytes(8, "little"))
        digest.update(encoded)
    return digest.hexdigest()


def _source_stat(
    path: Path,
    *,
    maximum_bytes: int,
    expected_size: int | None = None,
    expected_identity: tuple[int, int] | None = None,
) -> os.stat_result:
    try:
        path_lstat = path.lstat()
        if not stat.S_ISREG(path_lstat.st_mode):
            raise ContractError(f"stage source is empty or not a regular file: {path}")
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        observed = os.fstat(descriptor)
    except OSError as error:
        raise ContractError(f"cannot no-follow stat stage source {path}: {error}") from error
    finally:
        if "descriptor" in locals():
            os.close(descriptor)
    if (
        not stat.S_ISREG(observed.st_mode)
        or _file_identity(observed) != _file_identity(path_lstat)
        or observed.st_size <= 0
    ):
        raise ContractError(f"stage source is empty or not a regular file: {path}")
    if observed.st_size > maximum_bytes:
        raise ContractError(f"stage source exceeds its byte bound: {path}")
    if expected_size is not None and observed.st_size != expected_size:
        raise ContractError(f"stage source size differs from manifest: {path}")
    if expected_identity is not None and (observed.st_dev, observed.st_ino) != expected_identity:
        raise ContractError(f"stage source identity changed after path resolution: {path}")
    return observed


def _python_root_relative(path: Path, root: Path, label: str) -> Path:
    try:
        relative = path.relative_to(root)
    except ValueError as error:
        raise ContractError(f"{label} escapes its approved root") from error
    normalized: list[str] = []
    for part in relative.parts:
        if part in {"", "."}:
            continue
        if part == "..":
            if not normalized:
                raise ContractError(f"{label} escapes its approved root")
            normalized.pop()
        else:
            normalized.append(part)
        if len(normalized) > MAX_PYTHON_PATH_COMPONENTS:
            raise ContractError(f"{label} exceeds the path component bound")
    if not normalized:
        raise ContractError(f"{label} does not identify a Python environment entry")
    return Path(*normalized)


def _open_python_path(
    path: Path,
    approved_root: Path,
    root_identity: str,
) -> tuple[
    int,
    os.stat_result,
    Path,
    Path,
    tuple[_PythonLink, ...],
    tuple[int, int],
]:
    if not approved_root.is_absolute():
        raise ContractError("approved Python environment root is not absolute")
    relative = _python_root_relative(path, approved_root, "Python environment path")
    original_relative = relative
    seen_links: set[tuple[int, int]] = set()
    links: list[_PythonLink] = []
    root_fd = -1
    try:
        root_lstat = approved_root.lstat()
        if not stat.S_ISDIR(root_lstat.st_mode):
            raise ContractError("approved Python environment root is not a directory")
        root_fd = os.open(
            approved_root,
            os.O_RDONLY | os.O_DIRECTORY | getattr(os, "O_NOFOLLOW", 0),
        )
        root_stat = os.fstat(root_fd)
        if not stat.S_ISDIR(root_stat.st_mode) or _file_identity(root_stat) != _file_identity(
            root_lstat
        ):
            raise ContractError("approved Python environment root changed during inspection")
        while True:
            directory_fd = os.dup(root_fd)
            restart = False
            try:
                parts = relative.parts
                for index, part in enumerate(parts):
                    current_relative = Path(*parts[: index + 1])
                    observed = os.stat(part, dir_fd=directory_fd, follow_symlinks=False)
                    if stat.S_ISLNK(observed.st_mode):
                        identity = _file_identity(observed)
                        if identity in seen_links:
                            raise ContractError("Python environment symlink loop is forbidden")
                        seen_links.add(identity)
                        if len(seen_links) > MAX_PYTHON_SYMLINKS:
                            raise ContractError(
                                "Python environment symlink chain exceeds its bound"
                            )
                        try:
                            link_text = os.readlink(part, dir_fd=directory_fd)
                            encoded_link = link_text.encode("utf-8")
                        except (OSError, UnicodeError) as error:
                            raise ContractError(
                                "Python environment symlink text is unreadable"
                            ) from error
                        rechecked = os.stat(
                            part, dir_fd=directory_fd, follow_symlinks=False
                        )
                        rechecked_text = os.readlink(part, dir_fd=directory_fd)
                        if (
                            _source_fingerprint(rechecked) != _source_fingerprint(observed)
                            or rechecked_text != link_text
                        ):
                            raise ContractError(
                                "Python environment symlink changed during inspection"
                            )
                        if (
                            not encoded_link
                            or len(encoded_link) > MAX_PYTHON_SYMLINK_BYTES
                            or Path(link_text).is_absolute()
                        ):
                            raise ContractError(
                                "Python environment symlink must have bounded relative link text"
                            )
                        if index < len(parts) - 1:
                            raise ContractError(
                                "Python environment directory symlink is forbidden"
                            )
                        target = current_relative.parent / link_text
                        remaining = Path(*parts[index + 1 :])
                        if remaining.parts:
                            target /= remaining
                        relative = _python_root_relative(
                            approved_root / target,
                            approved_root,
                            "Python environment symlink target",
                        )
                        links.append(_PythonLink(current_relative, link_text, identity))
                        restart = True
                        break
                    final = index == len(parts) - 1
                    if not final and not stat.S_ISDIR(observed.st_mode):
                        raise ContractError(
                            "Python environment path component is not a directory: "
                            f"{approved_root / current_relative}"
                        )
                    if final and not (
                        stat.S_ISREG(observed.st_mode) or stat.S_ISDIR(observed.st_mode)
                    ):
                        raise ContractError(
                            "Python environment path target is not a regular file or directory: "
                            f"{approved_root / current_relative}"
                        )
                    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
                    if stat.S_ISDIR(observed.st_mode):
                        flags |= os.O_DIRECTORY
                    opened = os.open(part, flags, dir_fd=directory_fd)
                    opened_stat = os.fstat(opened)
                    if _source_fingerprint(opened_stat) != _source_fingerprint(observed):
                        os.close(opened)
                        raise ContractError(
                            "Python environment path changed during no-follow open: "
                            f"{approved_root / current_relative}"
                        )
                    if not final:
                        os.close(directory_fd)
                        directory_fd = opened
                        continue
                    if links and stat.S_ISDIR(opened_stat.st_mode):
                        os.close(opened)
                        raise ContractError(
                            "Python environment symlink target is not a regular file"
                        )
                    return (
                        opened,
                        opened_stat,
                        original_relative,
                        relative,
                        tuple(links),
                        _file_identity(root_stat),
                    )
            finally:
                os.close(directory_fd)
            if not restart:
                raise ContractError("Python environment path resolution did not terminate")
    except OSError as error:
        raise ContractError(f"cannot lstat Python environment path {path}: {error}") from error
    finally:
        if root_fd >= 0:
            os.close(root_fd)


def _resolve_python_path(
    path: Path,
    approved_root: Path,
    root_identity: str,
) -> tuple[Path, os.stat_result, Path, list[dict[str, str]]]:
    descriptor, observed, original, resolved, links, _ = _open_python_path(
        path, approved_root, root_identity
    )
    os.close(descriptor)
    resolved_identity = f"{root_identity}/{resolved.as_posix()}"
    return (
        approved_root / resolved,
        observed,
        original,
        [
            {
                "source_identity": f"{root_identity}/{item.source_relative.as_posix()}",
                "link_text": item.link_text,
                "resolved_target_identity": resolved_identity,
            }
            for item in links
        ],
    )


def _descriptor_digest(
    descriptor: int,
    *,
    maximum_bytes: int,
    label: str,
    retain_bytes: bool = False,
) -> tuple[str, bytes | None]:
    observed = os.fstat(descriptor)
    if (
        not stat.S_ISREG(observed.st_mode)
        or observed.st_size < 0
        or observed.st_size > maximum_bytes
    ):
        raise ContractError(f"{label} is irregular or exceeds its byte bound")
    os.lseek(descriptor, 0, os.SEEK_SET)
    digest = hashlib.sha256()
    data = bytearray() if retain_bytes else None
    copied = 0
    while chunk := os.read(descriptor, 1024 * 1024):
        copied += len(chunk)
        if copied > maximum_bytes:
            raise ContractError(f"{label} grew beyond its byte bound")
        digest.update(chunk)
        if data is not None:
            data.extend(chunk)
    final = os.fstat(descriptor)
    if copied != observed.st_size or _source_fingerprint(final) != _source_fingerprint(observed):
        raise ContractError(f"{label} changed while being read")
    return digest.hexdigest(), bytes(data) if data is not None else None


def _open_expected_python_source(source: _PythonSource) -> int:
    descriptor, observed, original, resolved, links, root_file_identity = _open_python_path(
        source.approved_root / source.source_relative,
        source.approved_root,
        source.root_identity,
    )
    if (
        root_file_identity != source.root_file_identity
        or original != source.source_relative
        or resolved != source.resolved_relative
        or links != source.links
        or _file_identity(observed) != source.file_identity
        or observed.st_size != source.size_bytes
        or not stat.S_ISREG(observed.st_mode)
    ):
        os.close(descriptor)
        raise ContractError("Python environment link or target changed after preflight")
    return descriptor


def _revalidate_python_source(source: _PythonSource, *, digest: bool) -> None:
    descriptor = _open_expected_python_source(source)
    try:
        if digest:
            observed_digest, _ = _descriptor_digest(
                descriptor,
                maximum_bytes=MAX_MODEL_FILE_BYTES,
                label="Python environment target",
            )
            if observed_digest != source.sha256:
                raise ContractError("Python environment target bytes changed after preflight")
    finally:
        os.close(descriptor)


def _prepare_python_source(
    path: Path,
    approved_root: Path,
    root_identity: str,
    staged: Path,
    *,
    maximum_bytes: int = MAX_MODEL_FILE_BYTES,
) -> _PythonSource:
    descriptor, observed, original, resolved, links, root_file_identity = _open_python_path(
        path, approved_root, root_identity
    )
    try:
        if not stat.S_ISREG(observed.st_mode):
            raise ContractError("Python environment path is not a regular file")
        digest, _ = _descriptor_digest(
            descriptor,
            maximum_bytes=maximum_bytes,
            label="Python environment target",
        )
    finally:
        os.close(descriptor)
    source = _PythonSource(
        approved_root=approved_root,
        root_identity=root_identity,
        root_file_identity=root_file_identity,
        source_relative=original,
        resolved_relative=resolved,
        staged=staged,
        size_bytes=observed.st_size,
        file_identity=_file_identity(observed),
        sha256=digest,
        links=links,
    )
    _revalidate_python_source(source, digest=True)
    return source


def _read_python_source_bytes(source: _PythonSource, *, maximum_bytes: int) -> bytes:
    descriptor = _open_expected_python_source(source)
    try:
        digest, data = _descriptor_digest(
            descriptor,
            maximum_bytes=maximum_bytes,
            label="Python environment target",
            retain_bytes=True,
        )
        if digest != source.sha256 or data is None:
            raise ContractError("Python environment target bytes changed after preflight")
    finally:
        os.close(descriptor)
    _revalidate_python_source(source, digest=True)
    return data


def _copy_python_source(source: _PythonSource, root: Path) -> None:
    destination = root / source.staged
    destination.parent.mkdir(parents=True, exist_ok=True)
    source_fd = _open_expected_python_source(source)
    destination_fd = -1
    try:
        destination_fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o400)
        digest = hashlib.sha256()
        copied = 0
        while chunk := os.read(source_fd, 1024 * 1024):
            copied += len(chunk)
            if copied > MAX_MODEL_FILE_BYTES:
                raise ContractError("Python environment target grew beyond its byte bound")
            digest.update(chunk)
            view = memoryview(chunk)
            while view:
                view = view[os.write(destination_fd, view) :]
        final = os.fstat(source_fd)
        if (
            copied != source.size_bytes
            or _file_identity(final) != source.file_identity
            or final.st_size != source.size_bytes
            or digest.hexdigest() != source.sha256
        ):
            raise ContractError("Python environment target changed while being copied")
        os.fsync(destination_fd)
    except OSError as error:
        raise ContractError(f"cannot stage Python environment source: {error}") from error
    finally:
        os.close(source_fd)
        if destination_fd >= 0:
            os.close(destination_fd)
    _revalidate_python_source(source, digest=True)
    _revalidate_python_source(source, digest=False)


def _python_source_links(sources: list[_PythonSource]) -> list[dict[str, Any]]:
    records = []
    for source in sources:
        resolved_target_identity = (
            f"{source.root_identity}/{source.resolved_relative.as_posix()}"
        )
        for link in source.links:
            records.append(
                {
                    "identity": source.staged.as_posix(),
                    "source_identity": (
                        f"{source.root_identity}/{link.source_relative.as_posix()}"
                    ),
                    "link_text": link.link_text,
                    "resolved_target_identity": resolved_target_identity,
                    "resolved_target_size_bytes": source.size_bytes,
                    "resolved_target_sha256": source.sha256,
                }
            )
            if len(records) > MAX_PYTHON_SOURCE_LINKS:
                raise ContractError("Python environment source link count exceeds its bound")
    records.sort(key=lambda item: (item["identity"], item["source_identity"], item["link_text"]))
    return records


def _read_source_bytes(path: Path, *, maximum_bytes: int) -> bytes:
    observed = _source_stat(path, maximum_bytes=maximum_bytes)
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        data = bytearray()
        while chunk := os.read(descriptor, 1024 * 1024):
            data.extend(chunk)
            if len(data) > maximum_bytes:
                raise ContractError(f"JSON stage source grew beyond its byte bound: {path}")
        if len(data) != observed.st_size or os.fstat(descriptor).st_size != observed.st_size:
            raise ContractError(f"JSON stage source changed while being read: {path}")
    except OSError as error:
        raise ContractError(f"cannot read stage source {path}: {error}") from error
    finally:
        if "descriptor" in locals():
            os.close(descriptor)
    return bytes(data)


def _read_json_source(path: Path, *, maximum_bytes: int) -> dict[str, Any]:
    try:
        value = json.loads(_read_source_bytes(path, maximum_bytes=maximum_bytes))
    except (UnicodeError, json.JSONDecodeError) as error:
        raise ContractError(f"stage source is not valid JSON: {path}: {error}") from error
    if not isinstance(value, dict):
        raise ContractError(f"stage JSON source is not an object: {path}")
    return value


def _preflight_sources(
    *,
    harness_dir: Path,
    model_dir: Path,
    manifest_path: Path,
    plan_path: Path,
    corpus_path: Path,
    acquisition_record_path: Path,
    legal_evidence_path: Path,
    python_executable: Path,
    nvidia_smi: Path,
    require_venv: bool,
) -> tuple[
    list[_PythonSource],
    str | None,
    list[dict[str, Any]],
]:
    manifest = _read_json_source(manifest_path, maximum_bytes=MAX_INPUT_FILE_BYTES)
    validate_manifest(manifest, require_verified=True)
    files = manifest["files"]
    if len(files) > MAX_SNAPSHOT_FILES:
        raise ContractError("model manifest file count exceeds the stage bound")
    aggregate_size = 0
    for item in files:
        expected_size = item["size_bytes"]
        aggregate_size += expected_size
        if aggregate_size > MAX_SNAPSHOT_BYTES:
            raise ContractError("model manifest aggregate size exceeds the stage bound")
        relative = _relative_path(item["path"], "model manifest file")
        _source_stat(
            model_dir / relative,
            maximum_bytes=MAX_MODEL_FILE_BYTES,
            expected_size=expected_size,
        )

    acquisition = _read_json_source(
        acquisition_record_path, maximum_bytes=MAX_EVIDENCE_FILE_BYTES
    )
    responses = acquisition.get("responses")
    capture = acquisition.get("capture")
    if (
        not isinstance(responses, list)
        or len(responses) < 3
        or len(responses) > MAX_ACQUISITION_RESPONSES
        or not isinstance(capture, dict)
    ):
        raise ContractError("acquisition response count exceeds the stage bound")
    evidence = [(capture.get("tool_path"), None)]
    evidence.extend(
        (response.get("body_path"), response.get("body_size_bytes"))
        for response in responses
        if isinstance(response, dict)
    )
    if len(evidence) != len(responses) + 1:
        raise ContractError("acquisition response inventory is malformed")
    aggregate_bodies = 0
    for relative_value, expected_size in evidence:
        relative = _relative_path(relative_value, "acquisition evidence file")
        if expected_size is not None:
            if not isinstance(expected_size, int) or isinstance(expected_size, bool):
                raise ContractError("acquisition response size is malformed")
            if expected_size > MAX_ACQUISITION_BODY_BYTES:
                raise ContractError("acquisition response exceeds 16 MiB")
            aggregate_bodies += expected_size
            if aggregate_bodies > MAX_ACQUISITION_TOTAL_BODY_BYTES:
                raise ContractError("acquisition response aggregate exceeds 64 MiB")
        _source_stat(
            acquisition_record_path.parent / relative,
            maximum_bytes=MAX_EVIDENCE_FILE_BYTES,
            expected_size=expected_size,
        )

    for path, maximum in (
        (legal_evidence_path, MAX_EVIDENCE_FILE_BYTES),
        (plan_path, MAX_INPUT_FILE_BYTES),
        (corpus_path, MAX_INPUT_FILE_BYTES),
    ):
        _source_stat(path, maximum_bytes=maximum)
    for name in STAGED_HARNESS_FILES:
        _source_stat(harness_dir / name, maximum_bytes=MAX_HARNESS_FILE_BYTES)
    for executable in (python_executable.resolve(strict=True), nvidia_smi.resolve(strict=True)):
        _source_stat(executable, maximum_bytes=MAX_EXECUTABLE_BYTES)
    python_environment, python_version_directory, python_source_links = _python_environment_sources(
        python_executable
    )
    if require_venv and python_version_directory is None:
        raise ContractError("measurement Python must be a real isolated venv")
    if len(python_environment) > MAX_PYTHON_ENVIRONMENT_FILES:
        raise ContractError("Python environment file count exceeds the stage bound")
    python_environment_bytes = 0
    for source in python_environment:
        python_environment_bytes += source.size_bytes
        if python_environment_bytes > MAX_PYTHON_ENVIRONMENT_BYTES:
            raise ContractError("Python environment aggregate size exceeds the stage bound")
    return python_environment, python_version_directory, python_source_links


def _python_environment_sources(
    python_executable: Path,
) -> tuple[
    list[_PythonSource],
    str | None,
    list[dict[str, Any]],
]:
    environment_root = python_executable.parent.parent
    resolved_root = environment_root.resolve(strict=True)
    try:
        if not stat.S_ISDIR(resolved_root.lstat().st_mode):
            raise ContractError("external Python venv root is not a directory")
    except OSError as error:
        raise ContractError(f"cannot lstat external Python venv root: {error}") from error
    configuration = resolved_root / "pyvenv.cfg"
    try:
        configuration.lstat()
    except FileNotFoundError:
        return [], None, []
    except OSError as error:
        raise ContractError(f"cannot lstat external pyvenv.cfg: {error}") from error
    configuration_source = _prepare_python_source(
        configuration,
        resolved_root,
        "venv",
        Path("inputs/python-environment/pyvenv.cfg.source"),
        maximum_bytes=MAX_PYVENV_BYTES,
    )
    configuration_bytes = _read_python_source_bytes(
        configuration_source, maximum_bytes=MAX_PYVENV_BYTES
    )
    try:
        configuration_text = configuration_bytes.decode("utf-8")
    except UnicodeError as error:
        raise ContractError("external pyvenv.cfg is not UTF-8") from error
    settings = {}
    for line in configuration_text.splitlines():
        if not line.strip():
            continue
        if len(line.encode("utf-8")) > 4096:
            raise ContractError("external pyvenv.cfg line exceeds 4096 bytes")
        name, separator, value = line.partition("=")
        name = name.strip().lower()
        if not separator or not name or name in settings:
            raise ContractError("external pyvenv.cfg is malformed or duplicated")
        settings[name] = value.strip()
    allowed_settings = {
        "home",
        "include-system-site-packages",
        "version",
        "executable",
        "command",
        "prompt",
    }
    if not {"home", "include-system-site-packages", "version"}.issubset(settings) or not set(
        settings
    ).issubset(allowed_settings):
        raise ContractError("external pyvenv.cfg fields are not canonical")
    if settings.get("include-system-site-packages", "").lower() != "false":
        raise ContractError("external pyvenv.cfg must set include-system-site-packages = false")
    home = Path(settings.get("home", ""))
    if not home.is_absolute():
        raise ContractError("external pyvenv.cfg home is not an absolute directory")
    try:
        resolved_home = home.resolve(strict=True)
        home_stat = resolved_home.lstat()
    except OSError as error:
        raise ContractError("external pyvenv.cfg home is not an absolute directory") from error
    if not stat.S_ISDIR(home_stat.st_mode):
        raise ContractError("external pyvenv.cfg home is not an absolute directory")
    sources: dict[Path, _PythonSource] = {
        configuration_source.staged: configuration_source
    }
    site_packages = sorted(resolved_root.glob("lib/python*/site-packages"))
    if len(site_packages) != 1:
        raise ContractError("external Python venv must have one site-packages directory")
    _, site_packages_stat, _, site_packages_links = _resolve_python_path(
        site_packages[0], resolved_root, "venv"
    )
    if not stat.S_ISDIR(site_packages_stat.st_mode) or site_packages_links:
        raise ContractError("external Python venv site-packages must be a real directory")
    python_version_directory = site_packages[0].parent.name
    expected_version = python_version_directory.removeprefix("python")
    if settings["version"] != expected_version and not settings["version"].startswith(
        f"{expected_version}."
    ):
        raise ContractError("external pyvenv.cfg version differs from site-packages")
    base_root = resolved_home.parent
    base_standard_library = base_root / "lib" / python_version_directory
    try:
        base_stat = base_root.lstat()
    except OSError as error:
        raise ContractError(f"cannot lstat external Python base root: {error}") from error
    if not stat.S_ISDIR(base_stat.st_mode):
        raise ContractError("external Python base root is not a directory")
    try:
        _, standard_library_stat, _, standard_library_links = _resolve_python_path(
            base_standard_library, base_root, "base-python"
        )
    except ContractError as error:
        raise ContractError("external Python base standard library is unavailable") from error
    if not stat.S_ISDIR(standard_library_stat.st_mode) or standard_library_links:
        raise ContractError("external Python base standard library is unavailable")

    def retain(path: Path) -> _PythonSource | None:
        try:
            path.lstat()
        except OSError as error:
            raise ContractError(f"cannot lstat Python venv source path: {path}") from error
        relative = _python_root_relative(path, resolved_root, "Python venv source path")
        if "__pycache__" in relative.parts or relative.suffix.lower() in {".pyc", ".pyo"}:
            return
        _, observed, _, links = _resolve_python_path(path, resolved_root, "venv")
        if stat.S_ISDIR(observed.st_mode):
            if links:
                raise ContractError("Python environment directory symlink is forbidden")
            return
        prepared = _prepare_python_source(path, resolved_root, "venv", relative)
        previous = sources.get(relative)
        if previous is not None and previous != prepared:
            raise ContractError(f"Python environment path is ambiguous: {relative}")
        sources[relative] = prepared
        return prepared

    for path in sorted(site_packages[0].rglob("*")):
        retain(path)
    for pth in sorted(site_packages[0].glob("*.pth")):
        pth_source = retain(pth)
        if pth_source is None:
            raise ContractError("Python .pth path is not a regular file")
        data = _read_python_source_bytes(
            pth_source, maximum_bytes=MAX_EVIDENCE_FILE_BYTES
        )
        try:
            lines = data.decode("utf-8").splitlines()
        except UnicodeError as error:
            raise ContractError("Python .pth file is not UTF-8") from error
        for line in lines:
            value = line.strip()
            if not value or value.startswith("#"):
                continue
            if value.startswith("import ") or value.startswith("import\t"):
                raise ContractError("Python .pth executable/import line is forbidden")
            path_value = PurePosixPath(value)
            if (
                path_value.is_absolute()
                or re.match(r"^[A-Za-z]:/", value) is not None
                or "\\" in value
                or value != path_value.as_posix()
                or any(part in {"", ".", ".."} for part in path_value.parts)
            ):
                raise ContractError("Python .pth path is not canonical and relative")
            target = site_packages[0] / path_value
            try:
                target_stat = target.lstat()
            except OSError as error:
                raise ContractError("Python .pth target is unavailable") from error
            if stat.S_ISLNK(target_stat.st_mode):
                raise ContractError("Python .pth target is a symlink")
            resolved_target, resolved_target_stat, _, target_links = _resolve_python_path(
                target, resolved_root, "venv"
            )
            if target_links or not stat.S_ISDIR(resolved_target_stat.st_mode):
                raise ContractError("Python .pth target is not a directory")
            for path in sorted(resolved_target.rglob("*")):
                retain(path)
    for record in sorted(site_packages[0].glob("*.dist-info/RECORD")):
        record_source = retain(record)
        if record_source is None:
            raise ContractError("Python distribution RECORD is not a regular file")
        data = _read_python_source_bytes(
            record_source, maximum_bytes=MAX_EVIDENCE_FILE_BYTES
        )
        try:
            rows = csv.reader(io.StringIO(data.decode("utf-8")))
            for row in rows:
                if len(row) != 3 or not row[0]:
                    raise ContractError("Python distribution RECORD row is malformed")
                retain(site_packages[0] / row[0])
        except UnicodeError as error:
            raise ContractError("Python distribution RECORD is not UTF-8") from error
    for path in sorted(base_standard_library.rglob("*")):
        relative = path.relative_to(base_standard_library)
        try:
            path.lstat()
        except OSError as error:
            raise ContractError(f"cannot lstat Python base environment path: {path}") from error
        if (
            "__pycache__" in relative.parts
            or path.suffix.lower() in {".pyc", ".pyo"}
            or relative.parts[0] in {"site-packages", "dist-packages"}
            or relative.as_posix() in {"sitecustomize.py", "usercustomize.py"}
        ):
            continue
        _, observed, _, links = _resolve_python_path(
            path, base_root, "base-python"
        )
        if stat.S_ISDIR(observed.st_mode):
            if links:
                raise ContractError("Python base environment directory symlink is forbidden")
            continue
        staged = Path("base/lib") / python_version_directory / relative
        prepared = _prepare_python_source(path, base_root, "base-python", staged)
        previous = sources.get(staged)
        if previous is not None and previous != prepared:
            raise ContractError(f"Python base environment path is ambiguous: {staged}")
        sources[staged] = prepared
    prepared_sources = [source for _, source in sorted(sources.items())]
    return prepared_sources, python_version_directory, _python_source_links(prepared_sources)


def _inventory(root: Path) -> list[dict[str, Any]]:
    records = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if relative.parts[0] == "volatile" or relative.as_posix() == "stage.json":
            continue
        if "__pycache__" in relative.parts or path.suffix.lower() in {".pyc", ".pyo"}:
            raise ContractError(f"staged closure contains bytecode cache state: {relative}")
        if path.is_symlink():
            raise ContractError(f"staged closure contains a symlink: {relative}")
        if path.is_dir():
            if path.stat().st_mode & 0o222:
                raise ContractError(f"staged closure directory is writable: {relative}")
            continue
        if not path.is_file() or path.stat().st_mode & 0o222:
            raise ContractError(f"staged closure file is mutable or irregular: {relative}")
        records.append(
            {
                "identity": relative.as_posix(),
                "size_bytes": path.stat().st_size,
                "sha256": sha256_file(path),
            }
        )
    return records


def _verify_unprivileged_executable(path: Path, label: str) -> None:
    descriptor = -1
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        observed = os.fstat(descriptor)
        if not stat.S_ISREG(observed.st_mode):
            raise ContractError(f"staged {label} is not a regular file")
        if observed.st_mode & (stat.S_ISUID | stat.S_ISGID):
            raise ContractError(f"staged {label} retains setuid or setgid privilege bits")
        try:
            os.getxattr(descriptor, "security.capability")
        except OSError as error:
            if error.errno != errno.ENODATA:
                raise ContractError(
                    f"cannot prove staged {label} has no file capabilities: {error}"
                ) from error
        else:
            raise ContractError(f"staged {label} retains file capabilities")
    except OSError as error:
        raise ContractError(f"cannot inspect staged {label}: {error}") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def _lock_stage(root: Path) -> None:
    for path in sorted(root.rglob("*"), reverse=True):
        relative = path.relative_to(root)
        if relative.parts[0] == "volatile":
            continue
        if path.is_dir():
            path.chmod(0o500)
        elif relative.as_posix() != "stage.json":
            path.chmod(0o500 if relative.parts[0] == "executables" else 0o400)
    for name in ("harness", "model", "inputs", "evidence", "executables"):
        (root / name).chmod(0o500)


def paths_from_root(root: Path) -> StagePaths:
    return StagePaths(
        root=root,
        manifest=root / "stage.json",
        harness=root / "harness",
        model=root / "model",
        model_manifest=root / "inputs/manifest.json",
        plan=root / "inputs/plan.json",
        corpus=root / "inputs/corpus.json",
        acquisition_record=root / "evidence/acquisition/acquisition.json",
        legal_evidence=root / "evidence/legal-evidence.json",
        python_executable=root / "executables/python",
        nvidia_smi=root / "executables/nvidia-smi",
        pycache=root / "volatile/pycache",
        receipt=root / "volatile/receipt.json",
        stdout=root / "volatile/stdout.log",
        stderr=root / "volatile/stderr.log",
    )


def initialize_volatile(paths: StagePaths) -> None:
    paths.root.mkdir(mode=0o700, parents=True, exist_ok=True)
    (paths.root / "volatile").mkdir(mode=0o700, exist_ok=True)
    paths.pycache.mkdir(mode=0o700, exist_ok=True)
    (paths.root / "volatile/tmp").mkdir(mode=0o700, exist_ok=True)
    for output in (paths.receipt, paths.stdout, paths.stderr):
        output.touch(mode=0o600, exist_ok=True)


def validate_volatile(paths: StagePaths) -> None:
    volatile = paths.root / "volatile"
    allowed = {
        "pycache",
        "tmp",
        "receipt.json",
        "stdout.log",
        "stderr.log",
    }
    observed = {path.name for path in volatile.iterdir()}
    if observed != allowed or any(paths.pycache.iterdir()):
        raise ContractError("execution stage volatile inventory differs")
    if any((volatile / "tmp").iterdir()):
        raise ContractError("execution stage temporary directory is not empty")
    total = 0
    for path in (paths.receipt, paths.stdout, paths.stderr):
        if path.is_symlink() or not path.is_file():
            raise ContractError("execution stage volatile output is irregular")
        size = path.stat().st_size
        if size > MAX_VOLATILE_FILE_BYTES:
            raise ContractError("execution stage volatile file exceeds 32 MiB")
        total += size
    if total > MAX_VOLATILE_BYTES:
        raise ContractError("execution stage aggregate writable bytes exceed 128 MiB")


def _build_stage_unchecked(
    root: Path,
    *,
    harness_dir: Path,
    model_dir: Path,
    manifest_path: Path,
    plan_path: Path,
    corpus_path: Path,
    acquisition_record_path: Path,
    legal_evidence_path: Path,
    python_executable: Path,
    nvidia_smi: Path,
    python_environment: list[_PythonSource],
    python_version_directory: str | None,
    python_source_links: list[dict[str, Any]],
) -> StagePaths:
    if root.exists() or not root.is_absolute() or not root.parent.is_dir():
        raise ContractError("stage root must be a new absolute path with an existing parent")
    root.mkdir(mode=0o700)
    paths = paths_from_root(root)
    initialize_volatile(paths)
    _copy_file(manifest_path, paths.model_manifest, maximum_bytes=MAX_INPUT_FILE_BYTES)
    manifest = load_json(paths.model_manifest)
    validate_manifest(manifest, require_verified=True)
    files = manifest.get("files")
    if not isinstance(files, list) or len(files) > MAX_SNAPSHOT_FILES:
        raise ContractError("model manifest file count exceeds the stage bound")
    aggregate_size = 0
    for item in files:
        if not isinstance(item, dict) or not isinstance(item.get("size_bytes"), int):
            raise ContractError("model manifest file inventory is malformed")
        aggregate_size += item["size_bytes"]
        if aggregate_size > MAX_SNAPSHOT_BYTES:
            raise ContractError("model manifest aggregate size exceeds the stage bound")

    for name in STAGED_HARNESS_FILES:
        _copy_file(
            harness_dir / name,
            paths.harness / name,
            maximum_bytes=MAX_HARNESS_FILE_BYTES,
        )
    _copy_file(plan_path, paths.plan, maximum_bytes=MAX_INPUT_FILE_BYTES)
    _copy_file(corpus_path, paths.corpus, maximum_bytes=MAX_INPUT_FILE_BYTES)
    _copy_file(
        legal_evidence_path, paths.legal_evidence, maximum_bytes=MAX_EVIDENCE_FILE_BYTES
    )
    _copy_file(
        acquisition_record_path,
        paths.acquisition_record,
        maximum_bytes=MAX_EVIDENCE_FILE_BYTES,
    )

    acquisition = load_json(paths.acquisition_record)
    capture = acquisition.get("capture")
    responses = acquisition.get("responses")
    if (
        not isinstance(capture, dict)
        or not isinstance(responses, list)
        or len(responses) < 3
        or len(responses) > MAX_ACQUISITION_RESPONSES
    ):
        raise ContractError("acquisition evidence cannot be staged")
    aggregate_body_bytes = 0
    for response in responses:
        if (
            not isinstance(response, dict)
            or not isinstance(response.get("body_size_bytes"), int)
            or isinstance(response.get("body_size_bytes"), bool)
            or response["body_size_bytes"] < 1
            or response["body_size_bytes"] > MAX_ACQUISITION_BODY_BYTES
        ):
            raise ContractError("acquisition response size cannot be staged")
        aggregate_body_bytes += response["body_size_bytes"]
        if aggregate_body_bytes > MAX_ACQUISITION_TOTAL_BODY_BYTES:
            raise ContractError("acquisition response aggregate cannot be staged")
    evidence_files = [(capture.get("tool_path"), None)]
    evidence_files.extend(
        (response.get("body_path"), response.get("body_size_bytes"))
        for response in responses
        if isinstance(response, dict)
    )
    for value, expected_size in evidence_files:
        relative = _relative_path(value, "acquisition evidence file")
        _copy_file(
            acquisition_record_path.parent / relative,
            paths.acquisition_record.parent / relative,
            maximum_bytes=MAX_EVIDENCE_FILE_BYTES,
            expected_size=expected_size,
        )

    for item in files:
        if not isinstance(item, dict):
            raise ContractError("model manifest file inventory is malformed")
        relative = _relative_path(item.get("path"), "model manifest file")
        _copy_file(
            model_dir / relative,
            paths.model / relative,
            maximum_bytes=MAX_MODEL_FILE_BYTES,
            expected_size=item["size_bytes"],
        )

    _copy_file(
        python_executable.resolve(strict=True),
        paths.python_executable,
        maximum_bytes=MAX_EXECUTABLE_BYTES,
        executable=True,
    )
    _copy_file(
        nvidia_smi.resolve(strict=True),
        paths.nvidia_smi,
        maximum_bytes=MAX_EXECUTABLE_BYTES,
        executable=True,
    )
    for source in python_environment:
        _copy_python_source(source, root)
    if python_version_directory is not None:
        (root / "base/bin").mkdir(parents=True, exist_ok=True)
        paths.python_executable.parent.mkdir(parents=True, exist_ok=True)
        generated_configuration = (
            f"home = {root / 'base/bin'}\n"
            "include-system-site-packages = false\n"
            f"version = {python_version_directory.removeprefix('python')}\n"
        )
        (root / "pyvenv.cfg").write_text(generated_configuration, encoding="utf-8")
    _lock_stage(root)
    records = _inventory(root)
    runtime_loaders = _runtime_loaders(paths)
    document = {
        "$comment": "SPDX-License-Identifier: Apache-2.0",
        "schema": STAGE_SCHEMA,
        "identity_sha256": _stage_identity(records, runtime_loaders, python_source_links),
        "files": records,
        "os_runtime_loaders": runtime_loaders,
        "python_source_links": python_source_links,
        "executed_harness_files": EXECUTED_HARNESS_FILES,
        "python_execution": "staged-file-open-descriptor-execve",
        "nvidia_smi_execution": "staged-read-only-copy",
        "pycache_policy": {
            "ignore_environment": True,
            "dont_write_bytecode": True,
            "pycache_prefix": "volatile/pycache",
            "preexisting_cache_entries": 0,
        },
    }
    paths.manifest.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    paths.manifest.chmod(0o400)
    root.chmod(0o500)
    validate_stage(paths)
    return paths


def build_stage(
    root: Path,
    *,
    harness_dir: Path,
    model_dir: Path,
    manifest_path: Path,
    plan_path: Path,
    corpus_path: Path,
    acquisition_record_path: Path,
    legal_evidence_path: Path,
    python_executable: Path,
    nvidia_smi: Path,
    require_venv: bool = False,
) -> StagePaths:
    existed = root.exists()
    try:
        python_environment, python_version_directory, python_source_links = _preflight_sources(
            harness_dir=harness_dir,
            model_dir=model_dir,
            manifest_path=manifest_path,
            plan_path=plan_path,
            corpus_path=corpus_path,
            acquisition_record_path=acquisition_record_path,
            legal_evidence_path=legal_evidence_path,
            python_executable=python_executable,
            nvidia_smi=nvidia_smi,
            require_venv=require_venv,
        )
        return _build_stage_unchecked(
            root,
            harness_dir=harness_dir,
            model_dir=model_dir,
            manifest_path=manifest_path,
            plan_path=plan_path,
            corpus_path=corpus_path,
            acquisition_record_path=acquisition_record_path,
            legal_evidence_path=legal_evidence_path,
            python_executable=python_executable,
            nvidia_smi=nvidia_smi,
            python_environment=python_environment,
            python_version_directory=python_version_directory,
            python_source_links=python_source_links,
        )
    except BaseException:
        if not existed:
            destroy_stage(paths_from_root(root))
        raise


def validate_stage(paths: StagePaths) -> dict[str, Any]:
    descriptor_root = str(paths.root).startswith("/proc/self/fd/")
    if (
        (paths.root.is_symlink() and not descriptor_root)
        or not paths.root.is_dir()
        or paths.root.stat().st_mode & 0o222
    ):
        raise ContractError("execution stage root is mutable or irregular")
    if paths.manifest.is_symlink() or paths.manifest.stat().st_mode & 0o222:
        raise ContractError("execution stage manifest is mutable")
    document = load_json(paths.manifest)
    validate_stage_document(document)
    records = _inventory(paths.root)
    if records != document["files"]:
        raise ContractError("execution stage file inventory differs")
    _verify_unprivileged_executable(paths.python_executable, "Python executable")
    _verify_unprivileged_executable(paths.nvidia_smi, "nvidia-smi executable")
    for item in document["os_runtime_loaders"]:
        path = Path(item["path"])
        resolved = path.resolve(strict=True)
        observed = _source_stat(resolved, maximum_bytes=MAX_RUNTIME_LOADER_BYTES)
        if observed.st_size != item["size_bytes"] or sha256_file(resolved) != item["sha256"]:
            raise ContractError("OS runtime loader bytes differ from the staged closure")
    if any(paths.pycache.iterdir()):
        raise ContractError("isolated bytecode cache prefix is not empty")
    validate_volatile(paths)
    return document


def validate_stage_document(document: dict[str, Any]) -> None:
    require_fields(
        document,
        {
            "$comment",
            "schema",
            "identity_sha256",
            "files",
            "os_runtime_loaders",
            "python_source_links",
            "executed_harness_files",
            "python_execution",
            "nvidia_smi_execution",
            "pycache_policy",
        },
        "execution stage manifest",
    )
    if (
        document["$comment"] != "SPDX-License-Identifier: Apache-2.0"
        or document["schema"] != STAGE_SCHEMA
        or document["executed_harness_files"] != EXECUTED_HARNESS_FILES
        or document["python_execution"] != "staged-file-open-descriptor-execve"
        or document["nvidia_smi_execution"] != "staged-read-only-copy"
        or document["pycache_policy"]
        != {
            "ignore_environment": True,
            "dont_write_bytecode": True,
            "pycache_prefix": "volatile/pycache",
            "preexisting_cache_entries": 0,
        }
    ):
        raise ContractError("execution stage policy differs")
    runtime_loaders = document["os_runtime_loaders"]
    if not isinstance(runtime_loaders, list) or not runtime_loaders:
        raise ContractError("execution stage OS runtime loader closure is missing")
    previous = ""
    for index, item in enumerate(runtime_loaders):
        item = require_fields(
            item, {"path", "size_bytes", "sha256"}, f"OS runtime loader {index}"
        )
        path = require_string(item["path"], f"OS runtime loader {index} path")
        if not Path(path).is_absolute() or path <= previous:
            raise ContractError("OS runtime loader paths must be absolute, sorted, and unique")
        if not isinstance(item["size_bytes"], int) or item["size_bytes"] < 1:
            raise ContractError("OS runtime loader size is invalid")
        require_sha256(item["sha256"], f"OS runtime loader {index} digest")
        previous = path
    python_source_links = document["python_source_links"]
    if (
        not isinstance(python_source_links, list)
        or len(python_source_links) > MAX_PYTHON_SOURCE_LINKS
    ):
        raise ContractError("execution stage Python source links exceed their bound")
    previous_link: tuple[str, str, str] | None = None
    staged_files = {
        item["identity"]: item for item in document["files"] if isinstance(item, dict)
    }
    for index, item in enumerate(python_source_links):
        item = require_fields(
            item,
            {
                "identity",
                "source_identity",
                "link_text",
                "resolved_target_identity",
                "resolved_target_size_bytes",
                "resolved_target_sha256",
            },
            f"Python source link {index}",
        )
        identity = _relative_path(item["identity"], f"Python source link {index} identity")
        source_identity = _relative_path(
            item["source_identity"], f"Python source link {index} source identity"
        )
        resolved_target_identity = _relative_path(
            item["resolved_target_identity"],
            f"Python source link {index} resolved target identity",
        )
        link_text = require_string(item["link_text"], f"Python source link {index} text")
        target_size = item["resolved_target_size_bytes"]
        target_sha256 = require_sha256(
            item["resolved_target_sha256"],
            f"Python source link {index} resolved target digest",
        )
        if (
            not isinstance(target_size, int)
            or isinstance(target_size, bool)
            or target_size < 0
            or target_size > MAX_MODEL_FILE_BYTES
        ):
            raise ContractError("Python source link resolved target size is invalid")
        try:
            encoded_link = link_text.encode("utf-8")
        except UnicodeError as error:
            raise ContractError("Python source link text is not UTF-8") from error
        if (
            not encoded_link
            or len(encoded_link) > MAX_PYTHON_SYMLINK_BYTES
            or Path(link_text).is_absolute()
            or "\0" in link_text
        ):
            raise ContractError("Python source link text is not bounded and relative")
        source_root, separator, source_relative = source_identity.partition("/")
        resolved_root, resolved_separator, _ = resolved_target_identity.partition("/")
        if (
            not separator
            or not resolved_separator
            or source_root != resolved_root
            or source_root not in {"venv", "base-python"}
        ):
            raise ContractError("Python source link crosses its approved root")
        direct_target: list[str] = list(PurePosixPath(source_relative).parent.parts)
        for part in PurePosixPath(link_text).parts:
            if part in {"", "."}:
                continue
            if part == "..":
                if not direct_target:
                    raise ContractError("Python source link escapes its approved root")
                direct_target.pop()
            else:
                direct_target.append(part)
            if len(direct_target) > MAX_PYTHON_PATH_COMPONENTS:
                raise ContractError("Python source link target exceeds its path bound")
        if not direct_target:
            raise ContractError("Python source link target is its approved root")
        if (
            identity not in staged_files
            or not source_identity.startswith(f"{source_root}/")
            or not resolved_target_identity.startswith(f"{source_root}/")
        ):
            raise ContractError("Python source link identities are outside the staged closure")
        staged_file = staged_files[identity]
        if (
            staged_file.get("size_bytes") != target_size
            or staged_file.get("sha256") != target_sha256
        ):
            raise ContractError(
                "Python source link resolved target differs from the staged materialization"
            )
        order = (identity, source_identity, link_text)
        if previous_link is not None and order <= previous_link:
            raise ContractError("Python source links must be sorted and unique")
        previous_link = order
    digest = _stage_identity(document["files"], runtime_loaders, python_source_links)
    if document["identity_sha256"] != require_sha256(
        digest, "execution stage identity"
    ):
        raise ContractError("execution stage identity digest differs")


def destroy_stage(paths: StagePaths) -> None:
    if not paths.root.exists():
        return
    paths.root.chmod(0o700)
    for path in paths.root.rglob("*"):
        if path.is_dir():
            path.chmod(0o700)
        else:
            path.chmod(0o600)
    shutil.rmtree(paths.root)
