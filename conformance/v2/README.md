# Managed Native v2 authority conformance

`authority-cases.json` is the canonical, fail-closed case inventory for the
managed Native protocol authority surface shipped through Native 1.3. It covers
exactly six secret-free security reads, six metadata writes, and the Owner-only
legacy-bearer terminal write.
Rust, Python, and the read-only MCP surface consume this shared authority
inventory. The corpus does not implement authorization; every adapter remains
bound to the Native product authority and its executable evidence.

The corpus binds the wire slice to the Native access-control contract, the
protocol minor boundary, built-in role decisions, pagination cursors, bounded
results, secret redaction, idempotency request digests, uniform authentication
denials, and next-operation revocation. `security.audit_read` intentionally
uses `audit.read`; therefore `operator` can read audit events but cannot use the
other five security-read operations.

Run the static gate with:

```sh
PYTHONPATH=. python3 -m unittest tools.test_check_native_v2_authority_conformance -v
PYTHONPATH=. python3 tools/check_native_v2_authority_conformance.py
```

Eight evidence rows name locked Rust test commands and one row names the live
Python managed-client runner. The checker verifies that all required cases are
covered and that the anchors still exist. The Python row runs the same built
wheel on Linux, macOS, and Windows against the real `hyphae serve` binary over
AF_UNIX or a local named pipe and the binary HTTP v2 edge. Generic operations
use `/v2/execute`; every API-key lifecycle operation and legacy-bearer revoke
use only the strict,
managed `/v2/security/keys` family. Every HTTP request and response carries the
exact protocol minor 3 header before session retention or body decoding. The JSON
corpus and static checker do not substitute for those hosted executions. The
role-matrix row remains fixed to the exhaustive managed write-plane test:
durable Admin and Owner credentials execute all six security mutations, while
durable Auditor, Developer, Operator, Reader, and Writer credentials receive
the same authorization denial for each mutation.

The authentication denial shape is deliberately identical for missing,
malformed, wrong, expired, and revoked credentials. Metadata cursors bind the
authorization epoch and fail with `catalog_conflict` after epoch drift. Audit
cursors bind retained event identity and fail with `invalid_request` outside
the retention window. Minor 0 cannot carry security reads or writes, and minor
1 cannot carry security writes; these shapes are rejected before product
dispatch.

The live Python receipt binds the source commit and tree, installed wheel,
product and fixture binaries, negotiated Native 1.3 protocol, exact transport
inventory, and canonical transcript digest. The aggregate additionally binds
those receipts to its own checkout and retains every lane's receipt, product,
fixture, and transcript digest. The live corpus exhausts each bounded page,
rejects repeated items and cursors, proves a stale cursor after an epoch change,
walks every metadata response for secret-bearing fields, exercises Start's
one-time delivery plus Activate/Abort/Revoke terminal replay, revokes the
migrated legacy bearer, and reads back the durable principal, role, and
assignment effects before accepting the transcript. The
daemon must remain alive until the harness performs its controlled shutdown.

Async Windows named-pipe lifecycle is retained as a separate hosted receipt
because the managed transcript above is intentionally byte-identical across
its three OS lanes. `Python Windows async named pipe` installs the same exact
wheel in an isolated environment on `windows-2025` and exercises a real Win32
byte stream. A controlled `CreateNamedPipeW` peer stalls both WELCOME and
product-response reads. At each stall, task cancellation, an absolute
deadline, and `aclose()` must interrupt synchronous I/O in less than one
second. Cancellation and deadline cases must reconnect on the same client
without stale bytes; close cases must remain terminal. The retained transcript
and receipt are checked against
`schema/python-windows-async-receipt.schema.json`, the exact source commit/tree,
and the SHA-256 of the installed wheel. Release readiness requires this hosted
job in addition to the three-platform managed aggregate.

No receipt contains an endpoint, filesystem path, credential, bearer header,
verifier, or raw response. The Windows lane proves local named-pipe and
loopback HTTP behavior; it does not claim owner-only Windows ACL enforcement
for credential files, which remains a separate 1.2 release gate.

## Security process-crash matrix

`security-crash-cases.json` is the fail-closed inventory for every current
mutating security `ProductOperation` in the access registry plus the exact
offline owner-recovery and legacy-bearer APIs. The data-driven product test
executes all 26 operation cases in 19 semantic families at all seven real
`CommitBoundary` values (182
injected interruption, drop, and reopen cases). State remains prior through
`PageSynchronized`; process-crash recovery treats `WalAppended`,
`WalSynchronized`, and `RootPublished` as complete.

The separate `security_crash_matrix` example runs the same 26-case inventory
through real child processes at every boundary (182 hard kills). The child
notifies the parent synchronously from the exact `CommitBoundary` hook and
parks there; the parent requires signal 9 and rejects a destructor-written
unwind sentinel before reopening. `Security*` cases enter through public
`ProductOperation` plus `NativeProduct::dispatch`; owner recovery and legacy
bearer cases enter through the five exact public offline APIs named in the
corpus. Start recovery verifies an inactive key and retained existing owner;
activation, abort, and revoke verify terminal state, including owner/legacy
combinations. The receipt schema is
`schema/security-crash-receipt.schema.json`.

The optional `<shard-index> <shard-count>` arguments partition operation cases
deterministically. Every shard still runs all seven boundaries, and the checker
accepts evidence only when the aggregate has every shard and exactly the 182
corpus-derived `(case, boundary)` pairs. Both injected and hard-kill layers
claim process-crash semantics only, not power-loss durability.

```sh
PYTHONPATH=. python3 -m unittest tools.test_check_security_crash_matrix -v
PYTHONPATH=. python3 tools/check_security_crash_matrix.py
cargo test --locked -p hyphae-native-product --test security_crash_matrix
cargo run --locked -p hyphae-native-product --example security_crash_matrix -- \
  <commit> <environment> [<shard-index> <shard-count>]
```
