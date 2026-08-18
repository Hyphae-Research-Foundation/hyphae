<!-- SPDX-License-Identifier: Apache-2.0 -->
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
| Catalog pagination leaks hidden topology or becomes authority | Minor-3 listing traverses exact roots or the durable ancestor-descendant index, deduplicates overlaps before filters, hides physical accounting, redacts hidden parents, and authenticates opaque cursors to current key/session authority, epoch, snapshot, filters, family, and last visible ID | Exact/subtree/sibling/overlap, rename/drop-recreate, cursor tamper/cross-binding, and empty-page tests |
| Raw scalar operation escapes object scope | `HYPDKB01` binds Get/Set/TTL to one lineage-bound canonical keyspace ID; malformed, absent-after-bootstrap, or mismatched definitions fail closed | Exact/unrelated scope, reopen, codec corruption, and crash-boundary tests |
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
| Offline bootstrap or recovery while server is active | Exclusive directory lock and no remote endpoint; Unix stable non-symlink directory with UID equal to EUID; Windows stable non-reparse/no-ADS directory owned by current SID with protected current-SID/LocalSystem-only DACL; output path must be a new restricted regular file outside the data directory | Process exclusion, ownership/ACL/identity, output-containment, and filesystem failure tests |
| Rollback to an older credential database | Normal manifest/WAL lineage verification plus caller-pinned external anchors where rollback resistance is required | Backup/restore lineage and pinned-anchor tests |
| Local client spoofs another principal | Before bootstrap, kernel peer identity is trusted-local compatibility metadata; after bootstrap only a durable API key is authority and client-supplied identity remains diagnostic | UDS/named-pipe adversarial handshake and automatic-cutover tests |
| Another local account reads raw data or reaches the local endpoint | A new native data directory is created owner-only (`0o700`) inside the `mkdir` call on Unix, covering WAL, pages, blobs, the security catalog, and the default in-directory socket; Windows inherits the profile-directory ACL and protects the named pipe with an owner/LocalSystem DACL. A custom Unix endpoint outside the data directory must be placed in an owner-only parent directory by the operator; the daemon still restricts the socket file itself to `0o600` | Directory-mode assertion on `create`/`create_pending`; socket-permission tests; documented operator requirement for external endpoints |
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
11. No request may provision or repair the default scalar keyspace; provisioning
    occurs under the exclusive directory owner before the first Owner is
    published. A bootstrapped directory with a missing or corrupt binding is
    not ready.
12. Catalog cursors are continuations, never capabilities. Each page resolves
    current `catalog.read` roots before cursor validation or traversal.

## Bootstrap and recovery risks

Bootstrap is permitted exactly once when no principal records exist. Failure
after durable principal creation but before key-file publication must not leave
an unknowable active credential: the transaction either publishes both the
verifier and a definite output receipt, or rolls back. The key file is created
with exclusive creation and restrictive permissions before the transaction is
acknowledged.

Before the first Owner catalog is published, the product requires the complete
default scalar Database/Schema/Keyspace and `HYPDKB01` binding. Preview
directories with no durable principals may receive that additive migration as
one strict transaction while holding the directory lock. Once bootstrapped,
missing state is diagnosed as corruption rather than silently recreated or
adopted by name.

Recovery proves operating-system control of the offline directory before it
reads the catalog. Unix binds pre-open, opened-handle, and post-open
device/inode identity and requires directory UID equal to process EUID. Windows
rejects ADS and reparse paths, binds a stable directory handle, and requires the
current process SID as owner plus a protected DACL containing only current-SID
and LocalSystem full-access trustees. The normal exclusive `LOCK` rejects a
running daemon, HTTP edge, console, or embedded owner.

Phase one creates exactly one explicit pending operation ID/key ID/epoch with
offline-OS-owner provenance and an inactive verifier while retaining all old
owner keys. A second recovery conflicts. The restricted output uses exclusive
creation outside the data directory, is restricted before secret bytes, and is
synchronized before any activation request. Resume requires the exact complete
file, key ID, verifier, and expected epoch. Activation atomically enables the
replacement, removes all prior owner keys, increments the epoch, and records a
terminal replay marker with the commit receipt. Exact abort removes only the
pending verifier/provenance and cannot delete caller paths or change active
owners. Interrupted activation/abort can therefore be retried after reopen.
Inspect exposes no verifier or secret. A failed recovery leaves complete old
authority, pending old authority plus an inactive replacement, or complete new
authority, never an implicitly active unknown secret.

## Legacy bearer migration risks

The legacy bearer is one explicitly labelled compatibility credential, not a
role system. Migration never hashes an arbitrary legacy token into the new key
format or claims that it has a key ID. It issues a new canonical owner key and
requires an explicit cutover and revocation. Missing legacy configuration does
not reactivate a previously revoked credential.

The durable state machine is `never_enabled -> migration_pending -> dual_window
-> revoked`, with owner recovery allowed to move either enabled state directly
to terminal `revoked`. `HYACAT05` makes downgrade rejection structural. Only an
offline, OS-owner-authorized migration can enter pending; only durable
restricted-key output can precede dual-window activation; and only canonical
Owner authority can revoke. The bearer plaintext never enters durable state;
enabled state stores only a BLAKE3 verifier keyed by the persisted product-local
cursor authority. A bare digest copied from configuration or an offline artifact
cannot authenticate, and missing keyed verifier material fails startup closed.
`HYACAT04` and older formats cannot represent enabled verifier state and are not
accepted as an enabled compatibility authority. Canonical-looking `hyp1` input
never reaches legacy comparison. The
synthetic session has no fabricated durable IDs, excludes security/ownership
management, and checks the terminal state every operation. Compatibility is
exactly Native HTTP 1.2; UDS, named pipe, non-loopback plaintext, normal
bootstrap, restore, and 1.3 authentication cannot enable it.

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
