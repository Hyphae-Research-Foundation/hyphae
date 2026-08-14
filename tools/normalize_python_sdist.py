#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Normalize a checked source distribution into reproducible gzip/tar bytes."""

from __future__ import annotations

import argparse
import copy
import gzip
import io
import os
import tarfile
from pathlib import Path, PurePosixPath


class SdistNormalizationError(ValueError):
    """The source distribution cannot be normalized safely."""


def normalize(path: Path, epoch: int) -> None:
    if epoch < 315_532_800:
        raise SdistNormalizationError(
            "source epoch must be representable by ZIP and tar tools"
        )
    entries: list[tuple[tarfile.TarInfo, bytes | None]] = []
    try:
        with tarfile.open(path, mode="r:gz") as source:
            for member in source.getmembers():
                posix = PurePosixPath(member.name)
                if (
                    posix.is_absolute()
                    or ".." in posix.parts
                    or "\\" in member.name
                    or member.issym()
                    or member.islnk()
                    or member.isdev()
                ):
                    raise SdistNormalizationError(
                        f"source distribution has unsafe member: {member.name}"
                    )
                stream = source.extractfile(member) if member.isfile() else None
                entries.append((member, stream.read() if stream is not None else None))
    except tarfile.TarError as error:
        raise SdistNormalizationError("source distribution is not a gzip tar archive") from error

    temporary = path.with_suffix(path.suffix + ".tmp")
    try:
        with temporary.open("wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as target:
                    for original, data in sorted(entries, key=lambda entry: entry[0].name):
                        member = copy.copy(original)
                        member.mtime = epoch
                        member.uid = 0
                        member.gid = 0
                        member.uname = ""
                        member.gname = ""
                        member.pax_headers = {}
                        target.addfile(member, io.BytesIO(data) if data is not None else None)
        os.replace(temporary, path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--path", type=Path, required=True)
    parser.add_argument("--source-date-epoch", type=int, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    normalize(args.path, args.source_date_epoch)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
