# Native access-control threat model v1

Status: normative design contract; implementation evidence pending

This document extends the baseline and server threat models for durable
principals, roles, scoped grants, and API keys. It does not weaken the existing
data-integrity, proof, bounded-resource, loopback-first, or TLS boundaries.

## Protected assets

- User records and search content.
- Catalog definitions and stable object identities.
- WAL, checkpoints, backups, restores, and proof anchors.
- Principal, role, assignment, key-verifier, and audit state.
- API-key secrets while they exist in caller memory or a restricted output
  file.
- Availability of the single product owner and its finite resource budgets.

## Trust boundaries

### Local operating system

Hyphae trusts the operating system to enforce process isolation, peer
credentials, file ownership, ACLs, and the exclusive data-directory lock. A
local root or administrator that controls the process and complete data
directory is outside the protection boundary.

The native daemon trusts kernel-supplied UID/GID/PID or Windows peer identity.
A caller-provided client name is diagnostic metadata and never authority.

### Remote transport

HTTP headers and all request bytes are untrusted. API keys authenticate a
principal but do not encrypt the transport. Any non-loopback deployment still
requires a trusted TLS-terminating boundary. Proxies may authenticate an outer
identity, but they cannot widen the product permissions resolved by Hyphae.

### Embedded callers

An embedding application may open a trusted local administrative session, but
ordinary embedded clients use the same principal and authorization evaluator
as transport clients. Constructing a public request cannot supply an arbitrary
permission bitset.

### Durable state

The access-control catalog becomes trusted only after normal directory, WAL,
manifest, catalog, and root verification. Unknown versions, malformed grants,
duplicate IDs, dangling assignments, invalid scopes, or a missing owner fail
closed before readiness.

## Attacker capabilities

An attacker may:

- send malformed, oversized, concurrent, replayed, or deliberately expensive
  authentication and product requests;
- know a key's public ID, principal name, role name, or catalog display name;
- possess one valid key with limited roles or scopes;
- reuse a revoked, expired, superseded, or legacy credential;
- race role/key changes with prepared statements, sessions, and explicit
  transactions;
- copy an old backup or access-control snapshot;
- observe process arguments, logs, error bodies, telemetry, and TUI rendering
  available to its operating-system account; or
- disconnect at any mutation boundary.

## Threats and required controls

| Threat | Required control | Evidence required before implementation closure |
|---|---|---|
| Key disclosure through argv, logs, errors, telemetry, TUI, or list APIs | Secrets never enter argv; one-time restricted-file output; redacted formatting and fixtures | Cross-surface secret canaries and log/output scans |
| Key guessing or verifier theft | 256 random secret bits; canonical parser; domain-separated verifier; constant-time compare; no password-derived keys | Boundary vectors, collision tests, comparison tests, dependency audit |
| Principal or key enumeration | Missing, malformed, revoked, expired, and wrong credentials expose the same bounded unauthorized result | HTTP/local/MCP equality fixtures |
| Privilege escalation through a key restriction | Effective permissions and scopes are intersections with current principal assignments; keys never add authority | Property/model tests over random role/key combinations |
| Scope escape by catalog rename | Scopes bind stable `ObjectId`, not names; SQL authorizes bound objects | Rename and prepared-plan conformance |
| Scope escape through SQL joins, views, links, or subqueries | Binder produces the complete referenced-object set before execution; every object must be admitted | Multi-object SQL and cross-engine negative tests |
| Stale session after revocation or role removal | Per-operation authorization epoch check and expiry check | Long-lived HTTP, UDS, named-pipe, and embedded session tests |
| Commit after authority loss | Stage and commit checks; authority loss before commit causes definite rollback | Concurrent revoke/commit histories and crash recovery |
| Prepared handle used after scope change | Prepared metadata retains stable objects and required permissions; execution reauthorizes | Prepare, revoke/re-scope, execute negatives |
| Restore delegated through backup authority | `backup.create`, `backup.verify`, and `restore` are independent; restore is built-in owner/admin only | Operation/role matrix checker and live negative tests |
| Monitoring key mutates state | `observe` excludes checkpoint, doctor, maintenance, security, and data mutation | Operator/auditor exhaustive denials |
| Proof wrapper bypasses underlying authorization | Proof generation requires `proof.generate` plus every permission and scope of the wrapped operation | Recursive operation tests |
| Key rotation creates an unbounded overlap | One explicit old/new pair, finite deadline, atomic publication, and exact terminal status | Rotation interruption and expiry tests |
| Authentication-failure write amplification | Failures use bounded telemetry and rate controls; no durable event per untrusted attempt | Saturation and disk-growth gate |
| Audit event forges or leaks a secret | Events are product-generated, append under WAL/CSN, and contain public IDs/redacted fields only | Crash, replay, redaction, and canonical-codec tests |
| Offline bootstrap or recovery while server is active | Exclusive directory lock and no remote endpoint; output path must be a new restricted regular file | Process exclusion and filesystem failure tests |
| Rollback to an older credential database | Normal manifest/WAL lineage verification plus caller-pinned external anchors where rollback resistance is required | Backup/restore lineage and pinned-anchor tests |
| Local client spoofs another principal | Before bootstrap, kernel peer identity is trusted-local compatibility metadata; after bootstrap only a durable API key is authority and client-supplied identity remains diagnostic | UDS/named-pipe adversarial handshake and automatic-cutover tests |
| Resource exhaustion through roles, grants, keys, or audit queries | Finite counts, name bytes, result pages, work budgets, deadlines, and admission | Exact-limit/one-past-limit tests |
| Error details disclose security state | Stable authorization errors omit principal existence, roles, scopes, key IDs, SQL text, paths, and secrets | Golden errors and unknown-field redaction tests |

## Authorization invariants

1. Every externally reachable operation has a nonempty permission rule.
2. Unknown operations, permissions, roles, scopes, key versions, and durable
   record versions fail closed.
3. Built-in roles are immutable and cannot be shadowed by custom names.
4. A principal with no active role assignments has no product authority.
5. A key can only reduce the authority of its current principal.
6. Role and scope decisions use stable identities rather than mutable names.
7. Revocation, expiry, disablement, and ownership recovery take effect on the
   next operation in every existing session.
8. No restore or ownership operation is object-scoped or implied by a broader
   backup/administration label.
9. Proof generation never grants access to the wrapped operation.
10. Authorization failure publishes no partial logical result or mutation.

## Bootstrap and recovery risks

Bootstrap is permitted exactly once when no principal records exist. Failure
after durable principal creation but before key-file publication must not leave
an unknowable active credential: the transaction either publishes both the
verifier and a definite output receipt, or rolls back. The key file is created
with exclusive creation and restrictive permissions before the transaction is
acknowledged.

Recovery assumes operating-system control of the offline directory. It creates
one replacement owner credential, revokes prior owner credentials, increments
the authorization epoch, and records the event. It does not delete other
principals or silently grant them owner. A failed recovery leaves either the
complete old authority or the complete new authority.

## Legacy bearer migration risks

The legacy bearer is one explicitly labelled compatibility credential, not a
role system. Migration never hashes an arbitrary legacy token into the new key
format or claims that it has a key ID. It issues a new canonical owner key and
requires an explicit cutover and revocation. Missing legacy configuration does
not reactivate a previously revoked credential.

The current foundation does not yet activate that compatibility credential.
Bootstrapped listeners reject a fixed bearer until migration, synthetic-owner
binding, and revocation ship as one verified slice; this fail-closed interim
does not consume the one-minor compatibility window.

## Residual and excluded threats

- Hyphae does not protect secrets from local root, a debugger attached with
  equivalent privilege, memory dumping, or a compromised TLS proxy.
- Durable local audit records are not an independent transparency log. An
  attacker controlling the complete directory and all trusted anchors can
  rewrite both data and local evidence.
- Native access-control v1 does not provide shared-kernel multitenancy,
  clustering, distributed revocation, OAuth, SSO, certificate management,
  billing, or hosted account recovery.
- Traffic analysis and denial of service beyond the documented finite local
  admission/resource policy remain deployment concerns.

## Verification ledger

The normative matrix and static fail-closed checker land before implementation.
Implementation closure additionally requires property tests, mutation testing
of the evaluator/parser, cross-transport conformance, crash injection for every
security mutation, backup/restore migration, secret canaries, and exact-source
receipts on Linux, macOS, and Windows.
