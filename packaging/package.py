#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Build one deterministic Hyphae release archive from a native binary."""

from __future__ import annotations

import argparse
import datetime as dt
import gzip
import hashlib
import io
import json
import os
import subprocess
import tarfile
import tomllib
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
INCLUDED_DOCUMENTS = (
    "LICENSE",
    "LICENSE-DOCUMENTATION",
    "LICENSE-POLICY.md",
    "NOTICE",
    "README.md",
    "THIRD_PARTY_NOTICES.md",
)


def product_version() -> str:
    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text("utf-8"))
    return str(cargo["workspace"]["package"]["version"])


def source_date_epoch() -> int:
    configured = os.environ.get("SOURCE_DATE_EPOCH")
    if configured is not None:
        value = int(configured)
    else:
        completed = subprocess.run(
            ("git", "show", "-s", "--format=%ct", "HEAD"),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
        value = int(completed.stdout.strip())
    if value < 0:
        raise ValueError("SOURCE_DATE_EPOCH must be nonnegative")
    return value


def verify_native_binary(binary: Path, version: str) -> None:
    completed = subprocess.run(
        (str(binary), "version", "--json"),
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    reported = json.loads(completed.stdout)
    if reported.get("product") != "hyphae" or reported.get("engine_version") != version:
        raise RuntimeError(f"binary version does not match workspace: {reported!r}")


def archive_inputs(binary: Path, windows: bool) -> list[tuple[str, bytes, int]]:
    binary_name = "hyphae.exe" if windows else "hyphae"
    inputs = [(binary_name, binary.read_bytes(), 0o755)]
    for name in INCLUDED_DOCUMENTS:
        inputs.append((name, (ROOT / name).read_bytes(), 0o644))
    return inputs


def build_tar_gz(
    output: Path,
    root_name: str,
    inputs: list[tuple[str, bytes, int]],
    epoch: int,
) -> None:
    with output.open("xb") as raw:
        with gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=epoch, compresslevel=9) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT) as archive:
                directory = tarfile.TarInfo(f"{root_name}/")
                directory.type = tarfile.DIRTYPE
                directory.mode = 0o755
                directory.uid = directory.gid = 0
                directory.uname = directory.gname = ""
                directory.mtime = epoch
                archive.addfile(directory)
                for name, content, mode in inputs:
                    info = tarfile.TarInfo(f"{root_name}/{name}")
                    info.size = len(content)
                    info.mode = mode
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    info.mtime = epoch
                    archive.addfile(info, io.BytesIO(content))


def build_zip(
    output: Path,
    root_name: str,
    inputs: list[tuple[str, bytes, int]],
    epoch: int,
) -> None:
    minimum = int(dt.datetime(1980, 1, 1, tzinfo=dt.timezone.utc).timestamp())
    timestamp = dt.datetime.fromtimestamp(max(epoch, minimum), tz=dt.timezone.utc)
    zip_time = (timestamp.year, timestamp.month, timestamp.day, timestamp.hour, timestamp.minute, 0)
    with zipfile.ZipFile(output, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name, content, mode in inputs:
            info = zipfile.ZipInfo(f"{root_name}/{name}", date_time=zip_time)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = mode << 16
            archive.writestr(info, content, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)


def build_archive(binary: Path, target: str, output_dir: Path, epoch: int) -> Path:
    version = product_version()
    windows = "windows" in target
    suffix = ".zip" if windows else ".tar.gz"
    root_name = f"hyphae-{version}-{target}"
    output = output_dir / f"{root_name}{suffix}"
    output_dir.mkdir(parents=True, exist_ok=True)
    inputs = archive_inputs(binary, windows)
    if windows:
        build_zip(output, root_name, inputs, epoch)
    else:
        build_tar_gz(output, root_name, inputs, epoch)
    return output


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output-dir", type=Path, default=ROOT / "artifacts")
    arguments = parser.parse_args()
    binary = arguments.binary.resolve(strict=True)
    version = product_version()
    verify_native_binary(binary, version)
    archive = build_archive(binary, arguments.target, arguments.output_dir, source_date_epoch())
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    print(json.dumps({"archive": str(archive), "sha256": digest}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
