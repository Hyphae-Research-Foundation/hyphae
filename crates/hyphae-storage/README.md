<p align="center"><a href="https://hyphae.dev"><img alt="Hyphae" src="https://raw.githubusercontent.com/celiumsai/hyphae/main/.github/assets/hyphae-lockup.svg" width="320"></a></p>

# hyphae-storage

[![crates.io](https://img.shields.io/crates/v/hyphae-storage?logo=rust)](https://crates.io/crates/hyphae-storage)
[![docs.rs](https://img.shields.io/docsrs/hyphae-storage)](https://docs.rs/hyphae-storage)

Durable local storage primitives for [Hyphae](https://hyphae.dev): an
append-only checksummed and digest-chained log, atomic/idempotent mutation,
recovery, snapshots, compaction, backups, and verified restore.

```toml
[dependencies]
hyphae-storage = "1.2.0"
```

This crate owns the format-2 compatibility disk format. New applications
should embed the Native product facade in `hyphae-native-product`; format-2
embeddings use `hyphae-engine`. Use this lower-level crate only when directly
implementing the documented durable formats and ownership model.

`StorageLimits::default()` defines a complete finite recovery policy: one
shared 60-second cooperative deadline, 2 GiB active-log/snapshot files, 1 GiB
decoded replay/snapshot payload, one million directory entries/frames/
transactions/operations/lexical documents, and ten million lexical tokens.
The packaged CLI and server select that policy. Embedded applications use
`StorageEngine::open_with_limits`; its retained writer ceilings prevent
accepted appends from making the segment unreopenable under the same policy.
The published `StorageEngine::open` method retains its `0.2.0` compatibility
behavior without the new finite ceilings. `snapshot_with_limits` and
`compact_with_limits` accept explicit maintenance policy.

Backup layout validation fails on the first noncanonical entry, and snapshot
copy is fixed to the initial length of one opened regular source handle. Full
backup verification and restore retain their `0.2.0` behavior without a shared
end-to-end deadline; restore still composes legacy verification, reopen, and
snapshot paths. Filesystem calls and `sync_all` are not preemptible, so callers
that need an absolute elapsed-time ceiling must enforce it operationally.

Code is Apache-2.0; documentation is CC-BY-SA-4.0. Source and security
policy:
[`celiumsai/hyphae`](https://github.com/celiumsai/hyphae).
