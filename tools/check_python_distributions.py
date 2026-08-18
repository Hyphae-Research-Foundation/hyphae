#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Inspect Python wheel and sdist contents without installing them."""

from __future__ import annotations

import argparse
import json
import re
import tarfile
import zipfile
from email.parser import BytesParser
from pathlib import Path, PurePosixPath


EXPECTED_NAME = "hyphae-sdk"
IMPORT_ROOT = "hyphae_sdk/"


class DistributionValidationError(ValueError):
    """A built Python distribution is incomplete or contains unsafe paths."""


def fail(message: str) -> None:
    raise DistributionValidationError(message)


def safe_names(names: list[str]) -> None:
    for name in names:
        path = PurePosixPath(name)
        if path.is_absolute() or ".." in path.parts or "\\" in name:
            fail(f"distribution contains an unsafe member path: {name}")
        if name.endswith((".pyc", ".pyo")) or "__pycache__" in path.parts:
            fail(f"distribution contains generated Python bytecode: {name}")


def validate_metadata(encoded: bytes, version: str) -> None:
    metadata = BytesParser().parsebytes(encoded)
    if metadata.get("Name") != EXPECTED_NAME or metadata.get("Version") != version:
        fail("distribution metadata name/version differs from the release")
    if metadata.get("Requires-Python") != ">=3.11":
        fail("distribution metadata lost the supported Python floor")
    if metadata.get("License-Expression") != "Apache-2.0":
        fail("distribution metadata lost the SPDX license expression")
    if metadata.get_all("Requires-Dist", failobj=[]):
        fail("distribution unexpectedly declares runtime dependencies")


def validate_wheel(path: Path, version: str) -> int:
    expected = f"hyphae_sdk-{version}-py3-none-any.whl"
    if path.name != expected:
        fail(f"wheel filename must be {expected}")
    try:
        with zipfile.ZipFile(path) as archive:
            names = archive.namelist()
            safe_names(names)
            metadata = [name for name in names if name.endswith(".dist-info/METADATA")]
            if len(metadata) != 1:
                fail("wheel must contain exactly one METADATA file")
            required = {
                "hyphae_sdk/__init__.py",
                "hyphae_sdk/py.typed",
                "hyphae_sdk/v2/__init__.py",
            }
            if not required.issubset(names):
                fail("wheel omits the import package or typed marker")
            validate_metadata(archive.read(metadata[0]), version)
            return len(names)
    except zipfile.BadZipFile as error:
        raise DistributionValidationError("wheel is not a valid ZIP archive") from error


def validate_sdist(path: Path, version: str) -> int:
    expected = f"hyphae_sdk-{version}.tar.gz"
    if path.name != expected:
        fail(f"sdist filename must be {expected}")
    root = f"hyphae_sdk-{version}/"
    try:
        with tarfile.open(path, mode="r:gz") as archive:
            members = archive.getmembers()
            names = [member.name for member in members]
            safe_names(names)
            if any(member.issym() or member.islnk() or member.isdev() for member in members):
                fail("sdist must not contain links or device files")
            required = {
                f"{root}PKG-INFO",
                f"{root}README.md",
                f"{root}LICENSE",
                f"{root}LICENSE-DOCUMENTATION",
                f"{root}LICENSE-POLICY.md",
                f"{root}THIRD_PARTY_NOTICES.md",
                f"{root}src/hyphae_sdk/py.typed",
            }
            if not required.issubset(names):
                fail("sdist omits package metadata, licensing, README, or typed marker")
            metadata = archive.extractfile(f"{root}PKG-INFO")
            if metadata is None:
                fail("sdist PKG-INFO is not a regular file")
            validate_metadata(metadata.read(), version)
            return len(names)
    except tarfile.TarError as error:
        raise DistributionValidationError("sdist is not a valid gzip tar archive") from error


def validate(directory: Path, version: str) -> dict[str, object]:
    if re.fullmatch(r"\d+\.\d+\.\d+", version) is None:
        fail("expected version must be strict semver")
    files = sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and path.name != ".gitignore"
    )
    wheel = [path for path in files if path.suffix == ".whl"]
    sdist = [path for path in files if path.name.endswith(".tar.gz")]
    if len(files) != 2 or len(wheel) != 1 or len(sdist) != 1:
        fail("distribution directory must contain exactly one wheel and one sdist")
    wheel_members = validate_wheel(wheel[0], version)
    sdist_members = validate_sdist(sdist[0], version)
    return {
        "name": EXPECTED_NAME,
        "sdist_members": sdist_members,
        "status": "passed",
        "version": version,
        "wheel_members": wheel_members,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--expected-version", required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    print(json.dumps(validate(args.directory, args.expected_version), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
