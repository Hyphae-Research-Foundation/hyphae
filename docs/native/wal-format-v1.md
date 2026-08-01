# Native WAL format v1

Status: normative target contract; block/record framing, append, integrity
chain, incomplete-tail repair, and the first typed transaction envelope are
implemented experimentally; checkpoint and group-commit coordination remain
pending

The WAL is the only transaction authority for the three native engines. It
records one cross-engine transaction, not three engine-specific commits.

## Block layout

- Block size: 65,536 bytes.
- Header size: 112 bytes.
- Maximum block payload: 65,424 bytes.
- Multi-block records are forbidden. Large values must use blob references.

| Offset | Width | Field |
|---:|---:|---|
| 0 | 8 | ASCII magic `HYWAL001` |
| 8 | 2 | format version `1` |
| 10 | 2 | header length `112` |
| 12 | 4 | flags |
| 16 | 8 | block sequence |
| 24 | 8 | first record LSN |
| 32 | 8 | last record LSN |
| 40 | 4 | payload length |
| 44 | 4 | CRC32C with integrity fields zeroed |
| 48 | 32 | previous complete-block digest |
| 80 | 32 | BLAKE3 digest of header and payload with this field zeroed |
| 112 | variable | record payload followed by zero padding |

LSN is the byte offset of a record header in the logical WAL stream. Block
sequences and complete record LSNs are strictly increasing.

## Record header

Every record begins with:

| Width | Field |
|---:|---|
| u32 | total record length |
| u32 | body length |
| u8 | record kind |
| u8 | engine: kernel `0`, relational `1`, structure `2`, search `3` |
| u16 | flags |
| u64 | record LSN |
| 128 bits | transaction ID |
| u32 | record CRC32C |
| u32 | reserved zero |

Record kinds are `BEGIN`, `MUTATION`, `COMMIT`, `ABORT`, `CHECKPOINT`, and
`CATALOG`. Unknown kinds or versions fail closed.

## Transaction records

`BEGIN` contains:

- snapshot/read CSN;
- catalog version;
- transaction logical time in signed UTC microseconds;
- durability class; and
- bounded operation and byte ceilings.

`MUTATION` contains a versioned engine opcode, target `ObjectId`, canonical key
bytes, canonical value/reference bytes, and the expected prior version when
conflict detection requires it.

`COMMIT` contains:

- read CSN and assigned commit CSN;
- catalog version;
- mutation count and aggregate mutation bytes;
- logical commit time;
- BLAKE3 digest of the ordered canonical mutation records; and
- root-set generation to be published.

`ABORT` is advisory and never makes preceding mutations visible.

## Ordering and atomicity

- A transaction has exactly one `BEGIN`, zero or more `MUTATION` records, and
  at most one terminal `COMMIT` or `ABORT`.
- Empty user commits are rejected.
- A committed transaction's mutation count and digest must match.
- Reusing a transaction ID with identical contents returns the original
  receipt. Reuse with different contents is an idempotency conflict.
- A commit CSN is unique and strictly increasing.
- Engine mutations become visible only when the root set named by `COMMIT` is
  installed and `global_visible_csn` advances.

## Durability classes

- `strict`: write the transaction and synchronize before acknowledgement.
- `group`: combine multiple transaction blocks in one synchronization, then
  acknowledge each included commit.
- `memory`: publish without synchronization; recovery may lose the acknowledged
  suffix and receipts must identify that risk.

All benchmark and API receipts name the durability class.

## Recovery

1. Verify the last checkpoint and its root manifest.
2. Scan blocks from the checkpoint LSN.
3. Truncate only an incomplete physical tail.
4. Reject every complete corrupt block, record, sequence, digest chain,
   transaction boundary, or content digest.
5. Ignore complete transactions without a valid commit.
6. Replay committed transactions in CSN order.
7. Verify or rebuild the committed root set before advancing visibility.

Recovery never guesses an opcode or skips an unknown committed mutation.

## Checkpoints

A `CHECKPOINT` record binds checkpoint CSN, root-manifest generation, complete
root digest, and prior checkpoint LSN. WAL deletion is allowed only when every
retained snapshot and replica requirement is newer than the candidate segment.

## Verification

Required tests cover golden blocks, all record kinds, transaction idempotency,
cross-engine ordering, torn writes at every byte, complete corruption,
sequence/digest divergence, unknown opcode rejection, group synchronization
receipts, checkpoint replay, and bounded recovery.
