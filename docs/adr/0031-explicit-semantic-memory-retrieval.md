# ADR 0031: Explicit semantic memory retrieval

Status: Accepted

## Context

In a mixed English/Spanish memory project, equal-weight lexical/vector RRF
can rank a generic lexical overlap ahead of the embedding's best translation
match. A Fedora acceptance case placed the correct translated match sixth
although the same model's vector ranking placed it first. Desktop and agent
callers need an explicit choice without changing existing hybrid semantics.

## Decision

Add semantic mode to the memory recall contracts. It uses the existing native
vector branch and memory/proof operation. Only remove the lexical branch after
successful embedding; retain lexical fallback on worker failure and for browsing.
A semantic profile can select hybrid or semantic as the default for requests
that omit mode. Existing profiles remain hybrid and serialize identically when
that default is selected. Status advertises the selected default.

This adds no transport, search engine, model runner, privilege, wire version or
durable format. Complete proofs retain the existing response-size limit.

## Compatibility

An explicitly semantic profile requires the supporting runtime. Switch it back
to hybrid before downgrading, because older policy parsers reject unknown fields.
Tests cover legacy policy decoding/encoding, explicit and default mode selection,
and preserving lexical fallback when no embedding is available.
