# Native G7 provisional control qualification on DigitalOcean

Date: 2026-08-13

Status: provisional control qualification passed; canonical G7 remains open

Source commit:
`ff188af589eff1f6f15ac4f2e782b43f0868fa21`

Source tree:
`d9f6493e31b8d4c139937dd57092908421256de6`

## Decision

This run establishes a pragmatic, non-canonical G7 control baseline. It does
not close G7. The dedicated-hardware and interference requirements in
`config/native-g7-readiness-profile.json` remain unchanged and open.

The checked
[machine-readable verdict](native-g7-provisional-do-c60-2026-08-13.json)
declares only `G7-provisional-control` and sets `closure_declared` to `false`.

## Environment

- DigitalOcean `c-60-intel` virtual machine in `nyc3`;
- 60 vCPUs, 120 GiB RAM, and 750 GB local storage;
- virtualized, non-dedicated hardware;
- warm state and control background mode only;
- quick hardware calibration after the thorough calibration was rejected as
  unstable;
- 1,000,000 documents and 1,000,000 384-dimensional vectors; and
- source checkout and generated receipts bound to the source commit and tree
  above.

Droplet `592176697` was deleted after the evidence bundle was copied and
verified. It is not an ongoing test host and incurs no continuing compute
charge.

## Contract exercised

The terminal matrix exercised all eleven G7 surfaces at C1, C8, and C32. Each
cell used exactly 1,000,000 measured observations and 100,000 warmup
observations per surface. This is 33,000,000 measured surface observations in
total.

The runner used a 60-second bounded database admission wait. This is the
contract fix for the earlier C8 failure: the policy intentionally exposes one
global I/O slot, while the old runner configured database and product
admission with a zero wait. Concurrent reads therefore failed closed with
`GlobalCapacity` instead of queueing. The bounded wait preserves the capacity
limit and includes queueing time in the measured latency.

All three cells passed:

| Concurrency | Surfaces | Observations per surface | Warmup per surface | ANN recall@10 | Recovery missing | Recovery mismatched |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 11 | 1,000,000 | 100,000 | 1.0 | 0 | 0 |
| 8 | 11 | 1,000,000 | 100,000 | 1.0 | 0 | 0 |
| 32 | 11 | 1,000,000 | 100,000 | 1.0 | 0 | 0 |

For strict group commit, every cell completed 1,000,000 logical commits with
1,000,000 distinct CSNs. Recovery verified every commit with zero missing,
zero mismatched, and zero replayed entries after vacuum and checkpoint.

The ANN selected route accounted for exactly 1,000,000 observations in each
cell:

| Concurrency | Targeted | Generic fallback |
|---:|---:|---:|
| 1 | 1,000,000 | 0 |
| 8 | 251,299 | 748,701 |
| 32 | 818,542 | 181,458 |

## Latency boundary

Latency is recorded but is not normative for this virtual machine. The pilot
used a two-times canonical envelope, and the terminal full run did not assert
the canonical latency thresholds. At C32, several surfaces reached roughly
7–8.4 ms p50, demonstrating why these measurements must not be represented as
dedicated-hardware closure evidence.

The result supports functional concurrency, accounting, ANN recall, and
durable recovery under a million-observation workload. It does not support a
claim that the canonical G7 latency or background-interference matrix passed.

## Evidence integrity

- terminal matrix SHA-256:
  `3e4523dadb308a2696fed0462ecf8d4cb166578c430e84744b11bb60a75147e6`;
- terminal verdict SHA-256 before repository placement:
  `95a78beb031de35a3d2532c7536d76491d6273f7b8ce74c6b3976038bc8c3234`;
- complete evidence bundle SHA-256:
  `760ae09fe409a7ba39222ba9ff55367dc5923620124525b02ef7c75b0f536a47`;
- bundle `SHA256SUMS` SHA-256:
  `1f1a5c3e86eef1a36231f4aa06977d9a1076c473c26914e1ef2380449837197`;
- 79 bundled files passed their recorded SHA-256 checks; and
- provisional runner override SHA-256:
  `f8a687ecfa06b3d2433dbe6d5a92ec69cc423c562d7ea9ebc717bf200a3ceccd`.

The complete bundle includes the terminal receipts, pilots, partials,
calibration, topology, policy, controller logs, failed-attempt diagnostics,
and host snapshot. Only the compact verdict is retained in Git to avoid
turning the repository into an artifact store; the hashes above bind the
external bundle exactly.

## Closure boundary

This evidence deliberately omits:

- dedicated, non-virtualized hardware;
- the background-interference cells;
- accepted thorough hardware calibration; and
- enforcement of the canonical full-run latency thresholds.

Therefore the canonical G7 status remains `open`. The practical product
decision is to stop purchasing dedicated benchmark hardware for this gate,
retain this result as the provisional control qualification, and optimize
only against production-observed bottlenecks before reconsidering canonical
closure.
