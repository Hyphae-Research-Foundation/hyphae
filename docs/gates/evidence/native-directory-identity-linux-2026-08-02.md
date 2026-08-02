# Native directory identity and writer exclusion evidence — 2026-08-02

## Scope

This evidence binds the first executable slice of the native directory-format
contract to one Linux commit. It proves canonical `FORMAT` creation and
fail-closed validation, stable directory identity across reopen, and
lifetime-held operating-system writer exclusion through `LOCK`.

It does not prove offline format-2 promotion, promotion crash recovery,
lineage threading through root manifests or retention anchors, the final
native directory layout, filesystem power-loss behavior, or G1 completion.

## Source identity

- source commit:
  `5b8fee2b21190755513dc86b8fa415d2425e8162`;
- source tree:
  `bad3a6be364b473a0b50c1f08c4a76c983e19aac`;
- branch at capture: `codex/native-directory-format`;
- parent: `f8930869ca626b4130b932877c35fc43e5bbd6a4`; and
- worktree after the source commit: clean.

## Implemented behavior

`NativeDatabase::create` now:

- creates and exclusively locks `LOCK`;
- generates a lowercase, hyphenated UUIDv7 directory identifier without
  expanding the reviewed native dependency closure;
- writes exactly
  `hyphae-native-format=1 directory=<uuid-v7> epoch=1\n`;
- synchronizes the marker file and, on Unix, the containing directory; and
- holds the lock file descriptor for the complete database-handle lifetime.

`NativeDatabase::open` acquires the pre-existing lock before opening any page,
blob, WAL, or manifest authority. It consumes at most 129 bytes to enforce the
128-byte marker limit and fails closed for:

- missing `LOCK` or `FORMAT`;
- a live writer in the same process or another process;
- `FORMAT.pending`, or simultaneous `FORMAT` and `FORMAT.pending`;
- disk-format-2 markers and unsupported native versions;
- missing, duplicated, reordered, noncanonical, truncated, oversized, or
  trailing fields; and
- disk-format-2-only `log/` or `indexes/` entries mixed into a native
  directory.

A stale but unlocked `LOCK` file is reusable. The lock carries no state
authority; WAL and verified manifests remain the data authorities.

## Red-to-green record

The focused test was introduced first on parent `f8930869`. Its first
execution failed while reading `FORMAT` with operating-system error 2,
`No such file or directory`. After implementation, the same test passed.

The final runtime suite contains 193 passing unit tests, including:

- byte-for-byte golden marker encoding;
- malformed and oversized marker matrices;
- identity equality after close and reopen;
- explicit marker-family and pending-state errors;
- same-process double-open exclusion; and
- a child-process probe proving operating-system lock contention.

## Commands and results

All commands ran from `/home/mario/celiumsai/hyphae`:

```text
cargo fmt --all -- --check
cargo clippy -p hyphae-native-runtime \
  --all-targets --all-features --locked -- -D warnings
cargo test -p hyphae-native-runtime --locked
python3 tools/check_documentation.py
python3 tools/check_native_dependencies.py
git diff --check
```

| Check | Result |
|---|---:|
| Rustfmt | pass |
| Native runtime Clippy | pass, zero warnings |
| Native runtime tests | 193 passed, 0 failed |
| Documentation inventory | 191 Markdown, 12 JSON examples |
| Native dependency gate exit status | 0 |
| Reachable native-closure packages | 30 |
| Hyphae-owned workspace packages | 11 |
| Reviewed external primitives | 19 |
| Reachable forbidden engines | 0 |
| Native unsafe findings | 0 |
| Diff whitespace check | pass |

No external package was added. A draft using the `uuid` crate's generation
feature was discarded because its multi-target metadata closure introduced a
second `syn` version and failed the exact native dependency gate. UUIDv7
generation and validation therefore use Hyphae code plus the already reviewed
BLAKE3 primitive.

## Environment

- EC2 host reached directly over SSH as `mario@10.77.10.10`;
- Linux `6.17.0-1019-aws`, x86_64;
- repository filesystem `/dev/nvme0n1p1`, ext4;
- Rust `1.96.0 (ac68faa20 2026-05-25)`;
- Cargo `1.96.0 (30a34c682 2026-05-25)`;
- cargo-deny `0.20.2`;
- cargo-geiger `0.13.0`; and
- Python `3.12.3`.

## Remaining boundaries

- Implement the offline importer and `FORMAT.pending` promotion protocol.
- Test interruption before rename, after rename before parent sync, and after
  parent sync.
- Version and implement lineage identity in root manifests and retention
  anchors, then prove offline divergence.
- Replace the remaining experimental root-file layout with the contracted
  native directory families.
- Run the full workspace and cross-platform hosted gates on the pull request.
- Add physical-durability and power-loss evidence on stable hardware.

This evidence advances one G1 substrate boundary. It closes no phase and does
not authorize format-2 migration.
