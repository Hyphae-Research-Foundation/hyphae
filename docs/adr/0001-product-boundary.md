# ADR-0001: Hyphae is an autonomous data engine

- Status: Superseded in product scope by ADR-0020
- Date: 2026-07-14
- Owners: Celiums Solutions LLC

## Context

Previous Hyphae lines combined cognitive experiments, memory behavior,
benchmark corpora, and deployment assumptions. That shape cannot provide a
small, stable data product with optional integrations.

## Decision

Hyphae is a Rust-native, embeddable, verifiable data engine whose base
experience is one binary and one data directory. KV and structured query must
work without a network, LLM, embeddings, or external database.

Mycelium, Hyphae Network, Celiums Network, cognitive subsystems, commercial
web/consoles, SaaS billing or multitenancy, cloud operations, benchmark
corpora, and internal framework code are excluded. Semantic providers and
framework adapters are optional consumers of public versioned contracts.

Historical repositories remain frozen. Porting is file-by-file and requires
an entry in `docs/porting/ledger.md`; no repository is merged or cherry-picked
wholesale.

## Consequences

- Product correctness is testable without infrastructure credentials.
- Integrations cannot become hidden runtime dependencies.
- Historical algorithms may inform clean implementations but do not define
  the new architecture.
- The repository stays private until all `0.1.0` gates are green.

## Verification

The dependency graph, default feature set, offline conformance suite, source
ledger, and release checklist enforce this boundary.
