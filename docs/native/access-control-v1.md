<!-- SPDX-License-Identifier: Apache-2.0 -->
# Native access control v1

Status: normative contract; implementation evidence pending

This contract defines durable identity, role-based access control, scoped
grants, and API keys for Hyphae Native. It extends the product contract without
creating another engine, data directory, listener, or authorization authority.

The machine-readable registry is
[`contracts/native-access-control-v1.json`](../../contracts/native-access-control-v1.json).
The registry wins over examples in this document if prose and a machine row
ever differ; the fail-closed checker prevents known drift.

## Terms

- **Principal**: stable identity for a human, service, or local operating-system
  caller.
- **Role**: named direct set of scoped permission grants.
- **Assignment**: association of one role with one principal.
- **API key**: bearer credential for one principal, optionally narrowed to a
  subset of roles, permissions, and scopes.
- **Scope**: stable resource boundary for a grant.
- **Authorization epoch**: monotonic durable generation invalidating cached
  effective authorization.
- **Owner**: the unique recovery and ownership authority for one data
  directory. This is not a hosted account owner.

## Stable identities

Principal, custom-role, assignment, key, and audit-event IDs are nonzero
128-bit values. Wire JSON represents them as canonical lowercase hexadecimal
text of exactly 32 characters. Native binary codecs use 16 raw bytes. Built-in
roles use reserved lowercase ASCII names and have no mutable definition.

Display names are 1 through 128 UTF-8 bytes and are not authority. Names may be
changed without changing an ID, key, assignment, scope, or audit reference.

## Permissions

Permissions are append-only lowercase dotted identifiers. Unknown identifiers
fail closed. Native access-control v1 defines:

| Permission | Meaning | Valid scope |
|---|---|---|
| `audit.read` | Read bounded durable security events | instance |
| `backup.create` | Create and verify a new backup | instance |
| `backup.verify` | Verify an existing backup without activation | instance |
| `catalog.read` | List, resolve, and describe catalog objects | instance, subtree, object |
| `catalog.write` | Create or mutate catalog definitions | instance, subtree, object |
| `credential.self_manage` | Create, abort pending creation/rotation, rotate, or revoke the caller's narrowed keys | instance |
| `data.read` | Read SQL and structure data and transaction outcomes | instance, subtree, object |
| `data.write` | Mutate SQL, structures, search documents, and transactions | instance, subtree, object |
| `discover` | Read versions and bounded capabilities | instance |
| `maintain` | Checkpoint, doctor, compact, vacuum, and retention operations | instance |
| `observe` | Read status, telemetry, and bounded explain information | instance |
| `ownership.manage` | Transfer ownership and authorize offline recovery policy | instance |
| `proof.generate` | Generate a proof for an otherwise-authorized read | instance, subtree, object |
| `proof.verify` | Verify caller-provided proof artifacts offline | instance |
| `restore` | Restore a verified backup into a new directory | instance |
| `search.execute` | Execute lexical, vector, ANN, and hybrid retrieval | instance, subtree, object |
| `security.manage` | Mutate principals, roles, assignments, and other principals' keys | instance |
| `security.read` | Read redacted security metadata | instance |

`proof.generate` is additional: the wrapped operation's permissions and scopes
are also required. `credential.self_manage` can never assign a role, permission,
or scope absent from the caller's current effective authorization.

## Built-in roles

Built-in roles are immutable and cannot be dropped, renamed, or shadowed.

| Role | Intended use | Important exclusions |
|---|---|---|
| `owner` | Unique complete instance authority and offline recovery | none |
| `admin` | Product/security administration and ordinary data work | ownership transfer/recovery |
| `operator` | Health, maintenance, backup, verification, and audit | record contents, DDL/DML, restore, key/role administration |
| `developer` | Schema and application development across all native engines | maintenance, backup, restore, security administration |
| `writer` | Application read/write and search | DDL, maintenance, security administration |
| `reader` | Application read, search, and proofs | every mutation and administrative operation |
| `auditor` | Metadata, telemetry, security metadata, audit, backup/proof verification | user record contents and every mutation |

The exact permission arrays are normative in the machine registry. Custom
roles contain direct grants only. V1 does not implement role inheritance,
negative grants, wildcard permission strings, or name-pattern scopes.

## Scopes

One grant has exactly one scope:

- `instance`: the complete local product instance;
- `catalog_subtree`: one stable catalog object and every descendant; or
- `catalog_object`: exactly one stable catalog object.

Catalog scopes encode a nonzero `ObjectId`, never a display name. Scope
containment is evaluated against the immutable catalog snapshot used to bind
the operation. Renaming cannot affect access. Dropping and recreating an object
does not transfer its old grant because the new object has a new ID.

SQL authorization happens after parse and bind. All relations, indexes,
keyspaces, search collections, links, views, and functions referenced by the
bound plan must be admitted. DDL requires `catalog.write`; DML requires
`data.write`; read-only SQL requires `data.read`. A string prefix or keyword
heuristic is not authorization. For a credential without instance-wide SQL
authority, bind failures for an out-of-scope existing object, a missing object,
and malformed SQL expose the same authorization denial; binder diagnostics
must not become a catalog-existence oracle.

`ExecuteSql`, `AdminExplainSql`, and `TransactionStageSql` consume the exact
binder result used for authorization. A read reuses its `PreparedStatement`
object set. DML includes the target relation, every maintained secondary index,
and every foreign-key relation or referenced index inspected by execution;
`DELETE` also includes child relations inspected for incoming references.
Staged SQL binds against the explicit transaction's private catalog and retains
the exact object/permission union for commit reauthorization. Execution either
reuses that binding or rejects a catalog-version mismatch; it never silently
authorizes one binding and executes a fresh one. `AdminExplainSql` requires
`observe` at instance scope plus `catalog.read` on every bound object. Only
`CREATE TABLE` remains instance-scoped for `catalog.write` because the current
catalog has no durable schema parent for a not-yet-created relation.

Raw scalar `StructureGet`, `StructureSet`, and `StructureTtl` operations bind to
the stable `ObjectId` in the strict internal `HYPDKB01` record before scope
evaluation. They require `data.read` or `data.write` at that exact durable
canonical keyspace, never merely at `instance`. The record is lineage-bound and
names are not authority. Search operations bind the requested stable collection
or index. A transaction accumulates the union of every referenced scope and
reauthorizes the complete union before commit.

Catalog listing currently requires instance-wide `catalog.read`. Object- and
subtree-scoped pages fail closed before traversal because the public cursor and
physical-work metadata are not yet scope-opaque. A requested parent must never
be treated as implicit authority for its children. A future scoped listing
must use a scope-aware traversal and continuation that cannot reveal invisible
object identities, counts, ordering, or stop conditions.

## API-key format

Canonical serialized form:

```text
hyp1_<key_id>_<secret>
```

where `key_id` is exactly 32 lowercase hexadecimal characters (128 random
bits), and `secret` is exactly 64 lowercase hexadecimal characters (256 random
bits). The complete key is therefore exactly 102 visible ASCII bytes.

Parsing is strict: no whitespace, uppercase hexadecimal, alternate alphabet,
missing field, trailing field, Unicode look-alike, or future version is
accepted as v1. Missing, malformed, unknown, expired, revoked, disabled, and
wrong credentials produce the same public unauthorized result.

Hyphae stores:

- key ID and principal ID;
- bounded display label;
- verifier and verifier algorithm/version;
- selected role IDs and optional permission/scope ceiling;
- creation, optional expiry, optional rotation-overlap, and revocation state;
- approximate last-use metadata; and
- authorization epoch at publication.

Hyphae does not store the secret or complete serialized key. The verifier is:

```text
BLAKE3("hyphae-api-key-v1\0" || key_id_bytes || secret_bytes)
```

Random high-entropy secrets do not use a password KDF. Verification compares
all verifier bytes in constant time.

## Key lifecycle

### Create

Creation chooses fresh key/secret bytes, validates requested roles and ceilings
against the principal's current assignments, creates a new restricted output
file with exclusive creation, and durably publishes the verifier and audit
event. The secret is returned only through that output file. If restricted
output or activation is interrupted after the pending verifier commit, the
actor can abort the unique pending issue by principal and label before retrying.
When the per-principal key limit would otherwise block issuance, Hyphae may
replace only the oldest unlinked revoked or expired record and includes that
retired public key ID in the issue audit event; live and pending keys are never
evicted.

### Rotate

Rotation creates one successor. An optional finite overlap keeps exactly the
immediate predecessor valid until the earlier of its overlap deadline,
revocation, expiry, principal disablement, or role removal. A key cannot have
multiple live successors. An interrupted inactive successor can be aborted by
its known predecessor ID; abort is durable, audited, and never applies to an
active successor. Zero-overlap activation immediately revokes and retains the
predecessor only as a verifier-bound terminal replay identity;
later rotations prune only fully retired ancestors and include every pruned
public key ID in the rotation audit event. A new successor is rejected while
an older predecessor remains inside a live overlap window.

### Revoke and expire

Revocation is durable and increments the authorization epoch. Expiry is
checked on every operation and cannot be extended by a cached session. Listing
keys returns redacted metadata only.

### Self-management

Self-management may create a key whose roles are a subset of the current key's
effective roles and whose scope and permission ceilings are no broader. Every
requested ceiling is checked independently against both the actor's credential
ceiling and current effective scoped grants in one immutable catalog snapshot.
`instance` covers all scopes; `catalog_subtree(parent)` covers only that stable
`ObjectId` and its descendants; and `catalog_object(id)` covers only that exact
stable ID. A descendant object or subtree is admitted by an ancestor subtree,
while siblings and ancestors are denied. Multiple requested ceilings must all
be covered. Missing, outside, and otherwise unresolvable requested scope
relationships return the same authorization denial. Self-management may rotate
or revoke only keys of the same principal. Quotas and deadlines remain
mandatory.

## Effective authorization

For one request:

1. authenticate the key or trusted local peer;
2. reject disabled principal, revoked/expired key, or stale credential version;
3. load the principal's current role assignments;
4. select only roles permitted by the key;
5. form the additive permission grants;
6. intersect them with key permission and scope ceilings;
7. bind the operation to stable resource identities;
8. require every permission and every bound resource; and
9. record the authorization epoch in the operation context.

Unknown or incomplete state fails closed. A transport may cache the result only
while the global authorization epoch is unchanged. Expiration is evaluated
even when the epoch did not change.

Once the catalog is bootstrapped, every online CLI/TUI and native local-daemon
HTTP entry point selects managed API-key authentication automatically. Offline
bootstrap/recovery is the only CLI exception. A public
transport must never project an operating-system peer, loopback placement, or
legacy fixed bearer into `ProductAuthorization::ALL` for that directory. The
sole exception is the explicit Native HTTP 1.2 migration window below; it uses
a synthetic authority kind rather than unmanaged or fabricated managed IDs.
Unmanaged sessions remain available only to an explicit trusted embedded
caller and to an unbootstrapped directory before the offline owner bootstrap.
Online CLI and TUI commands accept the credential only through
`--native-api-key-file <restricted-path>` (or
`HYPHAE_NATIVE_API_KEY_FILE`) and `--native-api-key-stdin`. The secret is never
accepted as an argument value or printed. On Unix, the file must be a regular,
non-symlink path with no group or other permission bits. Commands whose
maintenance action has not yet been promoted into the central product
operation registry fail closed after bootstrap instead of using a raw facade.
The Unix reader binds pre-open, post-open, and opened-handle device/inode
identity so a substituted path is rejected before any credential is parsed.

### Offline owner recovery

Offline owner recovery is not a public transport operation. The bounded CLI
surface is `security owner inspect|recover|resume|abort-pending`; each command
opens the directory directly under the exclusive native `LOCK` and requires
OS-owner authority rather than a managed key. Unix requires a stable regular
directory path, no symlink, and directory UID equal to process EUID. Windows
rejects ADS/reparse paths and requires stable identity, current-process-SID
ownership, and a protected DACL granting full access only to that SID and
LocalSystem.

The catalog has at most one pending owner-recovery record containing a fresh
operation ID, replacement key ID, phase-one authorization epoch, creation time,
and `offline_os_owner` provenance. `recover` publishes that record and inactive
verifier without changing active owner keys. It rejects existing pending state
with `catalog_conflict`. The new output file must not exist and must resolve
outside the data directory; permissions/ACL are restricted before secret bytes,
then file and, where the platform supports it, parent directory are
synchronized. Windows parent-directory synchronization is explicitly not
claimed.

`resume` requires the exact key ID and phase-one epoch and reads a stable,
restricted, complete canonical key file. It verifies key ID and secret against
the pending verifier before a strict atomic commit activates the replacement
and removes every prior owner key. `abort-pending` removes exactly the inactive
key and provenance while preserving all active keys and never deleting a file.
Both terminal operations persist a redacted replay marker bound to operation,
key, expected epoch, result epoch, and transaction ID, making exact retry after
reopen return the original commit. `inspect` returns only redacted provenance.

## Operations

Every current `ProductOperation` variant and each planned security operation
has a row in the machine registry. Notable rules include:

- SQL classification is parser/binder-owned.
- `AdminStatus` and `Telemetry` require `observe`, not a broad admin bit.
- `AdminCheckpoint` and `Doctor` require `maintain`.
- backup creation, backup verification, and restore are independent.
- proof generation inherits the complete wrapped operation rule.
- prepared execution reuses stable bound objects but reauthorizes them.
- transaction commit reauthorizes the union of staged permissions/scopes.

Adding an externally reachable operation without a matrix row fails CI.

### First read-only 1.2 slice

The first read-only promotion closes six security operations:
`security.status`, `security.principal_list`, `security.role_list`,
`security.assignment_list`, `security.key_list`, and `security.audit_read`.
They use typed `ProductOperation` variants, central managed authorization,
bounded canonical pages, redacted CLI/TUI output, protocol minor-1 gating, and
native-daemon/HTTP/Rust-client parity. Metadata reads require instance-scoped
`security.read`; audit reads require instance-scoped `audit.read`.

### First secret-free write-plane 1.2 slice

The first write-plane promotion is deliberately limited to these six typed
mutations:

| Operation | Contract |
|---|---|
| `security.principal_create` | Create one fresh principal in the disabled state. Enabling it is a separate mutation. |
| `security.principal_set_enabled` | Enable or disable one existing principal without changing its stable ID. |
| `security.custom_role_create` | Create one immutable custom role containing canonical direct grants. Rename, replacement, and drop are not part of this slice. |
| `security.assignment_create_built_in` | Create one built-in-role assignment at its exact scope. Assigning `owner` is forbidden. |
| `security.assignment_create_custom` | Create one assignment of an existing immutable custom role to one principal. |
| `security.assignment_revoke` | Revoke one exact assignment by stable ID. Revoking an `owner` assignment is forbidden. |

Every mutation requires an authenticated managed session with instance-scoped
`security.manage`. Each request carries a nonzero idempotency token and is
admitted only with strict durability. The first success commits the canonical
mutation and its redacted audit event atomically and returns a durable receipt.
An exact retry returns the retained result without a second mutation, epoch
advance, or audit event. Reusing the token for a different canonical request
returns `idempotency_conflict` and changes no state. A transport cannot
downgrade the durability or substitute a locally synthesized authority.
Replay retention is bounded to 64 cryptographically selected shards with 64
FIFO records per shard. Reuse after a record leaves that explicit window is a
new mutation attempt; clients that require longer recovery retain their final
receipt as the durable authority.

These operations contain no credential secret, verifier, secret-output path,
or one-time secret response. Embedded dispatch, the local protocol, and Native
HTTP use the same `ProductOperation`, managed authorization decision, request
identity, and receipt semantics. None of the six is eligible for `Prove`;
wrapping one in proof generation is an invalid request rather than a
read-authority shortcut.

This initial slice does not include principal rename; custom-role rename, drop,
or replacement; API-key lifecycle; ownership transfer; legacy-bearer migration;
or owner recovery. Later sections define the independently closed key,
migration, and recovery boundaries. The CLI exposes exactly these six mutations
through the same typed
`ProductOperation` variants and emits only redacted durable receipts. The TUI
remains read-only. Neither surface may call the access-control catalog
directly.

The canonical managed Native 1.2 authority cases for these twelve operations
live in [`conformance/v2/authority-cases.json`](../../conformance/v2/authority-cases.json).
The fail-closed checker binds that corpus to this contract, its exact role
matrix, uniform authentication denial, pagination and result limits, protocol
minor admission, request digests, revocation, redaction, and named executable
Rust evidence. The corpus is an authority inventory, not a second
authorization implementation.

`backup.verify` remains `planned-1.2` and local-only. It will not enter the
generic native or HTTP transport until a later contract defines a configured
backup root and handle-relative, no-follow path resolution. A client-selected
server filesystem path is not an accepted authority boundary.

## Bootstrap

When the access-control catalog is empty, an offline bootstrap command holding
the exclusive data-directory lock may create exactly one `owner` principal.
The command requires a new output-file path and creates the file with
owner-only permissions or an account-only Windows ACL. It never accepts or
prints a secret in argv.

A crash cannot leave a usable verifier without a definite restricted output
file or leave a key file whose verifier was not committed. Bootstrap cannot
run again after any principal exists.

## Legacy bearer migration

The process-wide bearer is accepted only by Native HTTP during the exact 1.2
compatibility minor, only after the offline command below, only when the
listener explicitly receives the same restricted bearer file, and only on a
loopback plaintext bind:

```text
hyphae security --data-dir <DIR> legacy-bearer migrate \
  --name <OWNER_NAME> --label <KEY_LABEL> \
  --legacy-bearer-file <RESTRICTED_FILE> --key-out <NEW_RESTRICTED_FILE>
```

`HYACAT05` records one of `never_enabled`, `migration_pending`, `dual_window`,
or terminal `revoked`, plus the durable keyed verifier required by either
enabled state. Older `HYACAT01` through `HYACAT03` decode as `never_enabled`;
`HYACAT04` remains readable only for non-enabled historical state because it
cannot represent the keyed verifier. A binary that does not know `HYACAT05`
rejects the magic rather than discarding terminal or verifier state. Phase one
creates the canonical Owner principal,
inactive `hyp1` key, migration ID, and `migration_pending` state under the
offline lock and OS-owner authority. The bearer contributes to a
domain-separated request digest and to a BLAKE3 verifier keyed by the persisted
product-local cursor authority. The plaintext bearer, fragments, and bare
bearer digest are never stored; only the keyed verifier is durable in the
catalog/WAL, and audit/output surfaces remain redacted. The CLI creates
and synchronizes a restricted key file before a second strict commit activates
the key and `dual_window`. An exact activation retry returns the retained
commit; conflicting inputs fail.

If the migration process crashes after phase one, a restarted 1.2 Native HTTP
edge may accept the explicitly configured legacy bearer while durable state is
`migration_pending`; canonical-key authentication remains disabled until the
restricted file is recovered and activation completes. This preserves a path
to finish or terminally revoke migration without making normal bootstrap an
implicit legacy enablement.

The enabled bearer verifier has a durable `HYACAT05` representation. It is a
keyed digest of the bearer digest under the persisted product-local cursor
authority, not an offline bearer credential: neither a bare digest nor catalog
bytes can authenticate without the product-local key and exact configured
bearer. Missing verifier material, an enabled `HYACAT04` state, or any downgrade
attempt fails closed. A canonical `hyp1_` candidate always takes the canonical
parser/authenticator and never
falls back to legacy comparison. The synthetic session is explicitly
`legacy-owner`, has no `SecurityId` or `ApiKeyId`, and is revalidated against
durable state on every operation. It can execute ordinary operations that the
old fixed bearer could execute, but it has neither `security.manage` nor
`ownership.manage`, cannot enter the managed security plane, and cannot revoke
itself. UDS and named-pipe handshakes never accept it.

Terminal revocation requires a canonical Owner key and an idempotency token:

```text
hyphae security --data-dir <DIR> --native-api-key-file <OWNER_KEY> \
  legacy-bearer revoke --idempotency-token <NONZERO_U128>
```

Revocation and `revoke_legacy_bearer` audit publication share one strict commit.
An already-retained synthetic session gets `authorization_denied` on its next
operation; fresh legacy authentication gets the uniform unauthenticated
response. Owner-recovery activation also changes pending or dual state to
terminal `revoked` in its activation commit. Backup and restore preserve the
state, so `revoked` cannot become enabled. The version constant permits auth
only at 1.2. A 1.3-mode server refuses startup while pending/dual state exists,
with an explicit instruction to revoke; it can start after terminal revocation.

## Owner recovery

Owner recovery is an offline operation requiring:

- exclusive ownership of the data-directory lock;
- operating-system ownership/ACL authority;
- no active daemon or HTTP server;
- an explicit new restricted output file; and
- an otherwise valid, recoverable data directory.

Recovery revokes all current owner credentials, increments the authorization
epoch, issues exactly one replacement owner key, and appends a durable event.
It cannot ignore catalog/WAL corruption, change user data, or run remotely.

The migrated HTTP bearer is represented durably only by a verifier keyed with
the product-local persisted cursor authority; plaintext is never stored. Server startup must
present the exact migrated bearer, and the sole product owner verifies every
new legacy session again. Owner-recovery activation clears that verifier and
publishes terminal revocation in the same strict commit.

## Audit contract

Durable security mutations emit append-only events with event ID, CSN, actor
principal/key public IDs, action, target public IDs, result, and bounded
redacted metadata. Secrets, token fragments, document content, SQL text, host
paths, and backtraces are forbidden.

Audit reads are bounded, ordered, cursor-based, and require `audit.read`.
Authentication failures use bounded telemetry rather than one durable write
per request. The local audit trail shares the data-directory trust limitation;
external anchoring is a deployment option, not a core dependency.

## Limits

Native access-control v1 fixes these positive defaults:

| Resource | Limit |
|---|---:|
| principals | 4,096 |
| custom roles | 1,024 |
| grants per custom role | 256 |
| assignments per principal | 128 |
| keys per principal | 64 |
| display-name or key-label bytes | 128 |
| one audit event | 4,096 bytes |
| retained audit events | 100,000 |
| one audit result page | 1,000 rows |
| key rotation overlap | 604,800 seconds (7 days) |
| verifier candidates per authentication request | 1 |
| authorization-cache entries | 4,096 |

The unique Owner principal can hold at most 63 ordinary key records. The 64th
slot is reserved exclusively for the inactive replacement used by offline
owner recovery, so normal issuance or rotation cannot exhaust the last-resort
recovery path. Recovery activation atomically retains only the replacement and
records every retired Owner key ID in the activation event. A catalog with 64
active Owner keys is rejected as noncanonical; this invariant predates the
first stable release of the v1 access-control catalog, so no deployed stable
format is silently reinterpreted.

The key ID is indexed before verifier work, so authentication never scans key
records. Implementations may expose lower configured limits but cannot silently
raise these v1 defaults or turn any limit into an unbounded collection.

## Compatibility

Permission identifiers and built-in role meanings are append-only within v1.
Adding a permission does not add it to any existing custom role. New product
operations require a new explicit matrix row; they never inherit a similarly
named operation's authority. Breaking key grammar or scope semantics requires
a new access-control contract version.

## Verification

The static registry checker proves matrix completeness and critical
least-privilege invariants. Implementation closure additionally requires:

- exhaustive role-by-operation allow/deny tests;
- random-model tests for role/key/scope intersection;
- parser/binder scope tests across SQL and all three engines;
- long-lived-session revocation and expiry tests;
- prepared and explicit-transaction stale-authority tests;
- crash injection for create, pending-abort, rotate, revoke, assignment,
  recovery, and audit;
- backup/restore and legacy-bearer migration fixtures;
- cross-surface embedded/UDS/named-pipe/HTTP/CLI/SDK/MCP parity;
- secret canaries across errors, logs, telemetry, TUI, artifacts, and receipts;
- mutation testing of key parsing and authorization evaluation; and
- Linux, macOS, and Windows exact-source receipts.
### Public API-key lifecycle

Protocol minor 3 makes key issue, rotation, activation, abort, and revoke core
`ProductOperation` variants. Self variants require `credential.self_manage` and
an exact actor-principal target. Administrative variants require
`security.manage`; any owner-targeted key additionally requires
`ownership.manage`.

Issue and rotate are two-phase. Start strictly commits an inactive verifier and
returns a fixed mutable one-time secret buffer. The client creates a new
restricted file, writes and synchronizes it, synchronizes the parent directory,
then sends Activate with the digest derived from that exact secret. No
filesystem path is present in a product operation, Native frame, HTTP body,
receipt, audit event, or telemetry event. A pending key never authenticates.
Cancellation or disconnect before Activate leaves it pending for an exact Abort.

Start is never automatically retried. Reusing its token and payload returns
`secret_delivery_consumed`; reusing the token for a different operation or
payload returns `idempotency_conflict`. Activate, Abort, and Revoke replay their
original receipt before current-authority checks only when actor key, token,
operation, and complete request digest match; no other operation can use a
revoked replay identity. Exact replay returns the original durable redacted
receipt. A wrong digest returns
`confirmation_digest_mismatch` without activating the verifier. HTTP Start
responses require `Cache-Control: no-store, private, max-age=0`, `Pragma:
no-cache`, and no `Content-Encoding`.

HTTP routes every self and administrative Start, Activate, Abort, and Revoke
variant through `/v2/security/keys`. That family requires catalog-managed
authority and strict durability; `/v2/execute` rejects lifecycle envelopes, so
the generic family cannot bypass either constraint.

At a transport boundary, a revoked canonical candidate is retained only as
opaque pending terminal state after normal authentication fails. It does not
open a session and is publicly indistinguishable from an unknown candidate.
HTTP rejects it before handler and product-client dispatch on every method and
path except `POST /v2/security/keys`; that route may consume it only after
bounded body decode. A fresh local daemon connection may consume it only after
the bounded first request frame is known. Only exact self-revoke or zero-overlap
self-rotation activation replay can open terminal authority; every operation,
token, target, confirmation digest, or marker mismatch is the uniform
authentication denial.

Sensitive Rust deliveries retain the 102 canonical bytes in fixed mutable
storage, have redacted `Debug`, do not implement `Clone`, and overwrite the
buffer on drop. Python exposes `SensitiveBytes`, backed by `bytearray`, with
`close()` and context-manager cleanup. TypeScript exposes `Uint8Array` plus
`clearSensitiveBytes()` for in-place overwrite.
