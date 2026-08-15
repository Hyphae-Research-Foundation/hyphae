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

Raw structure operations bind to the canonical default keyspace object before
scope evaluation. Search operations bind the requested stable collection or
index. A transaction accumulates the union of every referenced scope and
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
active successor. Zero-overlap activation removes the retired predecessor;
later rotations prune only fully retired ancestors and include every pruned
public key ID in the rotation audit event. A new successor is rejected while
an older predecessor remains inside a live overlap window.

### Revoke and expire

Revocation is durable and increments the authorization epoch. Expiry is
checked on every operation and cannot be extended by a cached session. Listing
keys returns redacted metadata only.

### Self-management

Self-management may create a key whose roles are a subset of the current key's
effective roles and whose scope and permission ceilings are no broader. It may
rotate or revoke only keys of the same principal. Quotas and deadlines remain
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

Once the catalog is bootstrapped, every online CLI/TUI, local-daemon, and Native
HTTP entry point selects managed API-key authentication automatically. Offline
bootstrap/recovery is the only CLI exception. A public
transport must never project an operating-system peer, loopback placement, or
legacy fixed bearer into `ProductAuthorization::ALL` for that directory.
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

The process-wide bearer is accepted for one compatibility minor as a synthetic
`legacy-owner` credential when explicitly configured. It is not persisted or
given a fabricated key ID. Offline migration creates a canonical owner key and
records the transition. Operators must cut clients over and explicitly revoke
legacy acceptance. New remote instances reject legacy-only setup after the
compatibility window.

That compatibility window is a target contract, not a claim of current
availability. Until the synthetic `legacy-owner` credential, migration
operation, and explicit revocation are implemented together, a bootstrapped
catalog rejects the fixed bearer fail-closed; the promised minor window has
not started.

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
