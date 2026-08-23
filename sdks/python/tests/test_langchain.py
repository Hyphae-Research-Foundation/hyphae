# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import unittest

try:
    from blake3 import blake3
except ImportError:  # pragma: no cover
    blake3 = None

from hyphae_sdk.langchain import AdapterError, HyphaeVectorStore


class Response:
    def __init__(self, value):
        self.value = value


class FakeEmbeddings:
    def embed_documents(self, texts):
        return [[float(len(text)), 1.0] for text in texts]

    def embed_query(self, text):
        return [float(len(text)), 0.5]


class FakeClient:
    def __init__(self):
        self.ingested = []
        self.searched = []
        self.proven = []
        self.hits = []

    def search_ingest(self, collection, batch):
        self.ingested.append((collection, batch))
        return Response({"documents": len(batch["documents"])})

    def search_collection(self, collection, request):
        self.searched.append((collection, request))
        return Response({"hits": self.hits})

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


@unittest.skipIf(blake3 is None, "the langchain extra is not installed")
class LangChainAdapterTests(unittest.TestCase):
    def store(self, client, **kwargs):
        kwargs.setdefault("metadata_fields", ("kind", "rank"))
        return HyphaeVectorStore(client, 13, FakeEmbeddings(), **kwargs)

    def test_add_texts_ingests_content_derived_identities(self) -> None:
        client = FakeClient()
        store = self.store(client)
        returned = store.add_texts(
            ["alpha document", "beta document"],
            metadatas=[{"kind": "book", "rank": 3, "skip": 1.5}, None],
        )
        (collection, batch), = client.ingested
        self.assertEqual(collection, 13)
        documents = batch["documents"]
        self.assertEqual(len(documents), 2)
        expected = int.from_bytes(blake3(b"alpha document").digest()[:16], "little")
        self.assertEqual(documents[0]["object_id"], expected)
        self.assertEqual(returned[0], str(expected))
        # Only opted-in typed doc-values survive; floats are dropped.
        self.assertEqual(documents[0]["doc_values"], {"kind": "book", "rank": 3})
        self.assertEqual(documents[1]["doc_values"], {})
        self.assertEqual(documents[0]["vectors"], {"exact": [14.0, 1.0]})
        # Re-adding the same content is idempotent on identity.
        store.add_texts(["alpha document"])
        self.assertEqual(
            client.ingested[1][1]["documents"][0]["object_id"], expected
        )

    def test_similarity_search_is_hybrid_and_maps_hits(self) -> None:
        client = FakeClient()
        store = self.store(client)
        ids = store.add_texts(["alpha document"])
        client.hits = [
            {"object_id": int(ids[0]), "score": 0.75, "doc_values": {"kind": "book"}}
        ]
        results = store.similarity_search_with_score("alpha", k=2)
        (_, request), = client.searched
        self.assertIn("lexical", request)
        self.assertEqual(request["vectors"][0]["target"], "exact")
        self.assertEqual(request["limit"], 2)
        (document, score), = results
        self.assertEqual(score, 0.75)
        self.assertEqual(document.page_content, "alpha document")
        self.assertEqual(document.metadata["kind"], "book")
        self.assertEqual(document.metadata["hyphae_object_id"], ids[0])

    def test_vector_only_mode_omits_the_lexical_branch(self) -> None:
        client = FakeClient()
        store = self.store(client, hybrid=False)
        store.similarity_search("query", k=1)
        (_, request), = client.searched
        self.assertNotIn("lexical", request)

    def test_proved_search_seals_the_proof_digest_into_metadata(self) -> None:
        client = FakeClient()
        store = self.store(client, prove=True)
        ids = store.add_texts(["alpha document"])
        client.hits = [{"object_id": int(ids[0]), "score": 1.0, "doc_values": {}}]
        (document,) = store.similarity_search("alpha", k=1)
        (operation, arguments), = client.proven
        self.assertEqual(operation, "search_collection")
        self.assertEqual(arguments["collection"], 13)
        self.assertEqual(
            document.metadata["hyphae_proof_blake3"],
            blake3(b"proof-bytes").digest().hex(),
        )
        self.assertEqual(store.last_proof["witness"], b"witness-bytes")

    def test_from_texts_requires_the_client_and_collection(self) -> None:
        with self.assertRaises(AdapterError):
            HyphaeVectorStore.from_texts(["text"], FakeEmbeddings())


if __name__ == "__main__":
    unittest.main()
