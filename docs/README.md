# Hyphae documentation

This directory is the canonical human documentation for shipped Hyphae
behavior.
Public wire behavior is normative in `contracts/`; durable encodings and
reference semantics are normative in the versioned specification documents
linked below. Tests and checked-in compatibility fixtures enforce both.

## Announcements

- [DEV launch article for Hyphae 0.1.0](announcements/devto-hyphae-0.1.0.md)
  ([published article](https://dev.to/terrizoaguimor/i-built-a-rust-data-engine-that-can-prove-its-own-results-12m7))

## Start here

- [Product capabilities and limits](product/capabilities.md)
- [Local quickstart](quickstart.md)
- [CLI reference](cli/reference.md)
- [Configuration reference](configuration.md)
- [Data model](concepts/data-model.md)
- [Architecture overview](architecture/overview.md)
- [Native local ecosystem target](architecture/native-local-ecosystem.md)

## Build and embed

- [Embed Hyphae in Rust](embedding/rust.md)
- [HTTP API v1](api/v1.md)
- [Stable API error codes](api/error-codes-v1.md)
- [Public clients](clients/v1.md)
- [Optional framework adapters](integrations/optional-adapters.md)
- [MCP adapter](../mcp/README.md)
- [Executable examples](../examples/README.md)

## Operate safely

- [Install, upgrade, and migrate](operations/install-upgrade.md)
- [Back up and restore](operations/backup-restore.md)
- [Run doctor and diagnose recovery](operations/doctor.md)
- [Troubleshoot common failures](operations/troubleshooting.md)
- [Verify a release](release/verification.md)
- [0.2.1 publication receipt](release/receipts/0.2.1.md)
- [Publish the Rust crates](release/crates-io.md)
- [Compatibility and versioning](compatibility/versioning.md)

## Understand correctness

- [Native canonical types v1](native/types-v1.md)
- [Native page, row, and blob format v1](native/page-row-blob-format-v1.md)
- [Native B+tree format v1](native/btree-format-v1.md)
- [Native root manifest and checkpoint format v1](native/root-manifest-checkpoint-v1.md)
- [Native WAL format v1](native/wal-format-v1.md)
- [Native MVCC and commit semantics v1](native/mvcc-commit-v1.md)
- [Native catalog v1](native/catalog-v1.md)
- [Hyphae SQL semantics v1](native/sql-semantics-v1.md)
- [Native structure-engine semantics v1](native/structures-semantics-v1.md)
- [Native search-engine semantics v1](native/search-semantics-v1.md)
- [Native ANN semantics v1](native/ann-semantics-v1.md)
- [Native local protocol v1](native/local-protocol-v1.md)
- [Native clean-room and dependency inventory](native/dependency-inventory.md)
- [Structured query semantics v1](query/reference-semantics-v1.md)
- [Exact retrieval semantics v1](retrieval/reference-semantics-v1.md)
- [Durable exact retrieval semantics v2](retrieval/exact-reference-semantics-v2.md)
- [Lexical retrieval semantics v1](retrieval/lexical-reference-semantics-v1.md)
- [Hybrid retrieval semantics v1](retrieval/hybrid-reference-semantics-v1.md)
- [0.2 retrieval benchmark methodology](performance/retrieval-benchmark-0.2.md)
- [Microsecond-first performance contract](performance/microsecond-first.md)
- [Result proof format v1](provenance/result-proof-v1.md)
- [Retrieval proof format v1](provenance/retrieval-proof-v1.md)
- [Baseline threat model](security/threat-model.md)
- [Server threat model](security/server-threat-model.md)

### Durable formats

- [Data directory and architecture](architecture/overview.md#data-directory)
- [Log format v1](storage/log-format-v1.md)
- [Mutation format v1](storage/mutation-format-v1.md)
- [Durable vector record format v1](storage/vector-record-format-v1.md)
- [Document format v1](storage/document-format-v1.md)
- [Snapshot format v1](storage/snapshot-format-v1.md)
- [Manifest format v1](storage/manifest-format-v1.md)
- [Compaction protocol v1](storage/compaction-v1.md)

## Decisions and governance

- [Roadmap](roadmap.md)
- [0.2 execution roadmap](roadmap-0.2.md)
- [Porting ledger](porting/ledger.md)
- [Development guide](development.md)
- [0.1.0 release gate](gates/0.1.0.md)
- [0.2.0 release gate](gates/0.2.0.md)
- [0.2.1 release gate](gates/0.2.1.md)
- [Native local ecosystem phase-1 gate](gates/native-local-phase-1.md)
- [Native phase-1 kernel evidence — 2026-08-01](gates/evidence/native-phase1-kernel-2026-08-01.md)
- [Native row, B+tree, and checkpoint evidence — 2026-08-01](gates/evidence/native-row-tree-checkpoint-2026-08-01.md)
- [Native blobs, mutations, and conflicts evidence — 2026-08-01](gates/evidence/native-blobs-mutations-conflicts-2026-08-01.md)
- [Native borrowed point-read evidence — 2026-08-01](gates/evidence/native-borrowed-point-read-2026-08-01.md)
- [Native relational version-chain evidence — 2026-08-01](gates/evidence/native-relational-version-chains-2026-08-01.md)
- [Native optimistic-writer evidence — 2026-08-01](gates/evidence/native-optimistic-writers-2026-08-01.md)
- [Native structure B+tree evidence — 2026-08-01](gates/evidence/native-structure-btree-2026-08-01.md)
- [Native scalar structure mutation evidence — 2026-08-01](gates/evidence/native-scalar-structure-mutations-2026-08-01.md)
- [Native hash structure evidence — 2026-08-01](gates/evidence/native-hash-structure-2026-08-01.md)
- [Native set structure evidence — 2026-08-01](gates/evidence/native-set-structure-2026-08-01.md)
- [Native chunked-list evidence — 2026-08-01](gates/evidence/native-list-2026-08-01.md)
- [Native inverted-search evidence — 2026-08-01](gates/evidence/native-inverted-search-2026-08-01.md)
- [Native ANN kernel evidence — 2026-08-01](gates/evidence/native-ann-kernel-2026-08-01.md)
- [Native ANN durability evidence — 2026-08-01](gates/evidence/native-ann-durability-2026-08-01.md)
- [Native canonical type-codec evidence — 2026-08-01](gates/evidence/native-type-codecs-2026-08-01.md)
- [Native catalog-definition evidence — 2026-08-01](gates/evidence/native-catalog-definitions-2026-08-01.md)
- [Native typed SQL-row evidence — 2026-08-01](gates/evidence/native-typed-sql-rows-2026-08-01.md)
- [Native secondary-index evidence — 2026-08-01](gates/evidence/native-secondary-indexes-2026-08-01.md)
- [Native direct secondary-index execution evidence — 2026-08-01](gates/evidence/native-direct-secondary-index-2026-08-01.md)
- [Native typed indexed-mutation evidence — 2026-08-01](gates/evidence/native-typed-indexed-mutations-2026-08-01.md)
- [Native bounded relational-scan evidence — 2026-08-01](gates/evidence/native-bounded-relational-scan-2026-08-01.md)
- [Native primary-key range evidence — 2026-08-01](gates/evidence/native-primary-key-ranges-2026-08-01.md)
- [Native SQL residual-filter evidence — 2026-08-01](gates/evidence/native-sql-residual-filters-2026-08-01.md)
- [Native SQL scalar-literal evidence — 2026-08-01](gates/evidence/native-sql-scalar-literals-2026-08-01.md)
- [Native SQL mutation-literal evidence — 2026-08-01](gates/evidence/native-sql-mutation-literals-2026-08-01.md)
- [Native indexed inner-join evidence — 2026-08-01](gates/evidence/native-indexed-inner-join-2026-08-01.md)
- [Native bounded inner-join evidence — 2026-08-01](gates/evidence/native-bounded-inner-join-2026-08-01.md)
- [Native secondary-index inner-join evidence — 2026-08-01](gates/evidence/native-secondary-inner-join-2026-08-01.md)
- [Native right secondary-index inner-join evidence — 2026-08-01](gates/evidence/native-right-secondary-inner-join-2026-08-01.md)
- [Native composite inner-join evidence — 2026-08-01](gates/evidence/native-composite-inner-join-2026-08-01.md)
- [0.2 local evidence catalog](gates/evidence/README.md)
- [0.2 Gate 0 repository audit and baseline](gates/0.2-gate-0.md)
- Phase evidence: [2](gates/phase-2.md), [3](gates/phase-3.md),
  [4](gates/phase-4.md), [5](gates/phase-5.md), [6](gates/phase-6.md),
  [7](gates/phase-7.md), and [8](gates/phase-8.md)

### Architecture decision records

- [ADR template](adr/0000-template.md)
- [ADR-0001: Product boundary (superseded in scope)](adr/0001-product-boundary.md)
- [ADR-0002: License and porting](adr/0002-license-and-porting.md)
- [ADR-0003: Public contracts](adr/0003-public-contracts.md)
- [ADR-0004: Durable authority](adr/0004-durable-authority.md)
- [ADR-0005: Workspace boundaries](adr/0005-workspace-boundaries.md)
- [ADR-0006: One binary and one data directory](adr/0006-one-binary-one-data-directory.md)
- [ADR-0007: Query and retrieval correctness](adr/0007-query-retrieval-correctness.md)
- [ADR-0008: Snapshot-witness result proofs](adr/0008-snapshot-witness-result-proofs.md)
- [ADR-0009: Secure loopback-first API](adr/0009-loopback-first-secure-v1-api.md)
- [ADR-0010: Generated clients and MCP stdio](adr/0010-generated-clients-and-mcp-stdio.md)
- [ADR-0011: Optional host-owned integrations](adr/0011-optional-host-owned-integrations.md)
- [ADR-0012: Portable recovery and verifiable releases](adr/0012-portable-recovery-and-verifiable-releases.md)
- [ADR-0013: Durable named vector records](adr/0013-durable-vector-records.md)
- [ADR-0014: Durable exact retrieval](adr/0014-exact-durable-retrieval.md)
- [ADR-0015: Canonical retrieval scoring](adr/0015-canonical-retrieval-scoring.md)
- [ADR-0016: Retrieval proof format](adr/0016-retrieval-proof-format.md)
- [ADR-0017: Provider-free lexical retrieval](adr/0017-provider-free-lexical-retrieval.md)
- [ADR-0018: Deterministic hybrid retrieval](adr/0018-deterministic-hybrid-retrieval.md)
- [ADR-0019: Finite resource policies](adr/0019-finite-resource-policies.md)
- [ADR-0020: Native local data ecosystem](adr/0020-native-local-data-ecosystem.md)

## Documentation contract

Repository documentation is written in English. Normative specifications and
product guides must describe shipped behavior, not intent; roadmaps and
explicitly marked planning gates may describe ordered future work. `python
tools/check_documentation.py --binary target/debug/hyphae` verifies local
links, this index, JSON examples, and the top-level CLI command inventory.
`python tools/run_documentation_examples.py --binary target/debug/hyphae`
executes the maintained HTTP examples. Public behavior changes must update the
relevant contract, guide, example, and gate evidence in the same change.
