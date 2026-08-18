<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native backup format v1

`NativeDatabase` backup is an offline, local operation. It uses the native
directory's existing exclusive lock and creates a synchronized root checkpoint
before copying any file. Backup, verify, and restore perform no network or
external-provider operation.

## Public API

- `NativeDatabase::backup(destination, limits)` creates a new backup.
- `verify_native_backup(path)` verifies one backup with default bounds.
- `verify_native_backup_with_limits(path, limits)` verifies with explicit bounds.
- `restore_native_backup(backup, destination)` restores with default bounds.
- `restore_native_backup_with_limits(backup, destination, limits)` restores with
  explicit bounds.

Destinations must not exist. A backup destination cannot be inside the live
data directory, and a restore destination cannot be inside the backup.

## Layout

A backup root contains exactly:

```text
NATIVE_BACKUP.json
data/
```

`data/` is an exact directory inventory of the locked native data directory,
including empty directories. `NATIVE_BACKUP.json` records the checkpoint visible
CSN and manifest digest, the sorted canonical directory inventory, and every
regular file's relative UTF-8 path, byte length, and BLAKE3 digest.

Absolute paths, `.` and `..` components, non-UTF-8 paths, duplicate or unsorted
manifest paths, symlinks, special files, missing entries, and additional entries
are invalid. Limits bound manifest bytes, path bytes, file count, directory
count, and total file bytes. File copying and hashing use fixed-size buffers.

## Publication

Creation copies into a sibling staging directory, synchronizes files and
directories, verifies the staged backup offline, and renames it to the requested
new destination. Restore verifies the source, copies only the verified inventory
to sibling staging, opens the staged directory through `NativeDatabase::open`,
and compares its logical visible CSN and current root-manifest digest with the
backup checkpoint. Only then is staging atomically renamed to the requested new
data path.

Any failure removes staging best-effort and never activates a partial requested
destination. Atomic rename and durable parent-directory synchronization require
the source staging and destination to share a filesystem and the platform to
support directory synchronization.
