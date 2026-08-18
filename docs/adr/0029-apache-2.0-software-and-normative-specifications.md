# ADR-0029: Apache-2.0 software and normative specifications

- Status: Accepted and effective
- Date: 2026-08-15
- Owners: Celiums Solutions LLC
- Supersedes: ADR-0025 for the current integration tree and `1.2.0` onward;
  ADR-0025 remains the immutable authority for `1.1.0`

## Context

ADR-0025 selected `AGPL-3.0-only` for software and `CC-BY-SA-4.0` for
documentation after `v1.0.1`. Hyphae is nevertheless designed as an offline,
single-process, embeddable engine rather than a hosted service boundary. The
AGPL network condition therefore does not protect its primary product surface,
while strong copyleft creates adoption friction for embedding applications.

The project first accepted a target direction for `1.2.0` while copyright and
relicensing authority, prior commitments, dependency compatibility,
contribution governance, and counsel approval remained preflight conditions.
Those conditions were subsequently accepted through source-bound evidence and
the interactive owner attestation recorded for the effective transition.

## Decision

### Immutable release history

Published grants are not revoked or rewritten:

| Release range | Software | Documentation |
|---|---|---|
| `0.1.0` through `1.0.1` | `Apache-2.0` | `not-separately-specified` |
| `1.1.0` | `AGPL-3.0-only` | `CC-BY-SA-4.0` |
| current integration tree and `1.2.0` onward | `Apache-2.0` | `Apache-2.0` for normative specifications and `CC-BY-SA-4.0` for narrative documentation |

The tags in the first row contain the root Apache License 2.0 text and declare
`Apache-2.0` in the root `README.md` and `Cargo.toml`. They do not contain a
`LICENSE-DOCUMENTATION` or another separate documentation grant, so this ADR
does not infer `CC-BY-4.0` or any other documentation license for them.

The last row became effective in the current integration tree at
`2026-08-16T13:15:26Z`. ADR-0025 and the published `v1.1.0` artifacts remain
historically accurate and are not retroactively changed.

### Exact categories

For the `1.2.0` target, **software** means executable source, tests, examples,
build and release tooling, operational and build configuration,
machine-enforced data and fixtures, JSON Schemas, OpenAPI documents, generated
models, and machine-readable contracts. It uses `Apache-2.0`.

An **implementable normative specification** defines behavior that an
independent implementation must reproduce: public APIs and protocols, durable
formats, query and retrieval semantics, proof formats, and bounded performance
contracts. Normative specifications use `Apache-2.0`. This category includes
`contracts/**`, packaged contract copies, and the normative roots `docs/api/`,
`docs/native/`, `docs/storage/`, `docs/query/`, `docs/retrieval/`,
`docs/provenance/`, and `docs/performance/`.
The embedded product-error Markdown contract is an exact machine-contract
exception; it is normative rather than a narrative file in a code root.
`docs/gates/native-local-phase-1.md` is also an exact exception because it is
the normative authority for gate outcomes, and
`docs/security/native-access-control-threat-model.md` is an exact exception
because it declares a normative design contract and required authorization
invariants.

**Narrative documentation** is prose-only explanation or governance. It uses
`CC-BY-SA-4.0`. This includes ADRs, roadmaps, architecture discussion, how-to
and operator guidance, gates and prose evidence, release prose and receipts,
security narratives, and README files, including README files inside code
distribution roots. The baseline and server threat models remain narrative:
they explain boundaries and reference separate normative contracts rather than
declaring themselves normative authorities. Machine-enforced JSON data remains
software under the mixed-file rule, including evidence, fixtures, and factual
JSON release receipts stored below narrative roots. Markdown release receipts
remain narrative. Historical prose and receipts may accurately quote AGPL or
other historical terms without changing the license of the document itself.

The root `NOTICE` is Apache legal material distributed with the software and
therefore uses the software category. It remains included in release archives.

The machine-readable authority for exact path rules, precedence, generated
copies, and current blocker state is
[`config/relicensing-1.2.0-classification.json`](../../config/relicensing-1.2.0-classification.json).
The lowest unique numeric priority wins. An unclassified repository path or a
priority tie is an error; difficult exceptions must be exact rather than
hidden in a broad heuristic.

### Mixed files and generated copies

Dominant purpose controls a mixed file. Executable or machine-enforced content
is software and therefore Apache-2.0. Implementable normative prose is a
normative specification and therefore Apache-2.0. Prose-only material is
narrative and therefore CC-BY-SA-4.0. A README is narrative unless an explicit,
higher-precedence rule identifies it as normative.

A generated or packaged copy inherits the category and target license of its
source contract. Checked-in JSON Schema, OpenAPI, and generated model copies
therefore use Apache-2.0 even when distributed inside a package whose README is
CC-BY-SA-4.0.

### Trademark and third-party boundary

Neither selected license grants rights in the Hyphae names, marks, logos,
domains, or visual identity. Those remain governed by
[`TRADEMARKS.md`](../../TRADEMARKS.md); reserved brand assets are outside both
project grants. Third-party works retain their original terms. Dependency and
license inventories, provenance records, historical release records, and the
porting ledger are factual evidence and must not be mechanically rewritten as
first-party license declarations.

### Effective transition

The transition became effective in the current integration tree after every
required category was accepted. The owner attestation records authority over
all first-party contributions, written counsel approval, absence of
incompatible commitments, coverage of the 173 `ec2-user` commits, and owner
acceptance of the Apache section 6 boundary with existing `TRADEMARKS.md`. It
does not falsely claim that counsel's confidential approval expressly covered
section 6 or the trademark policy, and no public locator or hash is claimed.

The twelve Dependabot commits were reviewed individually and accepted as
mechanical first-party-maintained changes. Exact source bindings and review
summaries are in the transition evidence. New contributions use
inbound-equals-outbound terms and DCO 1.1 without a CLA or assignment.

The transition stops if any of the following remains open or disputed:

- qualified counsel approval;
- complete copyright-holder and written relicensing authority;
- prior contractual, grant, program, or public commitments;
- exact-SHA dependency and license compatibility;
- this specification classification or generated-copy inheritance;
- counsel-approved contribution governance; or
- the ability to change every first-party authority coherently in one commit.

The accepted source-bound evidence and the current integration-tree digest are
recorded in `docs/gates/evidence/relicensing-1.2.0-transition.json`. A checker
failure reopens the transition rather than permitting a partial declaration.

## Consequences

- Apache-2.0 permits proprietary and commercial forks and provides an explicit
  patent grant; it does not provide an AGPL network-source condition.
- Implementers may use one permissive license for software and normative
  specifications without applying ShareAlike to an implementation.
- Narrative adaptations retain attribution and ShareAlike obligations.
- Current first-party checks require Apache-2.0 coherently across SPDX,
  manifests, package metadata, SBOM conclusions, and license texts.
- ADR-0025 remains in decision history and accurately governs `1.1.0`.

## Verification

`tools/check_relicensing_preflight.py` strictly validates the effective
classification contract, accepted evidence, all tracked or pending repository
paths, pinned historical tags, generated-copy inheritance, and the transition
tree digest. `tools/check_license_policy.py` verifies the expected license for
every classified first-party path and rejects stale effective AGPL declarations
outside its explicit historical-evidence allowlist.
