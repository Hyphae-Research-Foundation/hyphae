<!-- SPDX-License-Identifier: Apache-2.0 -->
# Multilingual embedding measurement harness v1

The [standalone multilingual embedding harness](../../benchmarks/multilingual-embedding-harness/README.md)
measures `Qwen/Qwen3-Embedding-0.6B` as an external subject on NVIDIA hardware.
It is excluded from the product workspace and introduces no model, Python,
CUDA, or GPU dependency into the default offline Hyphae product.

The harness is evidence infrastructure, not a shipped embedding provider. A
run requires an operator-populated manifest with an asserted full repository
commit, retained responses from commit-specific Hugging Face model/tree API
URLs, the commit-specific raw model-card URL, and SHA-256 for every local file,
including safetensors weights, configuration, and tokenizer data. Response URLs,
headers, bodies, sizes, and checksums remain portable evidence; ordinary blob
OIDs and LFS SHA-256/size metadata bind the snapshot inventory. They do not
reauthenticate a historical HTTPS session.
Mutable local remote refs are not accepted as origin evidence.

The retained API `cardData.license=apache-2.0` value is an SPDX metadata
declaration, not evidence that a physical license text is bundled. A separate
checksummed legal-review record states whether text exists and whether policy
permits product claims. If policy requires bundled text and it is absent,
product claims remain blocked. Operator legal review cannot unlock claims in
v1 because there is no approved signed trust-anchor contract. Measurement
receipts are always claim-empty and explicitly
`operator-attested-non-authoritative-measurement`.

The exact plan digest is
`295e8f7da8b632dec5ec9f199820e6986a06ae6f0af7197420c6ed91037f75a6`.
The exact 24-record corpus digest is
`ae87f7a91cf01895d55927e483b29c862cb0a03fab640a97c99cc16401233d89`.
Task text, repetitions, samples, warmups, timeout, token settings, and every
lane field, left padding, and right truncation are fixed v1 identities, not
operator-selectable values.

Receipts bind all workload identities and retain raw host/CUDA timing samples
for independent checking. The 1M through 1B vector values are arithmetic
projections derived from a measured cell, explicitly omit non-inference costs,
and are non-claims. No throughput or model digest is checked into this
foundation.

Receipts retain portable byte inventories for every file declared by each
installed Python distribution and the union of modules and native libraries
observed at declared phase boundaries from startup through every lane/dimension
and final cleanup. This is phase sampling, not a continuous loader trace. The
Python and
`nvidia-smi` executable digests are retained separately. Only allowed inherited
locale and CUDA/NVIDIA visibility names are explicit receipt data; environment
values are never receipted or logged. Loader, audit, Python, allocator, D-Bus,
and all other observed manager names are unset. Independent receipt checking
does not require the original absolute Python installation paths.

PyTorch's selected logical CUDA index is mapped from the measurement process to
an NVML UUID and then to a PCI bus identity. The receipt does not assume that
PyTorch logical indices equal physical `nvidia-smi` indices.

Execution uses one content-addressed private stage for harness source, model,
inputs, retained acquisition/legal evidence, Python, and `nvidia-smi`. Files are
copied through no-follow descriptors under byte limits, made read-only, and
validated before and after execution. Staged `contained_run.py` and the
reference subject execute from staged Python bytes; the stage itself is
held through a directory descriptor. The receipt binds the full portable stage
inventory and the files actually executed. `-I -B`, an isolated empty
`pycache_prefix`, and rejection of `__pycache__`, `.pyc`, and `.pyo` prevent
pre-existing harness bytecode from entering the execution closure. Volatile
logs, provisional output, and the empty cache directory are excluded from
evidence identity and separately file-size bounded.

The copied staged Python binary is the first executable inside containment.
For a venv, its base standard library, `pyvenv.cfg`, and non-bytecode
site-packages are preflighted and staged so no mutable original interpreter
path is needed for package resolution. All wheel `RECORD` paths, including
console scripts and metadata, must resolve within the venv root and enter the
stage identity. Relative file symlinks are resolved component by component only
within the approved venv or base-Python root, with bounded link text and chain
depth. Their link text, final target identity, target size, and target SHA-256
enter the stage identity, and the target bytes are copied to a regular read-only
staged file. No-follow descriptor walks revalidate each link and target before
and after the copy. Absolute links, escapes, loops, directory, device, or other
non-regular targets, and bytecode fail closed. Base `sitecustomize.py` and
`usercustomize.py` startup hooks are lstat'd and omitted from the isolated
closure. This admits the normal Debian/Ubuntu relative
`_sysconfigdata__linux_x86_64-linux-gnu.py` base-library layout without allowing
the staged interpreter to reopen the host link.

All distribution-declared files and all file-backed modules observed by the
measurement must resolve to stage-inventory entries with matching bytes.
Non-venv interpreters and venvs without canonical bounded `pyvenv.cfg` data or
`include-system-site-packages=false` fail before contained execution.

Before stage construction, every concrete source is lstat'd and no-follow statted. Model
file sizes must equal the manifest and remain within exact file-count and
aggregate-byte limits. The accepted acquisition record permits at most 18
responses, 16 MiB per body, and 64 MiB in aggregate, and its recursive tree must
exhaustively match nested snapshot files.

Unix read-only modes are not claimed as protection from the same UID. Runtime
requires a transient systemd user service on cgroup v2 with
`NoNewPrivileges`, `RestrictSUIDSGID`, read-only system/home/stage mounts, IP
denial, Unix-only address families, 64 tasks, 32 GiB memory, no swap, a
32 MiB file-size limit, and a 128 MiB writable temporary filesystem. One
parent-owned stdin/stdout channel carries a one-run challenge and bounded
provisional result; the contained unit has no host-writable path. Unit teardown
and timeouts kill the cgroup, including descendants outside the immediate
process group. Missing or unsupported containment fails closed.

The contained entry requires all real, effective, saved, and filesystem
credentials to equal the launcher's exact non-root uid/gid. Before measurement
it reads `/proc/self/status` and fails closed unless `NoNewPrivs` is active and
the effective, permitted, inheritable, and ambient capability sets (`CapEff`,
`CapPrm`, `CapInh`, and `CapAmb`) are all zero. Staged Python and `nvidia-smi`
must have neither setuid/setgid mode bits nor a `security.capability` file
capability. The kernel capability bounding set may be nonzero: systemd 255 user
managers cannot enforce `CapabilityBoundingSet=`, but a bounding bit is not a
usable capability and cannot be acquired through these zero usable sets,
`NoNewPrivileges`, `RestrictSUIDSGID`, and privilege-free staged executables.

`NoExecPaths=/` denies execution by default. `ExecPaths` must exactly contain
the immutable digest-bound stage, existing system library trees, and any
preflighted/digested ELF interpreter path outside those trees. Common host
binary directories are inaccessible, and only staged Python and `nvidia-smi`
have executable mode bits. Loader bytes enter the stage identity, while other
observed native libraries remain explicit
non-authoritative OS-runtime receipt data. The non-GPU lifecycle probe attempts
a host helper and accepts success only when helper execution is denied.

After activation, requested properties are compared with effective systemd
properties, including network/address-family restrictions and
`NoNewPrivileges`, `RestrictSUIDSGID`, and `ProtectControlGroups`. The launcher
independently checks cgroup v2
`memory.max`, `memory.swap.max`, and `pids.max`. Every success, exception, and
timeout performs a checked unit stop, waits for inactive/dead or unit removal,
and verifies that the captured cgroup is absent or `cgroup.procs` is empty.
Receipt publication is withheld if any teardown proof fails.

After `systemd-run --wait`, stop exit 5 for an already-unloaded transient unit
is accepted only when the unit is absent/inactive and the retained cgroup is
absent or has an empty `cgroup.procs`. Active or unverifiable descendants fail.

The public launcher and contained entry are separate. Internal flags are not
accepted by the public CLI. The contained entry cannot construct or write a
final receipt; only the outer parent can validate the one-run challenge,
finalize the non-authoritative schema after teardown, and publish it.

The service unsets D-Bus/runtime-directory discovery, makes user/system D-Bus
and systemd private sockets inaccessible, and denies `@network-io` syscalls.
The runner probes all management socket paths and rejects any successful
connection. Effective read-only, writable, inaccessible, environment-removal,
tmpfs path/size/options, and UMask values must match exact sets with no extra
writable path; effective `ReadWritePaths` must be empty. Runtime may not exceed
the frozen maximum. Full IP denial accepts systemd's canonical
`0.0.0.0/0 ::/0` representation as well as `any`.

Before unit start, manager environment output is read only through a bounded
non-persisted pipe and immediately reduced to names. Every inherited manager
name outside the frozen locale/GPU-visibility allowlist, plus mandatory
loader/audit/Python hooks, is placed in the exact effective
`UnsetEnvironment` set. Values are never logged or receipted. The first staged
Python and descriptor-executed subject each verify their permitted environment
name sets.

The acquisition record, checksummed legal review, and `nvidia-smi` mapping are
operator assertions unless a future contract introduces approved signed trust
anchors. None can change `claim_unlock=forbidden` or promote a receipt into
product evidence.

No verified RTX model/result metadata was available in the repository at this
revision. The checked-in result metadata remains explicitly
`unverified-no-measurement` with null revision, hardware, environment, and
receipt identities and no claims.
