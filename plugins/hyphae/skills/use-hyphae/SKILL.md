---
name: use-hyphae
description: Inspect and search a running local Hyphae engine through its bounded read-only Native v2 MCP tools.
---

# Use Hyphae

Use the `hyphae` MCP server as the only data authority for the requested
inspection or search. The server is local by default and exposes a versioned,
bounded, read-only Native v2 tool contract.

Pick the narrowest API key for the tools you need. A restricted key for the
built-in Auditor role at Instance scope supplies the `security.read` authority
required by both security tools without granting mutation authority. The two
search tools require the `search.execute` authority instead, which the
built-in Reader role carries; an Auditor key is denied on them, and a Reader
key is denied on the security tools. Authority always comes from the
authenticated key, never from tool arguments.

The current plugin cannot mutate data. Never claim that a write, deletion,
or proof operation was performed through this MCP surface.

Start with `hyphae_native_capabilities` when the available contract or limits
are not already known. Use only the narrowest matching tool:

- `hyphae_native_capabilities` for bounded product capabilities;
- `hyphae_native_security_status` for redacted access-control counts;
- `hyphae_native_security_principals` for a bounded, cursor-paginated redacted
  principal page;
- `hyphae_native_search_lexical` for one bounded term, phrase, prefix, or
  fuzzy query against a Native physical index; and
- `hyphae_native_search_collection` for one integrated lexical, named-vector,
  or hybrid search with typed filters, sorts, facets, and aggregations. Its
  result reports the per-branch strategy and whether any branch was
  approximate — repeat that recall evidence honestly instead of presenting
  approximate results as exact;
- `hyphae_native_prove_search` for the same integrated search plus a sealed
  offline-verifiable proof and complete witness, hex-encoded within the
  bounded message budget (it requires the `proof.generate` authority the
  Reader role carries, and fails closed with a typed limit error when the
  artifacts exceed the budget); and
- `hyphae_native_verify_proof` to verify one sealed proof, witness, and
  external trusted anchor entirely inside the adapter process — verification
  is trustless and never contacts the service, so a receipt can be checked
  even against an operator you do not trust; and
- `hyphae_native_memory_recall` to recall stored agent memories from one
  collection by bounded lexical retrieval. Expired or forgotten memories
  never return, and with `prove` the response carries the sealed proof,
  witness, and anchor so the recall itself is offline-verifiable. The
  write-scoped memory store and forget tools appear only when the operator
  starts the adapter with ingest allowed; a separate journal tool exists
  only in the Agent Memory profile (`hyphae mcp --profile memory`), not
  here.

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
