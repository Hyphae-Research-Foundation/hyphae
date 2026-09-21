# SPDX-License-Identifier: CC-BY-SA-4.0
# Multilingual embedding measurement harness

This directory is a standalone measurement subject, explicitly excluded from
the root Cargo workspace. Its optional external Python environment, PyTorch,
Transformers, CUDA libraries, NVIDIA tooling, tokenizer, and model weights are
not Hyphae dependencies and are never required to build, open, query, or
recover a Hyphae data directory.

The initial subject is `Qwen/Qwen3-Embedding-0.6B`. The checked-in
[`manifest template`](manifests/qwen3-embedding-0.6b.template.json) deliberately
contains no revision or fabricated digest and has status
`template-unverified`. Both the launcher and checker reject it. A networked
controlled acquisition must resolve a full 40-character repository commit,
fetch the model and recursive tree API endpoints using that asserted commit,
fetch the model card through its commit-specific raw URL, retain the exact
HTTPS source URLs, response headers, body bytes, sizes, and SHA-256 digests,
and download the snapshot for that commit. Mutable local Git remote refs are
not independent evidence of origin authenticity. The runtime harness never
uses the network.

## Identity and lanes

The populated manifest binds the repository, asserted full commit, every
snapshot file size and SHA-256, operator-retained API response bytes, model
config, tokenizer files, and a safetensors-only policy. Unknown files,
unmanifested files, symlinks, Python model code, mutable revisions, legacy
pickle weight formats, `auto_map`, digest changes, and
`trust_remote_code=true` all fail closed.

Population and every later verification rehash the retained HTTP bodies and
check that the model response names the repository and commit. Ordinary files
must match blob OIDs in the retained tree response; LFS files must match its
SHA-256 and size. The acquisition record also retains the acquisition-tool
digest and TLS-validation policy. This is retained acquisition evidence, not a
claim that a checksum can replay or independently reauthenticate the historical
HTTPS session. A 40-hex value or mutable local ref without these retained bytes
cannot become `verified`.

License evidence is deliberately separate. The model API's
`cardData.license=apache-2.0` is retained as an SPDX metadata declaration; it
does not assert that the snapshot bundles an Apache license text. A separately
checksummed, reviewed legal evidence record identifies whether text is present
and records the product-claim policy. If that policy requires bundled text and
none is present, `product_claims_allowed` must be false. In v1, even a positive
operator legal review cannot unlock product claims because no approved signed
trust-anchor contract exists. Harness receipts remain claim-empty in every
case.

Every generated receipt has status
`operator-attested-non-authoritative-measurement`. Retained acquisition bytes,
legal review, and `nvidia-smi` GPU observations are operator assertions, not
independent authenticity, legal-authority, or hardware-attestation anchors.
Their fields cannot promote a receipt or the checked-in placeholder into
product evidence.

The frozen plan records:

- output dimensions 384, 768, and 1024;
- separate FP32, BF16, and FP16 inference lanes;
- canonical little-endian FP32 output after dimension truncation and L2
  normalization, regardless of inference lane;
- exact query and passage instructions, tokenizer settings, last-token
  pooling, truncation/chunking behavior, and tokenizer/config file digests;
- a padded-token budget and maximum batch size for every precision lane;
- two warmups and seven raw measured samples for all nine lane/dimension
  cells.

The v1 plan bytes are frozen at
`295e8f7da8b632dec5ec9f199820e6986a06ae6f0af7197420c6ed91037f75a6`.
Its exact task, 32 repetitions, source/expanded record limits, two warmups,
seven samples, 14,400-second timeout, left padding, right truncation,
tokenizer/chunk token limits, and all three lane names, dtypes, token budgets,
and batch limits are constants in the checker rather than adjustable bounds.
The corpus contains exactly 24 source
records and is frozen byte-for-byte at
`ae87f7a91cf01895d55927e483b29c862cb0a03fab640a97c99cc16401233d89`.

The throughput scope starts after tokenization and includes host-to-device
transfer, model inference, last-token pooling, FP32 conversion, dimension
truncation, normalization, and device-to-host transfer. Model load and
tokenization timings are reported separately. This is not an end-to-end Hyphae
ingest benchmark.

## Controlled acquisition

Capture the operator-retained commit-specific responses in a controlled
networked step. The output directory must not exist:

```bash
python3 benchmarks/multilingual-embedding-harness/acquire_evidence.py \
  --revision "<FULL_40_CHARACTER_REVISION>" \
  --output-dir "/srv/hyphae-models/evidence/qwen3-embedding-0.6b/<FULL_REVISION>"
```

The command creates `acquisition.json`, retained response bodies, a retained
copy of the acquisition tool, and a `legal-evidence.review-required.json`
starting point. That legal file is not accepted as reviewed evidence. An
authorized review must verify the model card, declaration, applicable terms,
bundled-text status, and product-claim policy, then produce a `status=reviewed`
record with reviewer, UTC time, and notes.

After the same controlled task has produced an ordinary-file snapshot, populate
the manifest without network access:

```bash
python3 benchmarks/multilingual-embedding-harness/populate_manifest.py \
  --template "$PWD/benchmarks/multilingual-embedding-harness/manifests/qwen3-embedding-0.6b.template.json" \
  --model-dir "/srv/hyphae-models/qwen3-embedding-0.6b/<FULL_REVISION>" \
  --revision "<FULL_40_CHARACTER_REVISION>" \
  --acquisition-record "/srv/hyphae-models/evidence/qwen3-embedding-0.6b/<FULL_REVISION>/acquisition.json" \
  --legal-evidence "/srv/hyphae-models/evidence/qwen3-embedding-0.6b/<FULL_REVISION>/legal-evidence.reviewed.json" \
  --output "/srv/hyphae-models/manifests/qwen3-embedding-0.6b.json"
```

Population hashes all files, rejects unsupported content, verifies retained API
and legal evidence, and then performs a complete second verification. The
portable evidence directory must remain available for execution and independent
receipt checking; a manifest alone is not acquisition or legal evidence.

## Run on RTX

Create the reference environment outside this repository. It must provide
PyTorch with CUDA, Transformers, Tokenizers, and Safetensors. The receipt
captures the external Python binary digest, every declared file and aggregate
digest for every installed Python distribution, and unions of loaded Python
modules and native libraries sampled at recorded phase boundaries. It also
records exact required-package versions, CUDA and cuDNN versions,
NVIDIA driver, GPU name/UUID/PCI identity/compute capability, VRAM, power and
clock limits, precision controls, and before/after GPU state. This includes the
Torch, Transformers, Tokenizers, Safetensors, NVIDIA wheel, bundled CUDA, and
loaded driver/runtime bytes visible at those phase boundaries. This is not a
continuous loader trace, and the receipt does not claim to capture a library
that was loaded and unloaded entirely between samples.

The real RTX measurement command is:

```bash
python3 benchmarks/multilingual-embedding-harness/run.py \
  --python "/opt/hyphae-qwen3-embedding/bin/python" \
  --model-dir "/srv/hyphae-models/qwen3-embedding-0.6b/<FULL_REVISION>" \
  --manifest "/srv/hyphae-models/manifests/qwen3-embedding-0.6b.json" \
  --plan "$PWD/benchmarks/multilingual-embedding-harness/plans/qwen3-multilingual-v1.json" \
  --corpus "$PWD/benchmarks/multilingual-embedding-harness/corpora/multilingual-throughput-smoke-v1.json" \
  --nvidia-smi "/usr/bin/nvidia-smi" \
  --acquisition-record "/srv/hyphae-models/evidence/qwen3-embedding-0.6b/<FULL_REVISION>/acquisition.json" \
  --legal-evidence "/srv/hyphae-models/evidence/qwen3-embedding-0.6b/<FULL_REVISION>/legal-evidence.reviewed.json" \
  --gpu-index 0 \
  --output "/srv/hyphae-models/receipts/qwen3-embedding-0.6b-rtx.json"
```

Before importing another harness module, `run.py` re-executes itself with
`-I -B` and a new empty `-X pycache_prefix` directory. It then copies the
harness source, model, manifest, plan, corpus, acquisition package, legal
record, Python executable, and `nvidia-smi` into one private stage using
no-follow file descriptors and explicit byte bounds. Cache directories,
`.pyc`, `.pyo`, writable staged files, extra files, and digest drift fail
validation. Bounded relative Python file symlinks may resolve only within the
approved venv or base-Python root; their link text and resolved target identity
plus target size and SHA-256 are bound into the manifest, while only regular
read-only target bytes enter the symlink-free stage. Absolute links, escapes,
loops, directory, device, and other non-regular targets fail closed. Every link
and target is walked with no-follow directory descriptors and revalidated before
and after copying. The portable stage manifest binds every staged path, size,
SHA-256, executed harness file, executable method, source-link record, and
bytecode policy.
Before creating or copying the stage, the launcher no-follow stats every actual
harness, model, evidence, legal, plan/corpus, and resolved executable source.
Every model file must equal its declared manifest size, and count/aggregate
bounds are checked first. Accepted acquisition evidence is independently
bounded to 18 responses, 16 MiB per body, and 64 MiB total; nested tree files
must be exhaustive under `recursive=true&expand=true`.

The launcher starts staged `contained_run.py` and then the staged reference
subject from copied staged Python bytes. The original Python path is never a
contained executable. When the supplied Python belongs to a venv,
the launcher also preflights and stages its base standard library,
`pyvenv.cfg`, and non-bytecode site-packages. Every wheel `RECORD` path must
resolve inside the venv and is staged, including console scripts and metadata
paths. Escapes, unsafe symlinks, and bytecode are rejected. The copied staged
interpreter is the first contained executable and resolves only the staged
venv. The symlink rule admits normal relative stdlib layouts such as
`_sysconfigdata__linux_x86_64-linux-gnu.py`; all staged results remain regular
files. Base `sitecustomize.py` and `usercustomize.py` startup hooks are lstat'd
but deliberately omitted rather than imported into the measured environment.
The child receives the stage
through an inherited directory descriptor, and `nvidia-smi` runs from its
staged read-only copy. The stage is validated before both executions, by the
subject, after measurement, before publication, and after publication.
Original source paths are not reopened by the staged runner.
An interpreter without bounded canonical `pyvenv.cfg`, or with
`include-system-site-packages` other than `false`, is rejected before the unit
starts.

Read-only modes alone are not treated as immutability against the same UID. A
real run is admitted only through a transient systemd user service on cgroup
v2. The fixed unit denies IP networking, permits only Unix address families,
enables `NoNewPrivileges` and `RestrictSUIDSGID`, makes system, home, and staged
inputs read-only, caps memory at 32 GiB with no swap, caps tasks at 64, caps
each writable file at 32 MiB, and caps the only writable temporary filesystem
at 128 MiB. There is no contained host-writable path. The contained entry can
send only a bounded provisional result over inherited stdout after receiving a
one-run challenge over inherited stdin. Unit teardown and timeout kill the
complete cgroup, including descendants that escape the immediate process
group. Only the outer public launcher, after teardown proof, removes the
challenge, constructs the non-authoritative receipt, and atomically publishes
it. If the user manager, cgroup v2, network policy, mount sandbox, resource
property, control channel, or teardown proof is unavailable, no receipt is
published.

The staged entry requires its real, effective, saved, and filesystem uid/gid to
equal the launcher's exact non-root uid/gid. It reads `/proc/self/status` and
fails closed unless `NoNewPrivs` is active and `CapEff`, `CapPrm`, `CapInh`, and
`CapAmb` are all zero. Staged Python and `nvidia-smi` are regular files with no
setuid/setgid bits or `security.capability` file capability. A user service's
kernel capability bounding set may remain nonzero because systemd 255 user
managers cannot enforce `CapabilityBoundingSet=`; it is not a usable capability
set and cannot be acquired under the zero-set, no-new-privileges,
restricted-SUID/SGID, and privilege-free executable controls.

Host execution is denied by `NoExecPaths=/`. The exact `ExecPaths` exceptions
are the immutable digest-bound stage, existing system library trees, and any
preflighted ELF interpreter path outside those trees. Common host executable
directories remain inaccessible, and staged files other than Python and
`nvidia-smi` have no executable mode bits. Runtime loader paths and bytes are
bounded, digested into the stage identity, and checked after containment;
other loaded native libraries remain explicit
non-authoritative receipt observations. The lifecycle probe attempts a host
helper and succeeds only when execution is denied.

The contained service unsets `DBUS_SESSION_BUS_ADDRESS` and `XDG_RUNTIME_DIR`,
makes the user bus, user-manager private socket, system-manager private socket,
and system D-Bus sockets inaccessible, and denies systemd's `@network-io`
syscall group. The staged runner also attempts to connect to each management
socket and fails if any is reachable, preventing sibling-unit launch through
the user manager.

Before starting the unit, the launcher streams the user manager environment
through a 1 MiB bounded, non-persisted pipe and retains names only. Every
manager name outside the exact locale and CUDA/NVIDIA visibility allowlist is
added to `UnsetEnvironment`; loader, audit, allocator, and Python hook names are
included even when absent. Neither diagnostics nor receipts retain environment
values. The first staged Python checks that expected retained names are present
and that no name outside the frozen manager/systemd allowlists arrived.

Admission does not trust requested unit properties. After the unit becomes
active, the launcher queries systemd and requires the effective memory, swap,
tasks, runtime, file, network/address-family, `NoNewPrivileges`, `RestrictSUIDSGID`,
`ProtectControlGroups`, mount, and writable-path values. It also reads the
cgroup v2 `memory.max`, `memory.swap.max`, and `pids.max` controller files and
requires exact values. On success, exception, or timeout it executes a checked
unit stop, waits for inactive/dead (or confirmed unit removal), and requires the
captured cgroup to be absent or have an empty `cgroup.procs`. Failed property,
controller, termination, state, or empty-cgroup proof withholds the receipt.
Runtime duration must be no greater than the frozen limit; read-only,
writable, inaccessible, environment-removal, and temporary-filesystem sets
must match exactly, with an empty host `ReadWritePaths` set. Both `any` and
systemd's canonical `0.0.0.0/0 ::/0` representation are accepted only as full
IP denial.

After `systemd-run --wait`, stop exit 5 for an already-unloaded transient unit
is accepted only when the unit is absent/inactive and the retained cgroup is
absent or has an empty `cgroup.procs`. “Unit not loaded” never substitutes for
those postconditions.

`run.py` exposes only public launcher arguments. The staged internal entry is a
separate script with no receipt-output argument; direct invocation without the
parent challenge fails, and even a successful internal invocation can emit
only a provisional payload that the checker does not accept as a final receipt.

The fixed launch has a four-hour timeout, no stdin, a sanitized environment,
and all Hugging Face/Transformers offline flags. Child stdout, stderr, and
receipt writes are file-size bounded, and timeout kills the complete child
process group. Model and tokenizer loads use
`local_files_only=True`, `trust_remote_code=False`, and safetensors only. The
staged snapshot and retained evidence are semantically verified before the
subject, inside it, after all measurements, and once more before publication.

The tokenizer is explicitly set to left padding and right truncation, and both
values are checked and receipted. After the selected PyTorch logical device has
a CUDA context, the subject maps its process ID to exactly one UUID through
`nvidia-smi`/NVML and queries the physical GPU by that UUID. The receipt binds
the PyTorch logical index to the same UUID and PCI bus identity; physical and
logical numeric indices are not assumed to match.

`PATH`, `PYTHONPATH`, `HOME`, `LD_PRELOAD`, and other ambient launcher state are
not inherited. Only locale, CUDA device-order/visibility, and NVIDIA
visibility/capability names are in the frozen manager allowlist. The receipt
retains names only, never environment values. `LD_PRELOAD`, `LD_AUDIT`,
`LD_LIBRARY_PATH`, Python hooks, allocator hooks, D-Bus discovery, and every
other observed manager name are explicitly unset.
`PYTHONDONTWRITEBYTECODE=1` prevents the measured process from writing
bytecode; `-B` enforces the same policy independently.
`-X pycache_prefix` redirects cache lookup to the isolated empty volatile
directory, which must remain empty and is never part of the evidence identity.
Installed-distribution artifacts are hashed
before framework import and rehashed after measurement; modules and native
libraries are accumulated at the listed capture phases. Inventory identities
are portable labels rather than absolute installation paths. The launcher
binds the Python bytes executed by descriptor and the staged `nvidia-smi` bytes
in the receipt; an independent checker validates those identities from the
portable stage inventory without reopening the original executable or Python
installation paths.

Every distribution-declared file and every file-backed Python module observed
during measurement must resolve to a matching path, size, and SHA-256 in the
stage inventory. Built-in and frozen modules have no file path; any other
module or distribution path outside the stage fails closed.

Independently recheck the result and local weights with:

```bash
python3 benchmarks/multilingual-embedding-harness/check_receipt.py \
  --receipt "/srv/hyphae-models/receipts/qwen3-embedding-0.6b-rtx.json" \
  --manifest "/srv/hyphae-models/manifests/qwen3-embedding-0.6b.json" \
  --model-dir "/srv/hyphae-models/qwen3-embedding-0.6b/<FULL_REVISION>" \
  --plan "$PWD/benchmarks/multilingual-embedding-harness/plans/qwen3-multilingual-v1.json" \
  --corpus "$PWD/benchmarks/multilingual-embedding-harness/corpora/multilingual-throughput-smoke-v1.json" \
  --acquisition-record "/srv/hyphae-models/evidence/qwen3-embedding-0.6b/<FULL_REVISION>/acquisition.json" \
  --legal-evidence "/srv/hyphae-models/evidence/qwen3-embedding-0.6b/<FULL_REVISION>/legal-evidence.reviewed.json"
```

Each measured cell contains raw host and CUDA-event nanoseconds. Only after
those samples yield a checked throughput does the receipt derive idealized
1M, 10M, 50M, 100M, and 1B-vector elapsed-time arithmetic. Every projection is
labelled `non-claim-derived-projection`, states its omitted costs, and cannot be
used as a Hyphae capacity or completion-time claim.

No verified RTX model manifest or result receipt was available for this
change. The checked-in [result metadata placeholder](results/qwen3-embedding-0.6b-rtx.unverified.json)
therefore says `unverified-no-measurement`, contains no revision, hardware,
throughput, or receipt digest, and makes no claim.

## Local contract tests

The tests use only the Python standard library and test-only bytes that are
explicitly not model weights:

```bash
python3 -m unittest discover \
  -s benchmarks/multilingual-embedding-harness/tests \
  -p 'test_*.py' -v
```
