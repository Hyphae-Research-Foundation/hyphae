# SPDX-License-Identifier: Apache-2.0

"""LlamaIndex adapter: a Hyphae-backed vector store with provable retrieval.

``HyphaeLlamaVectorStore`` mirrors the LangChain adapter over the same
integrated search surface: nodes ingest with their pipeline-computed
embeddings under deterministic content-derived identities, queries run as
hybrid retrieval when the query carries text (BM25 plus the exact vector
branch under deterministic fusion) and as pure vector search otherwise,
and with ``prove=True`` every query runs through the engine's proof
generator. ``last_proof`` then holds the proof, witness, and trusted
anchor for offline verification with ``hyphae proof verify``.

The adapter needs the optional ``llamaindex`` extra
(``hyphae-sdk[llamaindex]``) for ``llama-index-core`` and the ``blake3``
digest. Without ``llama_index.core`` the class still functions with
duck-typed nodes and query objects.
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
    from llama_index.core.schema import TextNode as _TextNode
    from llama_index.core.vector_stores.types import (
        VectorStoreQueryResult as _QueryResult,
    )
except ImportError:  # pragma: no cover

    class _TextNode:  # type: ignore[no-redef]
        """Minimal stand-in when llama_index.core is not installed."""

        def __init__(self, text: str = "", metadata: dict | None = None, id_: str = "") -> None:
            self.text = text
            self.metadata = metadata or {}
            self.id_ = id_

        def get_content(self) -> str:
            return self.text

    class _QueryResult:  # type: ignore[no-redef]
        """Minimal stand-in when llama_index.core is not installed."""

        def __init__(self, nodes=None, similarities=None, ids=None) -> None:
            self.nodes = nodes or []
            self.similarities = similarities or []
            self.ids = ids or []


class AdapterError(Exception):
    """Fail-closed adapter failure."""


def _digest(data: bytes) -> bytes:
    if _blake3 is None:
        raise AdapterError(
            "the llamaindex extra is not installed: pip install 'hyphae-sdk[llamaindex]'"
        ) from _IMPORT_ERROR
    return _blake3(data).digest()


def _object_id(text: str) -> int:
    identity = int.from_bytes(_digest(text.encode("utf-8"))[:16], "little")
    return identity or 1


def _doc_values(metadata: dict | None, fields: tuple[str, ...]) -> dict:
    values = {}
    for name, value in (metadata or {}).items():
        if name in fields and (
            isinstance(value, bool) or isinstance(value, (int, str))
        ):
            values[str(name)] = value
    return values


class HyphaeLlamaVectorStore:
    """LlamaIndex vector store over one Hyphae search collection."""

    stores_text = True
    is_embedding_query = True

    def __init__(
        self,
        client: Any,
        collection: int,
        *,
        vector_target: str = "exact",
        hybrid: bool = True,
        candidate_limit: int = 1000,
        prove: bool = False,
        metadata_fields: tuple[str, ...] = (),
    ) -> None:
        self._client = client
        self._collection = collection
        self._vector_target = vector_target
        self._hybrid = hybrid
        self._candidate_limit = candidate_limit
        self._prove = prove
        self._metadata_fields = tuple(metadata_fields)
        self._nodes: dict[int, Any] = {}
        self.last_proof: dict[str, bytes] | None = None

    @property
    def client(self) -> Any:
        return self._client

    def add(self, nodes: list[Any], **kwargs: Any) -> list[str]:
        """Ingests embedded nodes under content-derived identities."""
        if not nodes:
            return []
        documents = []
        returned = []
        for node in nodes:
            embedding = getattr(node, "embedding", None)
            if not embedding:
                raise AdapterError("every node needs a pipeline-computed embedding")
            text = node.get_content()
            identity = _object_id(text)
            documents.append(
                {
                    "object_id": identity,
                    "text": text,
                    "doc_values": _doc_values(
                        getattr(node, "metadata", None), self._metadata_fields
                    ),
                    "vectors": {self._vector_target: list(embedding)},
                }
            )
            self._nodes[identity] = node
            returned.append(str(identity))
        batch_digest = _digest(
            b"".join(identity.to_bytes(16, "little") for identity in sorted(self._nodes))
        )
        self._client.search_ingest(
            self._collection,
            {
                "idempotency_id": int.from_bytes(batch_digest[:16], "little") or 1,
                "documents": documents,
            },
        )
        return returned

    def delete(self, ref_doc_id: str, **kwargs: Any) -> None:
        """Deletes one document by its content-derived identity."""
        identity = int(ref_doc_id)
        self._client.execute(
            "search_document_delete",
            {
                "collection": self._collection,
                "idempotency_id": identity,
                "object_id": identity,
            },
        )
        self._nodes.pop(identity, None)

    def query(self, query: Any, **kwargs: Any) -> Any:
        """Runs one bounded retrieval; proves it on request."""
        embedding = getattr(query, "query_embedding", None)
        if not embedding:
            raise AdapterError("the query needs an embedding")
        k = int(getattr(query, "similarity_top_k", 4) or 4)
        request: dict = {
            "vectors": [
                {
                    "target": self._vector_target,
                    "query": list(embedding),
                    "candidate_limit": self._candidate_limit,
                    "weight": 1,
                }
            ],
            "limit": k,
        }
        query_text = getattr(query, "query_str", None)
        if self._hybrid and query_text:
            request["lexical"] = {
                "query": str(query_text),
                "candidate_limit": self._candidate_limit,
                "weight": 1,
            }
        proof_digest = None
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
            proof_digest = _digest(proven.value["proof"]).hex()
            result = proven.value["response"].value
        else:
            self.last_proof = None
            result = self._client.search_collection(self._collection, request).value
        nodes = []
        similarities = []
        ids = []
        for hit in result.get("hits", []):
            identity = int(hit["object_id"])
            node = self._nodes.get(identity)
            if node is None:
                node = _TextNode(text="", metadata={}, id_=str(identity))
            metadata = dict(getattr(node, "metadata", {}) or {})
            metadata["hyphae_object_id"] = str(identity)
            for name, value in hit.get("doc_values", {}).items():
                metadata[name] = value
            if proof_digest is not None:
                metadata["hyphae_proof_blake3"] = proof_digest
            node.metadata = metadata
            nodes.append(node)
            similarities.append(float(hit["score"]))
            ids.append(str(identity))
        return _QueryResult(nodes=nodes, similarities=similarities, ids=ids)


__all__ = ["AdapterError", "HyphaeLlamaVectorStore"]
