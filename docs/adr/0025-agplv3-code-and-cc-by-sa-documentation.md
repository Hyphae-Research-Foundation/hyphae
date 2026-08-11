# ADR-0025: AGPLv3 code and CC BY-SA documentation

- Status: Accepted
- Date: 2026-08-10
- Owners: Celiums Solutions LLC
- Supersedes: ADR-0002 license selection; its provenance controls remain active

## Context

Hyphae's software and architecture have become independently valuable enough
that permissive redistribution no longer matches the project's intent. The
repository must preserve the freedom to inspect, run, and improve the engine
while requiring distributed derivatives and remotely operated modified
versions to make their corresponding source available under the same software
license. Documentation needs equivalent attribution and share-alike terms
without applying a documentation license to executable code.

Published Apache-2.0 releases already carry irrevocable grants. A new license
policy can govern this tree and future releases but cannot revoke those prior
permissions. Trademarks and third-party materials also require separate scope.

## Decision

Hyphae software uses `AGPL-3.0-only`. Software includes Rust, Python,
TypeScript, JavaScript, build and release tooling, tests, examples,
machine-readable contracts and schemas, generated code, and operational build
configuration. Authored source files use the corresponding SPDX identifier,
generated distributions carry the license bundle, and package metadata exposes
the same exact expression.

Repository documentation uses `CC-BY-SA-4.0`. This includes Markdown prose,
ADRs, diagrams, and documentation-only illustrations unless a file declares a
different license. The Hyphae name, logos, product marks, and visual identity
remain outside both grants and are governed by `TRADEMARKS.md`. Third-party
materials retain their original terms.

The root `LICENSE` contains the canonical AGPLv3 text,
`LICENSE-DOCUMENTATION` contains the canonical CC BY-SA 4.0 legal code, and
`LICENSE-POLICY.md` is the scope authority. Releases through `v1.0.1` remain
under their original Apache-2.0 terms; this decision governs the repository
after adoption and future releases.

For future release SBOMs, tracked first-party package manifests are the
authority for Hyphae package name, version, and license. The release pipeline
may conclude `AGPL-3.0-only` only after a discovered first-party artifact
matches that exact manifest authority and its applicable local lock/source
evidence. Because the pinned scanner does not catalog the Python source
manifest, the normalizer records that one package as an explicit
manifest-backed supplement rather than presenting it as scanner discovery.
The conclusion step does not rewrite third-party artifacts or their observed
licenses. SPDX and CycloneDX are derived from the same normalized inventory,
and release verification requires equality with the complete lock-derived plus
Python-manifest first-party inventory and exact multiset parity of first-party
`(name, version, purl)` identities between both formats.

## Consequences

- Distributed modified software must satisfy AGPLv3 source and license duties.
- Section 13 requires an operator of a remotely accessible modified Hyphae to
  offer the Corresponding Source of that running modified version to its users.
- Documentation adaptations must preserve attribution and share-alike terms.
- Package archives must ship both license texts and the scope policy.
- Dependency and historical-source licenses remain evidence, not project
  license declarations, and must not be mechanically rewritten.
- Contributors must have authority to offer their contribution under the
  applicable project license.

## Verification

CI verifies the canonical license-text digests, package metadata, OpenAPI
metadata, authored source SPDX identifiers, generated-client headers, archive
contents, absence of stale project-license declarations, manifest-backed
first-party SBOM license conclusions, and complete cross-format first-party
identity parity for the normalized inventory. Historical release records,
dependency evidence, and the porting ledger are explicit exceptions because
they describe third-party or prior-version facts.
