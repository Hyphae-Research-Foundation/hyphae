---
name: use-hyphae
description: Inspect a running local Hyphae engine through its bounded read-only Native v2 MCP tools.
---

# Use Hyphae

Use the `hyphae` MCP server as the only data authority for the requested
inspection. The server is local by default and exposes a versioned, bounded,
read-only Native v2 tool contract.

Use a restricted API key for the built-in Auditor role at Instance scope. That
role supplies the `security.read` authority required by both security tools
without granting mutation authority.

The current plugin cannot mutate data. Never claim that a write, deletion,
query, retrieval, or proof operation was performed through this MCP surface.

Start with `hyphae_native_capabilities` when the available contract or limits
are not already known. Use only the narrowest matching tool:

- `hyphae_native_capabilities` for bounded product capabilities;
- `hyphae_native_security_status` for redacted access-control counts; and
- `hyphae_native_security_principals` for a bounded, cursor-paginated redacted
  principal page.

Treat structured `ProductError` values as the authoritative denial. Never put
`role`, `scope`, `authority`, `api_key`, or similar control fields into tool
arguments: the schemas reject them and the authenticated key alone determines
authority.

Never print, echo, inspect, or request a bearer/API-key secret. Credentials are
provided to the `hyphae` process through restricted files or inherited
environment configuration outside tool arguments. The standard setup passes
only the restricted-file path through `HYPHAE_NATIVE_API_KEY_FILE`.

If the MCP server is unavailable, state that the local Hyphae service must be
started and do not substitute another database or fabricate results.
