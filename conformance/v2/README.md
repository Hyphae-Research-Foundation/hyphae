# Managed Native v2 authority conformance

`authority-cases.json` is the canonical, fail-closed case inventory for the
managed Native protocol authority surface shipped in Native 1.2. It covers
exactly six secret-free security reads and six secret-free security writes.
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
AF_UNIX or a local named pipe and the binary HTTP `/v2/execute` edge. The JSON
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
product and fixture binaries, negotiated Native 1.2 protocol, exact transport
inventory, and canonical transcript digest. The aggregate additionally binds
those receipts to its own checkout and retains every lane's receipt, product,
fixture, and transcript digest. The live corpus exhausts each bounded page,
rejects repeated items and cursors, proves a stale cursor after an epoch change,
walks every response for secret-bearing fields, and reads back the durable
principal, role, and assignment effects before accepting the transcript. The
daemon must remain alive until the harness performs its controlled shutdown.

No receipt contains an endpoint, filesystem path, credential, bearer header,
verifier, or raw response. The Windows lane proves local named-pipe and
loopback HTTP behavior; it does not claim owner-only Windows ACL enforcement
for credential files, which remains a separate 1.2 release gate.
