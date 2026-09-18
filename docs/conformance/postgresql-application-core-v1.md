<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# PostgreSQL Application Core v1 conformance profile

This profile is an external compatibility inventory, not a product runtime
dependency and not a parity statement. PostgreSQL is an oracle process in an
isolated lab only. It never plans, executes, stores, caches, recovers, or
authorizes Hyphae data.

The machine-readable authority is
[`conformance/postgresql/application-core-v1.json`](../../conformance/postgresql/application-core-v1.json).
It pins PostgreSQL 18.6 to the official source archive SHA-256 and the immutable
Docker Official Images manifest. The lab uses UTF-8 server and client encoding,
`C` collation and character classification, UTC, durable writes, no JIT or
parallel gather, and the complete fixed setting set recorded in the profile.

## Current result

The profile has 11 mandatory P0 cases. Three narrowly bounded cells are
classified `conformant`, two are `partial`, five are `unsupported`, and one is
`unverified` against the current Hyphae implementation. Eight mandatory cells
are therefore open, so **Hyphae does not claim PostgreSQL Application Core v1
parity**.

The generic phrases "PostgreSQL-compatible", "PostgreSQL replacement", and
equivalent unscoped claims are permanently prohibited. Only the exact bounded
statement "Hyphae satisfies PostgreSQL Application Core v1" may ever become
eligible under this profile's closure rules. It is currently blocked.

Current open cells are quoted identifier preservation through SQL, PostgreSQL
constraint SQLSTATE mapping, redundant `LIMIT` on a primary-key point lookup,
a bound `LIKE` pattern, `RETURNING`, `ON CONFLICT`, savepoint execution, and SQL
catalog column views. The bounded typed-parameter API, indexed inner equijoin,
and bounded grouped count case have current repository evidence. These narrow
classifications do not imply PostgreSQL wire, grammar, type-system, transaction,
or general SQL compatibility.

The rebased engine admits redundant positive `LIMIT` on a full primary-key
lookup, but that cell remains `unverified` until this profile accepts a
dedicated case-specific executable authority. Upstream implementation and unit
coverage are supporting context, not closure evidence.

A `conformant` cell closes only when it carries the exact case-specific Cargo
test authority fixed by the checker and that named Rust test still exists.
Documentation, implementation source, or an unrelated existing test is
supporting context only and cannot make a cell parity-eligible. Case IDs,
requirements, oracle files, and executable authorities are bound independently;
editing classifications or swapping files fails closed.

The typed-parameter cell is backed by
`sql_typed_parameters::parameterized_insert_persists_declared_integer_text_and_boolean_types`.
That test executes a parameterized insert against catalog-declared `BIGINT`,
`TEXT`, and `BOOLEAN` columns, commits and reopens the database, verifies the
persisted typed values through prepared SQL, and requires each wrong logical
parameter type to fail. A scalar codec round trip alone is not sufficient
authority for this cell. The grouped-count cell separately projects its group
key and executes an explicit `ORDER BY` and positive `LIMIT`.

## Commands

Profile validation is offline and requires only Python:

```bash
python tools/check_postgresql_application_core.py
```

Any publication path that intends to assert parity must use the stricter mode,
which exits nonzero while a mandatory cell is open:

```bash
python tools/check_postgresql_application_core.py --require-parity
```

The external oracle runner requires Docker and registry access. It may pull
only the exact pinned index and separately authenticates the index and selected
child manifest from Docker Hub by their content digests:

```bash
python conformance/postgresql/run.py \
  --operator "$USER" \
  --attest-non-claim-authoritative \
  --output /tmp/postgresql-core-v1.json
```

Validate a saved receipt against the exact source tree, runner/checker bytes,
profile, settings, case inventory and SQL bytes, result output hashes, aggregate
status, and image identities:

```bash
python tools/check_postgresql_application_core_receipt.py \
  --receipt /tmp/postgresql-core-v1.json
```

The runner requires an identified operator to affirm the fixed
non-claim-authoritative admission statement before Docker starts. The output
path must remain outside the source repository so generating a receipt cannot
change the source tree it records. `--output` is mandatory and the runner never
emits receipt bytes on standard output; shell redirection is not a publication
path. The absent target must have an existing normal directory with no symlink
components. Publication uses no-follow, exclusive creation at mode `0600`,
verifies the opened inode is regular, and flushes and synchronizes it. Existing
files, `/dev`, `/proc`, `/sys`, standard-output or file-descriptor aliases,
FIFOs, sockets, devices, and symlink targets are rejected.

The receipt retains bounded base64 copies of the authenticated raw OCI index
and child-manifest documents. The validator recomputes both digests, proves the
child descriptor and platform occur in the pinned index, and requires the
child's config digest to equal Docker's local image ID and the running
container image. It also binds the Git commit and exact clean or integration
tree, runner/profile-checker/receipt-validator SHA-256 values, every SQL oracle
SHA-256, host architecture, independently observed settings, and successful
cleanup. These checks establish only that the receipt is internally consistent
with its bound inputs; they do not establish that commands actually executed.
The receipt is always `operator-observation-supporting-only`, carries no claims,
declares no closure, and has `claim_authority: false`. Even a synthetically
constructed set of `ok:` markers cannot authorize eligibility or a compatibility
claim. Closing a Hyphae cell still requires its case-specific executable Hyphae
authority and a reviewed profile update.
