# Baseline threat model

Hyphae 0.2.x protects local data integrity, authenticated remote access, and
bounded resource consumption through the packaged CLI/server and explicit
bounded library entry points. Published compatibility entry points retain the
limits documented below. Hyphae assumes the operating system correctly
enforces file ownership and process isolation.

## In scope

- Accidental truncation, partial writes, bit flips, and interrupted recovery.
- Replay, reorder, insertion, deletion, and rollback attempts against log
  history, result proofs, and retrieval proofs.
- Malformed or excessive API input.
- Unauthorized access when the server is explicitly bound beyond loopback.
- Dependency, license, and secret exposure in the source and build pipeline.

## Explicit limitation

A local checkpoint detects corruption and partial manipulation. An attacker
who controls the entire data directory and every trusted checkpoint can
rewrite both history and its local roots. External signatures or anchors may
strengthen that model later, but they are optional and never required for the
base engine.

The accepted result- and retrieval-proof models are described below. The
accepted server model is maintained in
[`server-threat-model.md`](server-threat-model.md).

## Result-proof trust model

Result proof v1 uses a canonical logical snapshot as the complete offline
witness. The verifier checks the proof, checks the snapshot, reexecutes the
embedded operation, and compares the complete result. This detects edits,
insertions, deletions, reordering, truncation, and bit flips in either
artifact.

Rollback and replay detection require the caller to supply an expected anchor
digest that was pinned outside the proof/snapshot pair. Self-consistency alone
is useful for diagnostics but is not trusted verification. The normative
contract is [`result-proof-v1.md`](../provenance/result-proof-v1.md).

## Retrieval-proof trust model

Retrieval proof v1 binds the canonical request, ordered result or abstention,
retrieval semantics, score representation, snapshot checkpoint, and complete
format-2 logical witness. Offline verification reexecutes exact, lexical, or
hybrid retrieval and rejects a mismatched request, witness, rank, score,
modality explanation, or semantics version.

The expected retrieval anchor must also be pinned through a trusted channel.
The normative contract is
[`retrieval-proof-v1.md`](../provenance/retrieval-proof-v1.md).

## Offline verification resource limits

The version 0.2.1 default policy permits snapshot-witness files up to 2 GiB
and retains up to 1 GiB of aggregate decoded logical payload across KV
keys/values, vector-space and vector payloads, and lexical-index definitions.
Exact-vector replay, including the exact branch of hybrid replay, defaults to
1 GiB of candidate key/vector bytes. These are policy defaults, not memory
reservations or immutable hard ceilings. Embedded callers can lower or raise
them; raising a limit expands the caller's resource-exposure envelope, so
applications handling untrusted artifacts should select limits appropriate to
their environment.

The loader preflights file length and declared logical counts on the same open
handle used for canonical verification. It accounts decoded bytes while
scanning and checks the verifier's remaining cooperative deadline inside
record and payload loops. Snapshot readers require a regular file and reject a
same-handle length change observed before verification completes. Keeping the
verified handle open prevents a path replacement from selecting different
bytes, but it cannot make the underlying inode immutable: a same-length
external overwrite after verification remains outside the local isolation
model stated above.

The HTTP server and public clients retain a separate 512 MiB witness-download
default. Enlarging the local verifier bounds does not enlarge the remote
transport policy.

## Storage recovery and maintenance limits

The packaged CLI/server and an embedded
`open_with_limits(StorageLimits::default())` spend one 60-second cooperative
deadline across directory enumeration, snapshot verification/restore,
active-log scan, transaction replay, and lexical rebuild. Aggregate defaults
cap log/snapshot files at 2 GiB, decoded replay/snapshot payload at 1 GiB, and
all major record/frame/transaction collections at documented finite counts.
The writer preflights the same active-log and lexical ceilings before durable
append so accepted writes cannot create a state rejected by a subsequent open
under that policy. The published embedded `open` methods retain their `0.2.0`
compatibility behavior without these new finite ceilings.

Snapshot and compaction under a retained finite policy, or through explicit
`*_with_limits` methods, use one separate 60-second maintenance deadline and
fail before manifest activation when their policy is exhausted. Temporary
snapshot/index files are best-effort deleted on failure. Once a manifest
generation is durably committed, cleanup failure is reported without rolling
back or ambiguously failing the committed state.

These cooperative checks bound Hyphae's own loops; they do not preempt a
single blocking filesystem syscall or Redb commit. Backup layout and manifest
parsing are bounded, but complete backup copy/verify/restore does not yet
share the recovery/maintenance deadline and remains a documented residual
operator boundary.
