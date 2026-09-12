# ADR 0030: Restricted memory panel interface

Status: Accepted

## Context

A desktop memory panel needs memory data, verified queries and backups. The
existing Agent Memory operator interface also manages host integrations,
credentials, services and migration. Hiding operator actions in a client does
not restrict the authority of its credential or transport.

## Decision

Expose an independently provisioned Unix socket with a dedicated OS-random
256-bit token and a fixed server-side operation set. The normative contract is
`contracts/native-memory-panel-v1.json`. It is an external desktop contract;
the native engine protocol and durable formats remain unchanged.

The listener accepts only its dedicated token from the same effective user.
The token is never registered as a native or operator credential. Each request
is authenticated and checked against explicit argument types before dispatch.
The dispatcher calls memory operations directly and has no operator command
dispatcher, general native request, shell, subprocess or proxy endpoint.

Only status, project listing, recall/list, explicit store/forget, and backup
listing/creation are admitted. Proof verification happens inside the service
using server-generated artifacts. The client receives verification metadata,
not filesystem paths. Temporary proof files are removed. Backup destinations
are generated inside the configured memory backup directory.

Service lifecycle, runtime and model installation, capture policy, host
configuration, hooks, skills, MCP registrations, credentials and restores are
outside this interface. Independently installed Hyphae can retain those
operator commands. Neither possession of the desktop token nor a crafted
request can dispatch them through the panel socket.

The user provisions the memory application and the panel credential outside
the desktop plugin. A plugin contains only its UI and protocol client and
receives only the panel configuration. An unrestricted native socket or
credential is never its fallback.

## Consequences and limits

This separates API authority; it is not an operating-system sandbox for
unsandboxed processes that already run as the same Unix user. The credential
grants access to all of that user's memory projects, including explicit writes
and backups. It does not prove remembered statements true.

The listener bounds input, output, connection deadlines and concurrency.
Existing sockets and credentials are preserved when provisioning or binding
would overwrite them. Graceful termination removes the listener socket;
after a forced kill the operator removes its stale socket before restarting.

Tests must use the real socket and dedicated credential, reject operator and
generic requests and extra fields, reject wrong credentials, and verify that
denied requests preserve independently configured host files. Memory writes,
queries, proof verification and backups must still work against the real
engine. The existing full operator interface keeps its own regression suite.
