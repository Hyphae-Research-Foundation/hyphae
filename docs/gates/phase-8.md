# Phase 8 release-candidate gate

Status: historical `0.1.0` release-candidate evidence record. The candidate
was later tagged and published; any source change still invalidates closure
until the complete hosted matrix passes on the new exact commit.

## Implemented evidence

- portable atomic backup and verified new-directory restore, with bounded
  layout/manifest parsing; complete copy/verify/restore resource policy
  remains an explicit operator boundary documented for later hardening;
- `backup`, `backup-verify`, `restore`, and `doctor` in the single binary;
- immutable disk-format-1 fixture that rebuilds its omitted index and
  preserves idempotency;
- deterministic Linux/macOS/Windows archives containing one binary;
- pinned native release matrix for Linux x64, macOS x64/arm64, and Windows x64;
- installed-archive smoke on every native runner covering the documented
  offline KV, query, compaction, proof, backup/restore, and doctor flow;
- SHA-256 manifest, SPDX/CycloneDX SBOMs, SLSA v1 provenance, keyless Cosign
  signature/provenance/SBOM bundles, tag/version enforcement, and
  nonpublishing full manual dry runs;
- bounded parser fuzzing on a pinned nightly/cargo-fuzz pair;
- concurrent load and repeated kill/restart plus backup/restore soak gates;
- executable install, upgrade, backup, restore, doctor, and verification docs.

## Local commands

```bash
cargo test -p hyphae-storage -p hyphae-engine -p hyphae-cli --locked
python tools/generate_compatibility_fixture.py \
  --binary target/debug/hyphae \
  --check compatibility/v1/data-directory.json
python packaging/test_package.py
python tools/run_load_gate.py
python tools/run_soak_gate.py
cargo fuzz run document_decode -- -max_total_time=15 -max_len=1048576
cargo fuzz run proof_decode -- -max_total_time=15 -max_len=1048576
cargo fuzz run snapshot_verify -- -max_total_time=15 -max_len=1048576
```

The default local Linux load gate requires 256 durable writes at concurrency
8, elapsed time at most 90 seconds, nearest-rank p95 at most 2 seconds, exact
global output, and resident memory at most 512 MiB where `/proc` is available.
The soak gate performs four hard-kill cycles with 32 writes each, verifies all
128 records after every restart, then backs up, restores, diagnoses, reopens,
and recounts the result.

Local evidence was replayed on 2026-07-16:

- Rust 1.96 workspace tests passed natively on Windows and on Debian WSL2
  with an isolated Linux target; Windows Clippy and rustdoc passed with
  warnings denied, and the complete Rust 1.89 MSRV workspace passed;
- root and isolated fuzz Cargo locks passed RustSec and cargo-deny policy;
- checksum-verified `actionlint` 1.7.12 accepted every workflow;
- the common Rust, TypeScript, Python, CLI, and MCP conformance suite passed,
  as did the Astro, Next, and Vite adapters and host builds without Hyphae;
- native Windows load completed 256 durable writes at concurrency 8 in 0.829
  seconds with 47 ms nearest-rank p95; peak RSS is not exposed by this gate on
  Windows;
- soak recovered all 128 records through four hard kills and a restore;
- document, proof, and snapshot decoders completed 11,265,594, 2,437,845, and
  1,459,161 fuzz executions, respectively, without a crash;
- two real Windows x64 archives were byte-identical at SHA-256
  `2141297a77b19fa1967641f81f03fefdd35adb69653561d21421fc6b06d7f25b`,
  and the extracted binary reported the expected product/API/disk versions.

## Hosted closure evidence

- CI covers stable Rust 1.96, MSRV 1.89, Linux/macOS/Windows tests, public
  client conformance, optional integrations, contracts, compatibility, and
  deterministic packaging.
- Security and Dependency Review cover RustSec, license/source policy,
  manifest/lock consistency, registry checksums, npm integrity and audits,
  and full-history secret scanning.
- Fuzz and Stress cover the isolated fuzz lock, three bounded decoders,
  concurrent load, hard-kill recovery, and backup/restore soak.
- Release builds Linux x64, macOS x64/arm64, and Windows x64 archives; each
  extracted native binary executes the documented offline operational flow.
- Release assembly verifies checksums, SPDX/CycloneDX SBOMs, signatures,
  provenance, and attestations. Publication is skipped without an explicit
  matching tag.

At the time this gate was replayed, the repository was private and no release
tag had been created. The annotated `v0.1.0` tag later peeled to
`76b0cfdad90cf9e75d949a945c94a3badf0c6b59`, and the matching
[GitHub release](https://github.com/celiumsai/hyphae/releases/tag/v0.1.0) was
published. Those later outcomes do not replace the exact candidate evidence
recorded by the live pull-request checks.
