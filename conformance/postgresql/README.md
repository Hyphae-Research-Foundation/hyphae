<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# PostgreSQL external conformance

This standalone lab runs PostgreSQL only as an external conformance subject.
It is explicitly excluded from the root Cargo workspace and does not add a
PostgreSQL server, client, protocol crate, planner, executor, storage engine, or
cache to Hyphae's product graph.

The machine authority is
[`application-core-v1.json`](application-core-v1.json). It pins PostgreSQL
18.6 to both the PGDG source archive checksum and the immutable Docker Official
Images manifest, initializes UTF-8 with the `C` locale, sets UTC, fixes the
listed server settings, and inventories the P0 application cases. The
PostgreSQL scripts establish oracle behavior; current Hyphae status is a
separate evidence-backed classification and is not inferred from an oracle
pass. A closing classification requires the exact case-specific Cargo test
authority recognized by the checker; arbitrary repository files and unrelated
tests cannot close a mandatory cell. Each SQL oracle is independently bound to
its reviewed byte SHA-256.

Validate the inventory and print its current claim status without Docker:

```bash
python tools/check_postgresql_application_core.py
```

Run the external oracle when Docker and Docker Hub registry access are
available:

```bash
python conformance/postgresql/run.py \
  --operator "$USER" \
  --attest-non-claim-authoritative \
  --output /tmp/postgresql-core-v1.json
```

Validate the resulting receipt independently:

```bash
python tools/check_postgresql_application_core_receipt.py \
  --receipt /tmp/postgresql-core-v1.json
```

The container has no host port and uses `--network=none` after image
resolution. The runner verifies version, encoding, collation, timezone, and
every pinned setting before executing cases. Its receipt binds the host
architecture and operating system, repository source tree, runner and checker
bytes, pinned multi-architecture index, selected platform-specific manifest,
local image/config digest, observed settings, SQL oracle bytes, case outputs,
and successful cleanup. `--output` is mandatory, receipt output inside the
source repository is rejected, and no receipt is emitted on standard output;
shell redirection cannot bypass containment. The target must be absent beneath
an existing normal directory with no symlink components. The runner uses
no-follow exclusive mode-`0600` creation and rejects existing or special files,
`/dev`, `/proc`, `/sys`, FIFOs, sockets, devices, and stdout/file-descriptor
aliases. The retained raw OCI documents let the receipt validator recompute
index and child digests, authenticate child membership and platform, and bind
the child config to the locally selected Docker image identity.

Receipt validation checks consistency, not execution. Every run requires an
explicit operator admission stating that the result is supporting-only and
non-claim-authoritative; every receipt carries no claims and declares no
closure. Synthetic success markers therefore cannot authorize parity. The
generic term "PostgreSQL-compatible" is permanently prohibited; only the exact
profile-scoped statement may ever close, and it remains blocked while any
mandatory Hyphae cell is `partial`, `unsupported`, or `unverified`.
