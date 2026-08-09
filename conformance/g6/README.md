# Native G6 conformance corpus

This corpus executes the Native product, not a model of it. Every lane starts
from a separately restored native backup. Restore preserves the directory
lineage, catalog version, next object-ID authority, root digest, and visible
CSN; the runner records and compares those fields before it admits a lane.

`fixtures/corpus.json` is the closed operation inventory. `fixtures/seed.json`
fixes logical time, request IDs, SQL, scalar data, and the expected normalized
transcript. `schema/*.schema.json` defines the fixture, transcript, receipt,
and aggregate contracts consumed fail-closed by the Python checker.

The canonical transcript removes only adapter-local representation details:

- byte strings become lowercase hexadecimal strings;
- 128-bit integers become unsigned decimal strings;
- map keys are sorted recursively;
- telemetry process/session identities and physical timing/counter values are
  represented by explicit type markers;
- request IDs are compared to the fixed fixture IDs, never discarded.

Lineage, catalog version, visible/commit CSN, object IDs, result ordering,
stable error fields and explain text/version are not normalized
away. A mismatch in any of those fields fails the lane.

The all-surface runner identities are:

`embedded-rust`, `cli`, `local-daemon`, `http`, `rust-sdk-local`,
`rust-sdk-http`, `python-sdk-local`, `python-sdk-http`,
`typescript-sdk-local`, and `typescript-sdk-http`.

The product-backed Rust runner owns seed/backup/restore and the embedded,
daemon, HTTP, and Rust SDK lanes. Python and TypeScript runners use their
published local and HTTP SDK adapters against daemon instances started by the
orchestrator. Every runner executes the exact flattened fixture case sequence
through its declared adapter and transport. A lane may not reopen the directory
and substitute embedded results, and literal feature labels are not outcomes.
The CLI lane invokes the built `hyphae` binary against its restored directory.
The fixture names exact applicable lanes for operations or failures that a
surface cannot represent. The checker rejects missing, extra, or reordered
executions and compares each row only across that declared set. Stable failure
rows cover syntax, not-found, limit, deadline, cancellation, authorization,
malformed input, backpressure, missing completion, and disconnect/unknown
commit. Proof rows execute actual generation and origin-independent
verification; backup rows execute create, offline verify, restore, and
doctor-after-restore wherever the adapter exposes the operation.

Receipts are platform-local. The candidate workflow uploads each platform's
conformance receipt beside its requirement receipts. The checker only emits an aggregate after exact
Linux, macOS, and Windows receipts bind the same source commit, fixture digest,
schema digest, and canonical transcript digest.

Run locally with:

```text
python3 tools/run_native_g6_conformance.py --platform macos --output target/g6/receipt.json
python3 tools/check_native_g6_conformance.py receipt --receipt target/g6/receipt.json
```

CI entry points are `tools/ci/run_native_g6_conformance.sh` and
`tools/ci/check_native_g6_conformance.sh`. G6 config and workflow files are
intentionally not changed by this corpus patch.
