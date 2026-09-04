# Hyphae licensing policy

Copyright 2026 Celiums Solutions LLC.

Hyphae uses separate licenses by dominant purpose:

- Software and implementable normative specifications are licensed under the
  Apache License 2.0 (`Apache-2.0`). This includes source, tests, examples,
  tooling, configuration, machine-enforced data, schemas, public contracts,
  generated code, and normative specifications. The complete terms are in
  [`LICENSE`](LICENSE).
- Narrative documentation is licensed under the Creative Commons
  Attribution-ShareAlike 4.0 International license (`CC-BY-SA-4.0`). This
  includes prose-only guides, governance, ADRs, roadmaps, and README files
  unless a higher-precedence classification rule applies. The complete terms
  are in [`LICENSE-DOCUMENTATION`](LICENSE-DOCUMENTATION).

The Hyphae name, logos, product marks, and brand identity are not granted under
either license. Their use is governed by
[`TRADEMARKS.md`](https://github.com/Hyphae-Research-Foundation/hyphae/blob/main/TRADEMARKS.md).
Third-party components and materials retain their own licenses; see
[`THIRD_PARTY_NOTICES.md`](https://github.com/Hyphae-Research-Foundation/hyphae/blob/main/THIRD_PARTY_NOTICES.md),
dependency metadata, and
the source-porting ledger where applicable.

The current integration tree adopted this policy on 2026-08-16. Published
releases and immutable tags retain the terms under which they were published:

| Release | Software | Documentation |
|---|---|---|
| `v0.1.0` through `v1.0.1` | `Apache-2.0` | not separately specified |
| `v1.1.0` | `AGPL-3.0-only` | `CC-BY-SA-4.0` |
| current tree and `v1.2.0` onward | `Apache-2.0` | `Apache-2.0` for normative specifications; `CC-BY-SA-4.0` for narrative documentation |

Those prior grants are not revoked. Historical records may quote their exact
terms without becoming current first-party declarations.

Unless a contribution is explicitly accompanied by different accepted terms,
submitting software or a normative specification means licensing it under
`Apache-2.0`; submitting narrative documentation means licensing it under
`CC-BY-SA-4.0`. Inbound and outbound terms match. Contributions also require a
DCO 1.1 sign-off as described in the repository contribution policy; no CLA or
assignment is implied.
