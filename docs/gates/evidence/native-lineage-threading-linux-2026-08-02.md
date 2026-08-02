# Native lineage threading on Linux

Date: 2026-08-02
Scope commit: `f4495646ca61aa07592c5a5f4a43de30c2d224a2`
Branch: `codex/native-lineage-threading`
Base: `6f8eef4d477132408f62eb7fea44451f8a3d2222`

## Environment

- Host: AWS EC2 `m6i.2xlarge`, `x86_64`
- Kernel: Linux `6.17.0-1019-aws`
- Repository: `/home/mario/celiumsai/hyphae`
- Filesystem: ext4 on `/dev/nvme0n1p1`
- Rust: `rustc 1.96.0`, `cargo 1.96.0`
- Execution boundary: direct Linux; WSL was not used

## Contract and implementation

The slice fixes one canonical 24-byte lineage identity:

- 16 RFC 9562 UUIDv7 bytes in network order;
- one nonzero history epoch as a little-endian `u64`.

Native checkpoints now publish `HYROOT03`, whose 216-byte header carries the
lineage after the page-generation and retention-floor fields. Native WAL
retention now publishes `HYWAR002`, whose 280 bytes carry the same lineage and
bind it into the checksum and final digest.

`HYROOT01`, `HYROOT02`, and `HYWAR001` remain byte-identical and decodeable by
standalone historical codecs. They cannot become authority under a native
`FORMAT` marker. Manifest and retention-anchor chains reject missing, mixed,
or marker-divergent lineage before recovery can select a compacted WAL base or
complete a pending destructive reset.

No dependency was added. UUIDv7 generation and validation remain Hyphae-owned
and use the existing BLAKE3 primitive for nondeterministic tail material.

## Red-to-green evidence

The initial exact runtime test failed as intended:

```text
test tests::immutable_checkpoint_round_trips_without_advancing_csn ... FAILED
left:  HYROOT01
right: HYROOT03
```

After implementation, the same test passed. Added coverage also proves:

- canonical lineage text and 24-byte binary goldens;
- `HYROOT03` and `HYWAR002` field offsets and round trips;
- rejection of invalid UUID version, variant, text, and zero epoch;
- rejection of mixed manifest and anchor chains;
- rejection of legacy manifests and anchors under a native marker;
- rejection when `FORMAT` has a different valid UUID from either authority
  family;
- unchanged decoding and golden bytes for historical formats; and
- existing checkpoint and WAL-retention interruption matrices under the new
  lineage-bearing formats.

## Gates executed

```text
cargo fmt --all -- --check
python3 tools/check_documentation.py
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 tools/check_native_dependencies.py
```

Results:

- formatting: pass;
- documentation: pass, 192 Markdown files and 12 JSON examples;
- Clippy: pass with warnings denied across the complete workspace;
- workspace tests: pass, 489 listed tests;
- native runtime tests: pass, 196 tests;
- native dependency gate: pass, 30 packages in closure, 19 external packages,
  zero forbidden packages, and zero native unsafe findings;
- `cargo deny check`: pass;
- `cargo geiger` native closure: pass.

## Gates still open

This evidence does not close G1. The following remain explicit:

- hosted cross-platform CI for the branch and pull request;
- a sanctioned history-divergence operation that increments the epoch;
- offline format-2 to native migration and promotion;
- an explicit migration path for pre-lineage experimental native directories;
- physical power-loss and filesystem-reordering validation;
- the complete crash/corruption corpus and latency benchmark required by the
  G1 exit gate.
