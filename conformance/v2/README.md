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

Every evidence row names a locked Rust test command and one or more source
anchors. The checker verifies that all required cases are covered and that the
anchors still exist. The Rust commands remain the executable evidence; the
JSON corpus and checker do not substitute for running them on their declared
platforms. The role-matrix row is fixed to the exhaustive managed write-plane
test: durable Admin and Owner credentials execute all six security mutations,
while durable Auditor, Developer, Operator, Reader, and Writer credentials
receive the same authorization denial for each mutation.

The authentication denial shape is deliberately identical for missing,
malformed, wrong, expired, and revoked credentials. Metadata cursors bind the
authorization epoch and fail with `catalog_conflict` after epoch drift. Audit
cursors bind retained event identity and fail with `invalid_request` outside
the retention window. Minor 0 cannot carry security reads or writes, and minor
1 cannot carry security writes; these shapes are rejected before product
dispatch.
