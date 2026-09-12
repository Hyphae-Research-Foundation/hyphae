# Dedicated local memory interface

The memory-panel interface lets a desktop client access memory data with a
separate credential and Unix socket. Install and operate Hyphae independently
of the client. The existing Agent Memory operator commands remain separate.
A source build containing `hyphae memory-panel` is required; the published
Hyphae 3.0.0 registry packages predate this interface.

Build the reviewed source with Rust 1.96.0 and locked dependencies:

```bash
cargo build --locked --release -p hyphae-cli
```

Use that executable for the commands below, or install the reviewed build on
PATH as `hyphae`. Existing memory installations can retain their data and
services. For a new installation, initialize memory independently:

```bash
hyphae agent ui <<'JSON'
{"schema":"hyphae-omarchy-control-v1","id":1,"operation":"setup","arguments":{"enable_service":true}}
JSON
```

Provision a dedicated connection as the same user who runs the memory service
and desktop. The command creates private parent directories and a new credential;
it refuses to replace an existing connection file:

```bash
hyphae memory-panel init \
  --config "${XDG_CONFIG_HOME:-$HOME/.config}/hyphae-panel/client.json" \
  --socket "${XDG_RUNTIME_DIR}/hyphae-panel/memory.sock"
hyphae memory-panel serve \
  --config "${XDG_CONFIG_HOME:-$HOME/.config}/hyphae-panel/client.json"
```

`XDG_RUNTIME_DIR` must name the current user's runtime directory. The socket
path must fit the Unix address bound (at most 100 bytes for this interface).
The connection file contains its endpoint and a dedicated 256-bit OS-random
token. Keep its contents private. A desktop client receives this file only;
it uses neither the native memory socket nor an operator/reader/writer key.
Run the listener in your own session or independently managed user service.
The desktop plugin does not create, start, stop or update that service.

SIGINT or SIGTERM closes the listener and removes its socket. If a forced kill
leaves a stale socket, stop the old process and remove that socket explicitly
before starting again. Startup refuses to replace an existing path or listener.
The credential remains stable until the user deliberately provisions a new one.

## Operations and authority

The service authenticates every request and admits only:

| Operation | Effect |
| --- | --- |
| `status`, `projects` | Read memory status and project identifiers |
| `recall`, `list` | Retrieve scoped memories; optionally verify the complete query proof |
| `store`, `forget` | Explicit memory record changes with server-assigned provenance |
| `backups`, `backup` | List and create backups in the memory backup directory's private `panel` subdirectory |

Backups in this interface have server-generated IDs and destinations. Use the
independent Hyphae application to restore or export them. Successful proved
queries return verified proof/witness/anchor digests; generated temporary proof
files are removed. The underlying native proof remains bounded to 16 MiB.
Proofs establish retrieval at a snapshot, not the truth of remembered text.

The socket has no operator, agent configuration, capture policy, service,
credential management, runtime/model installation, restore, arbitrary path,
generic native or proxy operation. The dispatcher calls fixed memory functions
directly. Rejected operations cannot fall through to the broader operator API.
The dedicated token is not a native credential and is never registered with the
native authorization registry.

The server restricts socket and credential access to their owner, checks peer
UID, limits concurrency to four requests, bounds request/response size to
64 KiB/1 MiB, and applies input/operation/output deadlines of 5/120/5 seconds.
This is API capability separation, not an OS sandbox for arbitrary processes
already running with the user's filesystem permissions.

The machine-readable contract is
[`native-memory-panel-v1.json`](../contracts/native-memory-panel-v1.json).
[ADR 0030](adr/0030-restricted-memory-panel-interface.md) describes the boundary.
The real-socket test exercises denied operations with a valid panel credential,
wrong credentials, input bounds, private-path preservation, memory writes,
project isolation, complete proof verification and data backups:

```bash
cargo test --locked -p hyphae-cli --test memory_panel
```
