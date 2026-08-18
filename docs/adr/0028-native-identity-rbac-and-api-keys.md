# ADR-0028: Native identity, RBAC, and API keys

- Status: Accepted
- Date: 2026-08-14
- Owners: Celiums Solutions LLC

## Context

Hyphae Native already binds an authenticated `ProductPrincipal` and a closed
permission set to every product session. The local daemon derives a principal
from operating-system peer credentials, while HTTP `/v1` and `/v2` can compare
one process-wide bearer token. Those boundaries prove that authorization is a
product concern, but they do not provide durable principals, reusable roles,
scoped grants, independent credentials, rotation, revocation, or security
audit history.

The current permission set is also too coarse for least privilege. `Admin`
combines observation, explain, checkpoint, and doctor operations. `Backup`
combines backup creation with restore. A credential intended only for
monitoring or backup could therefore receive mutation or recovery authority it
does not need.

The TUI, public SDKs, MCP adapter, and agent plugins must all consume one
authorization decision. Transport-specific role systems would drift and could
become confused deputies.

## Decision

### One authorization authority

Identity and authorization are evaluated inside the Native product boundary.
HTTP, the native local daemon, embedded callers, CLI, SDKs, MCP, and the TUI
may authenticate a caller, but none may invent or widen permissions.

The durable model separates:

- a **principal**, which names a human, service, or local operating-system
  identity;
- a **role**, which contains additive scoped permission grants; and
- an **API key**, which authenticates one principal and may only narrow that
  principal's assigned roles and scopes.

Built-in roles are immutable. Custom roles contain direct grants. Native
access-control v1 has no role inheritance and no negative grants. Removing a
role assignment or revoking a key takes effect on the next operation, including
an operation in an already-open transport session.

The normative permission, built-in-role, operation, scope, and key-format
registry is [`contracts/native-access-control-v1.json`](../../contracts/native-access-control-v1.json).
Its human contract is
[`docs/native/access-control-v1.md`](../native/access-control-v1.md).

### Stable identifiers and scopes

Principal, role, key, and audit-event identities are nonzero 128-bit values.
Names are mutable display metadata and are never authority.

Grants may target the complete instance, a catalog subtree rooted at a stable
`ObjectId`, or one stable catalog object. SQL authorization is evaluated after
binding against every referenced stable object. Structure operations without
an explicit object identity are bound to the canonical default keyspace before
authorization. A rename cannot expand a grant.

Security administration, ownership, restore, instance maintenance, backup,
and audit permissions are instance-scoped. Object-scoped forms of those
permissions are invalid.

### API keys

The canonical key is:

```text
hyp1_<32 lowercase hexadecimal key-id bytes>_<64 lowercase hexadecimal secret bytes>
```

The key ID is 128 random bits and the secret is 256 random bits, both produced
by the operating system CSPRNG. Hyphae stores the key ID and a domain-separated
BLAKE3 verifier over the decoded key ID and secret. The secret is returned once
and is never stored, logged, accepted through argv, or returned by list and
inspection operations. Random high-entropy keys do not use a password KDF.

Keys may select a subset of the principal's roles, impose a permission and
scope ceiling, expire, rotate with a bounded overlap, and be revoked. Effective
authority is the intersection of current principal assignments and current key
restrictions. Approximate `last_used_at` metadata is not authorization state.

### Bootstrap, compatibility, and recovery

A new data directory has one local bootstrap boundary. While no durable
principal exists, an offline command holding the exclusive directory lock may
create the first owner principal and write its first key to a newly created
restricted file. It may not print the key to a terminal by default.

The legacy process-wide bearer remains an explicit compatibility credential
for one minor release. It maps to a synthetic `legacy-owner` principal and
never silently becomes a durable key. An offline migration command issues a
new canonical owner key; operators switch clients and explicitly revoke the
legacy credential. New remote installations require canonical keys.

The compatibility line is exactly product 1.2 and Native HTTP only. Durable
`HYACAT05` state is `never_enabled`, `migration_pending`, `dual_window`, or
terminal `revoked`; older binaries fail on the new magic. Enabled state carries
a durable BLAKE3 verifier keyed by the persisted product-local cursor authority.
The bearer plaintext remains process-local and must be presented from the
restricted configuration file; a bare bearer digest is never sufficient for
offline authentication. Canonical `hyp1` never falls back, and the synthetic
authority has no fabricated principal/key IDs or security/ownership management.
Because `HYACAT04` cannot represent the keyed verifier, downgrade or an enabled
older-format state fails closed rather than synthesizing authority.
Owner recovery revokes enabled legacy state in the same activation commit. A
1.3-mode server cannot authenticate it and refuses enabled-state startup until
canonical Owner revocation completes.

Owner recovery is offline only. It requires the exclusive data-directory lock,
operating-system ownership of that directory, an explicit output file, and no
active server. Recovery increments the authorization epoch, revokes every
existing owner credential, creates one replacement owner key, and appends a
durable security event. It does not repair or bypass data-integrity failures.

### Revocation and transactions

Every session caches effective authorization together with a durable
authorization epoch. Each operation performs a cheap epoch check and reloads
authority when the epoch changed. Expiry is evaluated for every operation.

Prepared statements retain the stable objects and permissions established at
bind time and are checked again at execution. Explicit transactions check
authority while staging and immediately before commit. Losing authority before
commit fails closed and rolls the transaction back; staged work cannot commit
under stale grants.

### Auditing

Principal, role, assignment, key, ownership, migration, and recovery mutations
append bounded durable audit events under the same WAL and commit sequencing as
the access-control state. Events contain public IDs and redacted metadata, never
secrets. Authentication failures are counted and emitted through bounded local
telemetry; untrusted failures do not force an unbounded durable write workload.

### Product boundary

Hyphae remains offline and single-instance. RBAC does not claim hosted
multitenancy, OAuth, TLS termination, billing, cluster-wide revocation, or
protection from an attacker controlling both the process and the complete data
directory. Remote API keys still require a trusted TLS boundary.

## Consequences

- TUI, SDK, MCP, and plugin behavior share one authorization result.
- Backup creation, verification, and restore can be delegated independently.
- Observation no longer grants checkpoint, doctor, or security authority.
- Product sessions require epoch-aware authorization rather than one immutable
  bitset for their complete lifetime.
- Catalog binding becomes part of scoped authorization and must remain stable
  across rename and prepared execution.
- The durable catalog, WAL, backup, restore, migration, and proof surfaces gain
  new access-control records and negative-path tests.
- Legacy bearer compatibility has a bounded removal path instead of becoming a
  permanent second authority.

## Alternatives considered

### Encode permissions directly in each API key

Rejected because role changes would require rotating every key and stale keys
could retain privileges after the intended role changed.

### Use PostgreSQL roles or another external identity database

Rejected because Hyphae must remain one autonomous binary and data directory.
External identity services may authenticate at a deployment edge later, but
the resulting principal still resolves through this product contract.

### Authorize only at HTTP and local-protocol adapters

Rejected because embedded callers and future adapters could bypass the policy,
and transport-specific implementations would drift.

### Preserve the current `Admin` and `Backup` permissions

Rejected because they grant observation credentials mutation authority and
backup credentials restore authority.

### Persist every failed authentication attempt durably

Rejected because an unauthenticated peer could turn the audit trail into a
write-amplification and disk-exhaustion primitive.

## Verification

- `tools/check_native_access_control.py` validates the normative registry,
  built-in roles, key grammar, scopes, dangerous-role exclusions, and complete
  coverage of every current `ProductOperation` variant.
- `tools/test_check_native_access_control.py` proves that permission, role,
  operation, restore, scope, and key-format drift fail closed.
- RBAC implementation requires exhaustive operation-by-role conformance,
  revocation/expiry session tests, scoped SQL and catalog tests, transaction
  revocation tests, crash/recovery tests for security mutations, transport
  parity, secret-redaction tests, and migration/recovery fixtures before the
  feature can be labelled implemented.
