# Native performance evidence v1

Status: implemented P0 evidence contract; no performance closure claimed

This contract provides one fail-closed evidence envelope for Native SQL,
structures, lexical search, vector search, WAL, storage, and converged work.
It is independent of a particular benchmark gate. G7 may consume audited
receipts, but an accepted receipt does not by itself close G7.

The machine-readable authority is
[`config/native-performance-evidence-profile.json`](../../config/native-performance-evidence-profile.json).
The public structural contracts are
[`native-performance-receipt-v1.schema.json`](../../contracts/json-schema/native-performance-receipt-v1.schema.json)
and
[`native-performance-progress-v1.schema.json`](../../contracts/json-schema/native-performance-progress-v1.schema.json).
Suite envelopes use
[`native-performance-suite-v1.schema.json`](../../contracts/json-schema/native-performance-suite-v1.schema.json)
and a separately digested required-cell authority such as
[`native-performance-baseline-suite-profile.json`](../../config/native-performance-baseline-suite-profile.json).
`tools/check_native_performance_receipt.py` enforces the semantic constraints
that JSON Schema cannot express by itself.

## Evidence classes

- `diagnostic-baseline` records a comparable observation. Unsupported
  platform counters are permitted when they are explicit and include a reason.
- `regression-candidate` records an exact-source comparison intended for an
  accepted regression policy. The policy and threshold remain separate
  authorities.
- `qualification-candidate` requires dedicated physical hardware and every
  required counter to be measured. It still carries no release or G7 claim.

Every accepted receipt has `claims: []` and `closure_declared: false`.

## Complete suites

An individual receipt cannot prove that a matrix is complete. A suite binds
all receipts to one source tree, measured binary, clean-state flag, hardware
fingerprint, and immutable required-cell profile. The checker rejects missing,
extra, or duplicate cells and rejects a dataset identity that changes between
cells for the same operation and generator.

The required-cell profile is an authority outside the measured suite. A
producer cannot make an incomplete run pass by omitting the same cell from
both its output and its expectations. The baseline runner emits both the
individual receipt and its audited suite envelope.

## Workload classes

The workload class is one of:

- `foreground-point`;
- `foreground-bounded`;
- `mutation`;
- `bulk`;
- `maintenance`;
- `recovery`; or
- `administrative`.

The receipt also lists the participating engines in canonical profile order.
This allows one schema to cover an isolated kernel, one engine, or converged
execution without inventing engine-specific timing definitions.

## Identity

A receipt binds:

- the exact Git commit and tree;
- whether that tree is the clean commit tree;
- the measured binary digest;
- the evidence-profile digest;
- the dataset generator, digest, size, and source commit;
- the workload-parameter digest; and
- the hardware fingerprint, target, topology, affinity, compiler, and build
  profile.

The dataset source commit must equal the receipt source commit. A seed produced
for another source is rejected even if its path or human label is unchanged.
Corpus-specific checkers remain responsible for independently recomputing the
dataset digest. Diagnostic evidence may bind a dirty tree explicitly;
qualification candidates require a clean commit tree.

## Exhaustive clocks

One measurement reports aggregate nanoseconds for the same observation set in
these mutually exclusive components:

1. admission;
2. queueing;
3. parse, bind, planning, or prepared lookup;
4. engine execution;
5. cross-engine fusion;
6. WAL append;
7. physical synchronization;
8. transport;
9. result or proof encoding; and
10. explicitly unattributed time.

The component total must equal `elapsed_nanos`. Unknown overhead is assigned
to `unattributed`; it is never silently dropped. Percentiles remain
operation-specific observations and are not added or subtracted to manufacture
component latency.

## Counters

The authority requires CPU time, cycles, instructions, cache misses, context
switches, page faults, allocations, peak RSS, bytes read, and bytes written.

A measured counter contains a non-negative integer, its normative unit, and a
non-empty provider. An unsupported counter contains `value: null`,
`provider: "none"`, and a non-empty reason. Encoding an unavailable counter as
zero is rejected. Qualification candidates cannot contain unsupported
counters.

## Correctness

Performance evidence is accepted only with a passed correctness oracle and a
result digest. A raw timing without correctness may be retained as an
unvalidated diagnostic log, but it is not a receipt under this contract.

## Long-operation progress

Bulk load, ANN construction, index creation, consolidation, recovery, backup,
and similar work emit progress records bound to the exact source tree and
dataset. Records contain a sequence, stage, completed and total units, elapsed
time, and optional checkpoint digest.

When a previous record is supplied to the checker, sequence, completed units,
and elapsed time must advance monotonically without changing operation or
dataset identity. A completed record requires all units and a checkpoint
digest. The progress contract does not promise that every algorithm can resume
from every intermediate stage; resumability is declared by the presence and
meaning of its checkpoint.

The G7 ANN corpus builder emits `ann-private-build` observations from the
canonical HNSW kernel, followed by `ann-publication` and `ann-published`. The
completed checkpoint is the durable base-generation identity observed after
commit, not a digest invented by the controller. A reused, independently
validated seed does not replay synthetic build progress. The current
publication path does not expose node-level progress and replays the initial
vector mutation during commit; eliminating that duplicate generation is a P4
optimization and remains visible as a separate stage in the meantime.

The complete G7 controller keeps ANN build progress separate from matrix
progress. After every state/background/concurrency cell it atomically publishes
an exact-source record containing the completed cell identities, total count,
and the currently running cell. This controller record is diagnostic, not a
claim that a partial cell can resume. Each cell has a two-hour watchdog, the
matrix has an eleven-hour controller deadline, and the dedicated workflow has
a twelve-hour hard stop so a failed or stalled run retains actionable progress
without exceeding its approved host budget. Failure artifacts preserve both
controller and ANN progress even when no closure matrix exists.

## Validation

Validate one receipt:

```console
python3 tools/check_native_performance_receipt.py \
  --receipt receipt.json \
  --expected-commit "$SOURCE_COMMIT" \
  --output receipt-audit.json
```

Validate a progress transition:

```console
python3 tools/check_native_performance_receipt.py \
  --progress current-progress.json \
  --previous-progress previous-progress.json \
  --expected-commit "$SOURCE_COMMIT" \
  --output progress-audit.json
```

Validate a complete suite against its required-cell authority:

```console
python3 tools/check_native_performance_receipt.py \
  --suite baseline.suite.json \
  --suite-profile config/native-performance-baseline-suite-profile.json \
  --expected-commit "$SOURCE_COMMIT" \
  --output baseline-suite-audit.json
```

The audit repeats the source and evidence class but never adds a performance
or release claim.
