# Native page-generation vacuum evidence

Date: 2026-08-02

Status: implemented and measured current-root physical reclamation; broader
retention, background scheduling, G1, and G7 remain open

Measured source commit:
`e764857bd8157ec43b9202592f621eeeb188e53e`

Measured source tree:
`0dcb24c979a7cad4e16512ae9a227e9ca4da9c00`

Branch at measurement: `codex/native-page-vacuum`

## Change

The native runtime now replaces an append-only page file with one immutable,
smaller generation. The candidate copies the catalog, bulk rebuilds the
relational, structure, lexical-search, and ANN namespaces, and retains only
the open head of each relational V2 row chain. It traverses and materializes
the complete candidate as a differential oracle before publication.

`HYCMT002` and `HYROOT02` bind the page generation and nonzero retention-floor
CSN. Generation-one commits and checkpoints retain their exact V1 encodings.
The strict vacuum transaction uses one empty kernel maintenance mutation,
publishes the candidate through the WAL, advances one global CSN, resets the
generation-keyed buffer pool, and removes the prior page file only after WAL
synchronization. A candidate that is not smaller is removed without a WAL
record, CSN, or generation change.

Recovery authenticates the WAL before selecting a page file. A surviving WAL
precommit without a terminal commit receives a synchronized abort record.
Recovery validates the selected retained generation before removing canonical
temporary, orphan, or retired page-generation files. Page-like noncanonical
directory entries fail closed.

## Correctness evidence

The runtime passed all 141 tests and strict Clippy over all targets and
features with warnings denied. Focused coverage proves:

- byte-compatible V1 and round-trip/malformed V2 WAL and root-manifest codecs;
- buffer-pool separation of equal page IDs in different generations;
- exact catalog, relational, structure, lexical-search, and ANN equality
  before vacuum, after publication, and after reopen;
- preservation of ANN build identity and ordered hits;
- V2 row history reduction to one open head with its original row begin CSN;
- continued use of already materialized `NativeSnapshot` values;
- explicit rejection of a detached writer below the retention floor;
- V2 checkpoint publication and mixed V1/V2 manifest-chain recovery;
- deterministic prior state at candidate/precommit boundaries and complete
  vacuum state at synchronized-WAL/post-publication boundaries;
- unread removal of a corrupt orphan candidate without WAL authority; and
- exact no-op behavior when a rebuilt candidate is not smaller.

The six-boundary matrix reopens every interrupted directory twice. This
verifies that aborting a surviving precommit and cleaning page candidates
leaves a stable directory rather than a one-open-only repair.

## Windows release observation

The [machine-readable receipt](native-page-vacuum-windows.json) was produced
from the clean measured source commit with Rust 1.96.0 on
`x86_64-pc-windows-msvc`, release profile, concurrency one. The deterministic
corpus contains 64 relational rows with nine versions each, 64 structure keys,
64 lexical documents, and 64 ANN vectors.

| Observation | Result |
|---|---:|
| Page file | 3,580 → 72 pages |
| File bytes | 58,654,720 → 1,179,648 |
| Physically reclaimed | 3,508 pages / 57,475,072 bytes |
| Reclamation ratio | 97.989% |
| Strict vacuum latency | 29.791 ms |
| Immediate no-op vacuum latency | 12.638 ms |
| Isolated 4 KiB `sync_data` probe | 1.171 ms |
| Warm point read p50, before / after | 1.000 µs / 1.000 µs |
| Warm point read p99, before / after | 1.200 µs / 1.300 µs |
| Reopen verification | passed |

The vacuum and no-op are measured maintenance operations in milliseconds, not
microsecond paths. The point read remained in the microsecond domain on this
run. The isolated file-sync probe is a same-filesystem observation, not a
decomposition of the page and WAL synchronization inside vacuum.

## Remaining boundary

This is a current-root retention policy. It does not preserve restartable
pre-floor snapshots, collect unreferenced blobs, truncate authenticated WAL
history, retain multiple generations, schedule background work, or prove
concurrent/saturated/background-interference behavior. One Windows run does
not establish p99.9 stability, cold behavior, filesystem portability, or a
universal G7 latency threshold. The vertical advances G1 substrate work but
closes no phase gate.
