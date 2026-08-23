# SPDX-License-Identifier: Apache-2.0

"""LangChain adapter: a Hyphae-backed vector store with provable retrieval.

``HyphaeVectorStore`` speaks the integrated search surface of a local
Hyphae collection: documents ingest with client-supplied embeddings under
deterministic content-derived identities, and queries run as hybrid
retrieval (BM25 plus the exact vector branch under deterministic fusion) or
as a pure vector search. With ``prove=True`` every search runs through the
engine's proof generator and each returned document carries the sealed
proof's BLAKE3 digest in its metadata — retrieval a third party can verify
offline.

Verification is offline by design: ``last_proof`` holds the proof,
witness, and trusted anchor after every proved search, and
``hyphae proof verify`` (or the MCP ``hyphae_native_verify_proof`` tool)
re-executes the sealed request against the witness without the engine.
Large witnesses exceed the in-product ``proof_verify`` work-unit cap, so
the offline verifiers are the canonical path.

The adapter needs the optional ``langchain`` extra
(``hyphae-sdk[langchain]``) for the ``langchain_core`` base classes and the
``blake3`` digest. Without ``langchain_core`` the class still functions as
a plain store (duck-typed embeddings are accepted), but LangChain-specific
surfaces such as ``as_retriever`` are unavailable.
"""

from __future__ import annotations

from typing import Any

try:  # pragma: no cover - exercised without the extra
    from blake3 import blake3 as _blake3
except ImportError as _error:  # pragma: no cover
    _blake3 = None
    _IMPORT_ERROR: ImportError | None = _error
else:
    _IMPORT_ERROR = None

try:  # pragma: no cover - exercised without the extra
    from langchain_core.documents import Document as _Document
    from langchain_core.vectorstores import VectorStore as _VectorStore
except ImportError:  # pragma: no cover

    class _Document:  # type: ignore[no-redef]
        """Minimal stand-in when langchain_core is not installed."""

        def __init__(self, page_content: str, metadata: dict | None = None) -> None:
            self.page_content = page_content
            self.metadata = metadata or {}

    _VectorStore = object  # type: ignore[assignment]


class AdapterError(Exception):
    """Fail-closed adapter failure."""


def _digest(data: bytes) -> bytes:
    if _blake3 is None:
        raise AdapterError(
            "the langchain extra is not installed: pip install 'hyphae-sdk[langchain]'"
        ) from _IMPORT_ERROR
    return _blake3(data).digest()


def _object_id(text: str, explicit: str | None) -> int:
    """Deterministic content-derived identity, or a caller-supplied integer."""
    if explicit is not None:
        identity = int(explicit)
        if not 0 < identity < 1 << 128:
            raise AdapterError("document id must be a nonzero unsigned 128-bit integer")
        return identity
    identity = int.from_bytes(_digest(text.encode("utf-8"))[:16], "little")
    return identity or 1


def _doc_values(metadata: dict | None, fields: tuple[str, ...]) -> dict:
    """Typed doc-values for the metadata keys the caller opted into; the
    collection schema must declare them."""
    values = {}
    for name, value in (metadata or {}).items():
        if name in fields and (
            isinstance(value, bool) or isinstance(value, (int, str))
        ):
            values[str(name)] = value
    return values


class HyphaeVectorStore(_VectorStore):
    """LangChain vector store over one Hyphae search collection."""

    def __init__(
        self,
        client: Any,
        collection: int,
        embedding: Any,
        *,
        vector_target: str = "exact",
        hybrid: bool = True,
        candidate_limit: int = 1000,
        prove: bool = False,
        metadata_fields: tuple[str, ...] = (),
    ) -> None:
        self._client = client
        self._collection = collection
        self._embedding = embedding
        self._vector_target = vector_target
        self._hybrid = hybrid
        self._candidate_limit = candidate_limit
        self._prove = prove
        self._metadata_fields = tuple(metadata_fields)
        self._texts: dict[int, str] = {}
        self._metadata: dict[int, dict] = {}
        self.last_proof: dict[str, bytes] | None = None

    @property
    def embeddings(self) -> Any:
        return self._embedding

    def add_texts(
        self,
        texts: Any,
        metadatas: list[dict] | None = None,
        ids: list[str] | None = None,
        **kwargs: Any,
    ) -> list[str]:
        """Ingests texts with embeddings under content-derived identities."""
        texts = list(texts)
        if not texts:
            return []
        vectors = self._embedding.embed_documents(texts)
        if len(vectors) != len(texts):
            raise AdapterError("embedding output shape differs")
        documents = []
        returned = []
        for index, text in enumerate(texts):
            identity = _object_id(text, ids[index] if ids else None)
            metadata = metadatas[index] if metadatas else None
            documents.append(
                {
                    "object_id": identity,
                    "text": text,
                    "doc_values": _doc_values(metadata, self._metadata_fields),
                    "vectors": {self._vector_target: vectors[index]},
                }
            )
            self._texts[identity] = text
            self._metadata[identity] = dict(metadata or {})
            returned.append(str(identity))
        batch_digest = _digest(
            b"".join(identity.to_bytes(16, "little") for identity in sorted(self._texts))
        )
        self._client.search_ingest(
            self._collection,
            {
                "idempotency_id": int.from_bytes(batch_digest[:16], "little") or 1,
                "documents": documents,
            },
        )
        return returned

    def _request(self, query: str, k: int) -> dict:
        request: dict = {
            "vectors": [
                {
                    "target": self._vector_target,
                    "query": self._embedding.embed_query(query),
                    "candidate_limit": self._candidate_limit,
                    "weight": 1,
                }
            ],
            "limit": k,
        }
        if self._hybrid:
            request["lexical"] = {
                "query": query,
                "candidate_limit": self._candidate_limit,
                "weight": 1,
            }
        return request

    def similarity_search_with_score(
        self, query: str, k: int = 4, **kwargs: Any
    ) -> list[tuple[Any, float]]:
        """Returns documents with fused scores; proves the search on request."""
        request = self._request(query, k)
        proof_metadata: dict[str, str] = {}
        if self._prove:
            proven = self._client.prove(
                "search_collection",
                {"collection": self._collection, "request": request},
            )
            self.last_proof = {
                "proof": proven.value["proof"],
                "witness": proven.value["witness"],
                "trusted_anchor": proven.value["trusted_anchor"],
            }
            proof_metadata = {
                "hyphae_proof_blake3": _digest(proven.value["proof"]).hex()
            }
            result = proven.value["response"].value
        else:
            self.last_proof = None
            result = self._client.search_collection(self._collection, request).value
        scored = []
        for hit in result.get("hits", []):
            identity = int(hit["object_id"])
            metadata = {
                **self._metadata.get(identity, {}),
                "hyphae_object_id": str(identity),
                **{name: value for name, value in hit.get("doc_values", {}).items()},
                **proof_metadata,
            }
            scored.append(
                (
                    _Document(
                        page_content=self._texts.get(identity, ""),
                        metadata=metadata,
                    ),
                    float(hit["score"]),
                )
            )
        return scored

    def similarity_search(self, query: str, k: int = 4, **kwargs: Any) -> list[Any]:
        return [document for document, _score in self.similarity_search_with_score(query, k)]

    @classmethod
    def from_texts(
        cls,
        texts: list[str],
        embedding: Any,
        metadatas: list[dict] | None = None,
        *,
        client: Any = None,
        collection: int | None = None,
        **kwargs: Any,
    ) -> "HyphaeVectorStore":
        if client is None or collection is None:
            raise AdapterError("from_texts needs client= and collection=")
        store = cls(client, collection, embedding, **kwargs)
        store.add_texts(texts, metadatas)
        return store


__all__ = ["AdapterError", "HyphaeVectorStore"]
