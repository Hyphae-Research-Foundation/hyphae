# Hyphae Memory on Omarchy

The Omarchy 0.2 desktop client connects to an independently installed Hyphae
service through the dedicated [memory-panel interface](../memory-panel.md).
It provides memory data, scoped queries, proof verification and backup creation.

The service authenticates a separate client token on a separate Unix socket.
Its server-side dispatcher excludes operator and agent-configuration actions,
including indirect activation, upgrade, restore and removal paths. The client
receives no native control credential, general execution endpoint or runtime
installer. See [ADR 0030](../adr/0030-restricted-memory-panel-interface.md).

Hyphae installation, services, model configuration and optional agent
integrations are administered independently. Existing integrations can continue
to share the same memory data. Their lifecycle and configuration belong to the
[Agent Memory application](../product/agent-memory.md), outside the desktop
client's authority.

The separate plugin repository contains QML, its Python standard-library
socket client, the public contract, tests and desktop documentation:
https://github.com/Hyphae-Research-Foundation/hyphae-omarchy

The earlier complete operator interface remains available to independently
managed applications. Its configuration and lifecycle APIs are not accepted by
the dedicated memory-panel socket.
