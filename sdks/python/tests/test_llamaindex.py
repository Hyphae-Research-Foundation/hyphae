# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import unittest

try:
    from blake3 import blake3
except ImportError:  # pragma: no cover
    blake3 = None

from hyphae_sdk.llamaindex import AdapterError, HyphaeLlamaVectorStore


class Response:
    def __init__(self, value):
        self.value = value


class FakeNode:
    def __init__(self, text, metadata=None, embedding=None):
        self.text = text
        self.metadata = metadata or {}
        self.embedding = embedding

    def get_content(self):
        return self.text


class FakeQuery:
    def __init__(self, embedding, text=None, k=3):
        self.query_embedding = embedding
        self.query_str = text
        self.similarity_top_k = k


class FakeClient:
    def __init__(self):
        self.ingested = []
        self.searched = []
        self.proven = []
        self.executed = []
        self.hits = []

    def search_ingest(self, collection, batch):
        self.ingested.append((collection, batch))
        return Response({"documents": len(batch["documents"])})

    def search_collection(self, collection, request):
        self.searched.append((collection, request))
        return Response({"hits": self.hits})

    def execute(self, operation, arguments):
        self.executed.append((operation, arguments))
        return Response({})

    def prove(self, operation, arguments):
        self.proven.append((operation, arguments))
        return Response(
            {
                "response": Response({"hits": self.hits}),
                "proof": b"proof-bytes",
                "witness": b"witness-bytes",
                "trusted_anchor": bytes(32),
            }
        )


@unittest.skipIf(blake3 is None, "the llamaindex extra is not installed")
class LlamaIndexAdapterTests(unittest.TestCase):
    def store(self, client, **kwargs):
        kwargs.setdefault("metadata_fields", ("kind",))
        return HyphaeLlamaVectorStore(client, 13, **kwargs)

    def test_add_requires_embeddings_and_derives_identities(self) -> None:
        client = FakeClient()
        store = self.store(client)
        with self.assertRaises(AdapterError):
            store.add([FakeNode("no embedding")])
        ids = store.add(
            [FakeNode("alpha node", {"kind": "book", "skip": 2.5}, [1.0, 0.0])]
        )
        (_, batch), = client.ingested
        document = batch["documents"][0]
        expected = int.from_bytes(blake3(b"alpha node").digest()[:16], "little")
        self.assertEqual(document["object_id"], expected)
        self.assertEqual(ids, [str(expected)])
        self.assertEqual(document["doc_values"], {"kind": "book"})
        self.assertEqual(document["vectors"], {"exact": [1.0, 0.0]})

    def test_query_is_hybrid_with_text_and_vector_only_without(self) -> None:
        client = FakeClient()
        store = self.store(client)
        ids = store.add([FakeNode("alpha node", {}, [1.0, 0.0])])
        client.hits = [{"object_id": int(ids[0]), "score": 0.5, "doc_values": {}}]
        result = store.query(FakeQuery([1.0, 0.0], text="alpha", k=2))
        (_, request), = client.searched
        self.assertIn("lexical", request)
        self.assertEqual(request["limit"], 2)
        self.assertEqual(result.ids, [ids[0]])
        self.assertEqual(result.similarities, [0.5])
        self.assertEqual(result.nodes[0].get_content(), "alpha node")
        store.query(FakeQuery([1.0, 0.0]))
        self.assertNotIn("lexical", client.searched[1][1])

    def test_proved_query_seals_the_digest_and_retains_artifacts(self) -> None:
        client = FakeClient()
        store = self.store(client, prove=True)
        ids = store.add([FakeNode("alpha node", {}, [1.0, 0.0])])
        client.hits = [{"object_id": int(ids[0]), "score": 1.0, "doc_values": {}}]
        result = store.query(FakeQuery([1.0, 0.0], text="alpha"))
        self.assertEqual(
            result.nodes[0].metadata["hyphae_proof_blake3"],
            blake3(b"proof-bytes").digest().hex(),
        )
        self.assertEqual(store.last_proof["proof"], b"proof-bytes")

    def test_delete_uses_the_content_derived_identity(self) -> None:
        client = FakeClient()
        store = self.store(client)
        ids = store.add([FakeNode("alpha node", {}, [1.0, 0.0])])
        store.delete(ids[0])
        (operation, arguments), = client.executed
        self.assertEqual(operation, "search_document_delete")
        self.assertEqual(arguments["object_id"], int(ids[0]))


if __name__ == "__main__":
    unittest.main()
