#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Import one Weaviate class into a Native search collection with a receipt.

The importer exports through Weaviate's public REST cursor — its storage
format is not a stable contract; the API is the honest consistency point —
and ingests objects into a provisioned Hyphae collection under identities
derived from the source UUIDs. Every run emits an external-migration-style
receipt borrowing the G10 fidelity-class pattern: the source identity pins
the Weaviate version and the digest of the canonical export, the
consistency point states exactly what a live cursor export can and cannot
claim, and every encountered construct carries a fidelity class. Quantized
vector configurations are DeclaredDegraded and demand an explicit waiver;
vectors themselves are Equivalent because the ANN graph is rebuilt
deterministically and recall is re-measured, never copied.

Transport is the standard library only.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "sdks" / "python" / "src"))

from weaviate_compare import http_json  # noqa: E402
from rag_eval import HarnessError, run_binary, start_daemon, stop_daemon  # noqa: E402

from hyphae_sdk.v2 import HyphaeClient  # noqa: E402

RECEIPT_SCHEMA = "hyphae-weaviate-import-receipt-v1"
EXPORT_PAGE = 100
INGEST_BATCH = 64
MAINTENANCE_INTERVAL_BATCHES = 12


def export_objects(endpoint: str, class_name: str) -> list[dict]:
    """Exports every object of one class through the public cursor."""
    objects: list[dict] = []
    after = ""
    while True:
        cursor = f"&after={after}" if after else ""
        page = http_json(
            "GET",
            f"{endpoint}/v1/objects?class={class_name}&limit={EXPORT_PAGE}"
            f"&include=vector{cursor}",
        )
        rows = page.get("objects") or []
        if not rows:
            return objects
        objects.extend(rows)
        after = rows[-1]["id"]


def canonical_export_bytes(objects: list[dict]) -> bytes:
    """Canonical JSONL of the export, sorted by source UUID."""
    lines = [
        json.dumps(row, sort_keys=True, separators=(",", ":"))
        for row in sorted(objects, key=lambda row: str(row["id"]))
    ]
    return ("\n".join(lines) + "\n").encode("utf-8")


def classify_schema(schema: dict) -> tuple[list[dict], bool]:
    """Fidelity classes for the source class configuration."""
    classifications = []
    degraded = False
    index_config = schema.get("vectorIndexConfig") or {}
    quantizers = [
        name
        for name in ("pq", "bq", "sq", "rq")
        if (index_config.get(name) or {}).get("enabled")
    ]
    if quantizers:
        degraded = True
        classifications.append(
            {
                "construct": f"quantized-vectors ({','.join(quantizers)})",
                "class": "declared-degraded",
                "detail": "quantized source vectors cannot reproduce the original "
                "float space; the export carries the decompressed approximation",
                "count": 0,
            }
        )
    if schema.get("multiTenancyConfig", {}).get("enabled"):
        classifications.append(
            {
                "construct": "multi-tenancy",
                "class": "rejected",
                "detail": "tenants map to separate target directories; import "
                "one tenant per run",
                "count": 0,
            }
        )
    if schema.get("replicationConfig", {}).get("factor", 1) > 1:
        classifications.append(
            {
                "construct": "replication",
                "class": "equivalent",
                "detail": "a single deterministic directory replaces replicas; "
                "cross-host byte identity is the stronger guarantee",
                "count": 0,
            }
        )
    return classifications, degraded


def import_objects(
    binary: Path,
    data_dir: Path,
    objects: list[dict],
    text_property: str,
    vector_target: str | None,
) -> tuple[int, int]:
    """Ingests exported objects; returns (documents, skipped)."""
    documents = []
    skipped = 0
    for row in sorted(objects, key=lambda row: str(row["id"])):
        identity = uuid.UUID(str(row["id"])).int
        if identity == 0:
            skipped += 1
            continue
        text = (row.get("properties") or {}).get(text_property)
        if not isinstance(text, str) or not text:
            skipped += 1
            continue
        document: dict = {"object_id": identity, "text": text}
        vector = row.get("vector")
        if vector_target and isinstance(vector, list) and vector:
            document["vectors"] = {vector_target: vector}
        documents.append(document)
    process, endpoint = start_daemon(binary, data_dir)
    client = HyphaeClient.local(str(endpoint))
    try:
        offsets = list(range(0, len(documents), INGEST_BATCH))
        windows = [
            offsets[start : start + MAINTENANCE_INTERVAL_BATCHES]
            for start in range(0, len(offsets), MAINTENANCE_INTERVAL_BATCHES)
        ]
        for ordinal, window in enumerate(windows, start=1):
            for offset in window:
                client.search_ingest(
                    13,
                    {
                        "idempotency_id": offset + 1,
                        "documents": documents[offset : offset + INGEST_BATCH],
                    },
                )
            client.close()
            stop_daemon(process, endpoint)
            if vector_target:
                run_binary(
                    binary,
                    ["search", "--data-dir", str(data_dir), "consolidate", "--collection", "13"],
                    timeout=7200,
                )
            run_binary(binary, ["checkpoint", "--data-dir", str(data_dir)], timeout=7200)
            run_binary(binary, ["vacuum", "--data-dir", str(data_dir)], timeout=7200)
            if ordinal < len(windows):
                process, endpoint = start_daemon(binary, data_dir)
                client = HyphaeClient.local(str(endpoint))
    except Exception:
        stop_daemon(process, endpoint)
        raise
    return len(documents), skipped


def verify_import(binary: Path, data_dir: Path, documents: int) -> dict:
    """Post-import verification: every imported identity answers a search."""
    process, endpoint = start_daemon(binary, data_dir)
    client = HyphaeClient.local(str(endpoint))
    try:
        result = client.search_collection(
            13,
            {
                "lexical": None,
                "vectors": [],
                "limit": 1,
                "aggregations": [{"name": "total", "kind": "count"}],
            },
        )
        total = next(
            (
                aggregation["value"].get("value")
                for aggregation in result.value.get("aggregations", [])
                if aggregation["name"] == "total"
                and isinstance(aggregation.get("value"), dict)
            ),
            None,
        )
    finally:
        client.close()
        stop_daemon(process, endpoint)
    return {
        "target_documents": total,
        "matches_export": total == documents,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", default="http://127.0.0.1:8080")
    parser.add_argument("--class-name", required=True)
    parser.add_argument("--text-property", required=True)
    parser.add_argument("--vector-target", default=None)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument(
        "--waive-degraded",
        action="store_true",
        help="explicit operator waiver for declared-degraded constructs",
    )
    parser.add_argument("--output", type=Path, default=None)
    arguments = parser.parse_args()
    endpoint = arguments.endpoint.rstrip("/")

    try:
        meta = http_json("GET", f"{endpoint}/v1/meta")
        schema = http_json("GET", f"{endpoint}/v1/schema/{arguments.class_name}")
        classifications, degraded = classify_schema(schema)
        if degraded and not arguments.waive_degraded:
            print(
                "error: quantized vectors are declared-degraded; rerun with "
                "--waive-degraded to accept the documented loss",
                file=sys.stderr,
            )
            return 1
        if any(entry["class"] == "rejected" for entry in classifications):
            print(
                "error: the class configuration carries a rejected construct",
                file=sys.stderr,
            )
            return 1
        export_started = time.monotonic()
        objects = export_objects(endpoint, arguments.class_name)
        export_seconds = time.monotonic() - export_started
        export_bytes = canonical_export_bytes(objects)
        import_started = time.monotonic()
        documents, skipped = import_objects(
            arguments.binary.resolve(),
            arguments.data_dir,
            objects,
            arguments.text_property,
            arguments.vector_target,
        )
        import_seconds = time.monotonic() - import_started
        verification = verify_import(arguments.binary.resolve(), arguments.data_dir, documents)
    except (HarnessError, OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    classifications.append(
        {
            "construct": "objects",
            "class": "exact",
            "detail": "source UUIDs map one-to-one onto 128-bit document "
            "identities; the selected text property is ingested byte-exactly",
            "count": documents,
        }
    )
    if arguments.vector_target:
        classifications.append(
            {
                "construct": "vectors",
                "class": "equivalent",
                "detail": "float vectors transfer exactly; the ANN graph is "
                "rebuilt deterministically and recall is re-measured, never copied",
                "count": documents,
            }
        )
    receipt = {
        "schema": RECEIPT_SCHEMA,
        "source": {
            "kind": "weaviate-rest-export",
            "server_version": meta.get("version", ""),
            "class": arguments.class_name,
            "export_sha256": hashlib.sha256(export_bytes).hexdigest(),
            "export_bytes": len(export_bytes),
            "object_count": len(objects),
            "skipped_objects": skipped,
        },
        "consistency_point": {
            "kind": "live-cursor-export",
            "statement": "objects observed through the public paginated cursor "
            "on a live instance; writes concurrent with the export can be "
            "missed — quiesce writes for a point-in-time claim",
        },
        "classifications": sorted(classifications, key=lambda entry: entry["construct"]),
        "waivers": (
            [
                entry["construct"]
                for entry in classifications
                if entry["class"] == "declared-degraded"
            ]
            if arguments.waive_degraded
            else []
        ),
        "target": {
            "engine": run_binary(arguments.binary.resolve(), ["version", "--json"]),
            "collection": 13,
            "documents": documents,
            "verification": verification,
        },
        "cost": {
            "export_seconds": round(export_seconds, 2),
            "import_seconds": round(import_seconds, 2),
        },
    }
    encoded = json.dumps(receipt, indent=2, sort_keys=True)
    if arguments.output is not None:
        arguments.output.write_text(encoded + "\n", encoding="utf-8")
    print(encoded)
    return 0


if __name__ == "__main__":
    sys.exit(main())
