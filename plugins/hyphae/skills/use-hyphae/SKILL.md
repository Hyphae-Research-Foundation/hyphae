---
name: use-hyphae
description: Use a running local Hyphae engine through its bounded MCP tools for structured records, SQL-adjacent queries, lexical retrieval, vector retrieval, or hybrid RAG.
---

# Use Hyphae

Use the `hyphae` MCP server as the only data authority for the requested
operation. The server is local by default and exposes versioned, bounded tools.

Before a write, summarize the intended mutation and obtain the user's approval
unless the current request already explicitly authorizes that exact mutation.
Never infer permission to delete or overwrite data from a read request.

Start with `hyphae_capabilities` when the available contract or limits are not
already known. Prefer the narrowest matching tool:

- `hyphae_get` and `hyphae_query` for structured reads;
- `hyphae_retrieve_lexical`, `hyphae_retrieve_exact`, or
  `hyphae_retrieve_hybrid` for retrieval;
- `hyphae_put`, `hyphae_delete`, `hyphae_put_vectors`, and
  `hyphae_delete_vectors` only for explicit mutations; and
- definition tools only when the requested durable space or index is absent.

Treat returned proof and request identities as evidence. Do not claim that a
result was verified unless the required witness was independently verified.

Never print, echo, inspect, or request a bearer/API-key secret. Credentials are
provided to the `hyphae` process through restricted files or inherited
environment configuration outside tool arguments.

If the MCP server is unavailable, state that the local Hyphae service must be
started and do not substitute another database or fabricate results.
