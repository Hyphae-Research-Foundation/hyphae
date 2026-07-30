# Backup and restore

Hyphae backups are local, portable, verified logical snapshots. They require
no server, database, cloud account, AI provider, or network connection.

## Create and verify

Stop any separate Hyphae process that owns the directory. The backup command
itself opens and exclusively locks the data directory:

```bash
hyphae backup --data-dir ./hyphae-data --out ./backups/hyphae-2026-07-15
hyphae backup-verify --backup ./backups/hyphae-2026-07-15
```

The output statuses must be `created` and `verified`. A backup contains exactly
`BACKUP.json` and `snapshot.hysnap`. Store the whole directory. Do not modify
either file, add files, or place the backup inside the live data directory.
Hyphae refuses to overwrite an existing destination.

The operator must exclusively control the destination parent while creation
or restore is running. Do not let another process create, rename, or replace
entries there concurrently.

Layout verification retains only whether the two canonical filenames were
seen and fails on the first unexpected entry; it does not accumulate directory
names. `BACKUP.json` is capped at 64 KiB. Snapshot copy opens one regular source
handle, captures its initial length, copies no more than that boundary, and
rejects a different copied or final length from the same handle.

Backup creation inherits the engine's bounded snapshot policy. In `0.2.1`, the
complete snapshot copy, standalone `backup-verify`, and multi-phase restore do
not yet share the new `StorageLimits` end-to-end deadline. The fixed copy
boundary prevents an append from extending one copy indefinitely, but it is
not an elapsed-time deadline. `backup-verify` retains the published absolute
limits of its `0.2.0` API, and restore still uses the legacy verification,
reopen, and snapshot paths. A single filesystem call, `sync_all`, or a very
large operator-selected backup cannot be preempted by Hyphae. Treat source
size, destination free space, and an external command timeout as explicit
operator controls; do not describe the whole backup/restore workflow as
resource-bounded.

## Restore

Restore always targets a path that does not yet exist:

```bash
hyphae restore \
  --backup ./backups/hyphae-2026-07-15 \
  --data-dir ./hyphae-restored
hyphae doctor --data-dir ./hyphae-restored
hyphae get --data-dir ./hyphae-restored --key alpha
```

The command verifies the source, reconstructs storage in a sibling staging
directory, rebuilds its embedded index, reopens it, and compares the checkpoint
before the final destination becomes visible. A corrupt source fails without
activating `./hyphae-restored`.

For disk format 2, the backup identity covers KV entries, vector-space
definitions, vectors, lexical-index definitions, and durable receipts. Restore
rebuilds Redb only from those logical sections. Validate at least one exact
and one lexical retrieval after restore when the application uses them.

Restore does not merge data and never modifies the source backup. To replace
an existing installation, restore to a new path, run `doctor`, stop the old
process, and switch the application to the verified new directory.

## Retention test

A backup is not proven merely because it was created. For every retention
cycle:

1. run `backup-verify` on the stored copy;
2. restore it to a disposable new directory;
3. run `doctor` and an application-specific read/query check;
   include exact/lexical/hybrid checks for retrieval-enabled data;
4. remove only the disposable restored directory after validation.

Keep at least one independently stored generation according to the operator's
recovery-point requirements. Encryption, media replication, and retention are
operator policies outside the Hyphae data format.
