#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Capture operator-retained commit-specific responses for offline checking."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import tempfile
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from contract import (
    ACQUISITION_EVIDENCE_KIND,
    ACQUISITION_SCHEMA,
    ContractError,
    HEX40,
    LEGAL_EVIDENCE_SCHEMA,
    MAX_ACQUISITION_TOTAL_BODY_BYTES,
    MODEL_API_PURPOSE,
    MODEL_CARD_PURPOSE,
    MODEL_REPOSITORY,
    SPDX,
    TREE_API_PURPOSE,
)

MAX_RESPONSE_BYTES = 16 * 1024 * 1024
MAX_TREE_PAGES = 16


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    return parser.parse_args()


def _headers(response: Any) -> dict[str, str]:
    retained = {}
    for name in sorted(set(response.headers.keys()), key=str.lower):
        values = response.headers.get_all(name) or []
        retained[name.lower()] = "\n".join(values)
    if not retained:
        raise ContractError("Hugging Face response did not contain headers")
    return retained


def _next_link(value: str | None) -> str | None:
    if value is None:
        return None
    for item in value.split(","):
        match = re.search(r"<([^>]+)>", item)
        if match is not None and re.search(r'rel="?next"?', item) is not None:
            return match.group(1)
    return None


def _fetch(url: str, purpose: str, body_path: str) -> tuple[dict[str, Any], bytes, str | None]:
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/json",
            "User-Agent": "hyphae-multilingual-embedding-acquisition-v1",
        },
        method="GET",
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            body = response.read(MAX_RESPONSE_BYTES + 1)
            response_url = response.geturl()
            status = response.status
            headers = _headers(response)
    except (OSError, TimeoutError) as error:
        raise ContractError(f"commit-specific Hugging Face acquisition failed: {error}") from error
    if len(body) > MAX_RESPONSE_BYTES:
        raise ContractError("commit-specific Hugging Face response exceeds 16 MiB")
    if response_url != url:
        raise ContractError("commit-specific Hugging Face acquisition redirected")
    if status != 200:
        raise ContractError(f"commit-specific Hugging Face acquisition returned HTTP {status}")
    return (
        {
            "purpose": purpose,
            "request_url": url,
            "response_url": response_url,
            "http_status": status,
            "response_headers": headers,
            "body_path": body_path,
            "body_size_bytes": len(body),
            "body_sha256": hashlib.sha256(body).hexdigest(),
        },
        body,
        _next_link(headers.get("link")),
    )


def acquire(revision: str, output_dir: Path) -> None:
    if HEX40.fullmatch(revision) is None:
        raise ContractError("revision must be a full lowercase 40-character commit")
    if not output_dir.is_absolute() or not output_dir.parent.is_dir():
        raise ContractError("output directory and its parent must be absolute")
    if output_dir.exists():
        raise ContractError("output directory already exists")
    temporary = Path(tempfile.mkdtemp(prefix=".embedding-acquisition-", dir=output_dir.parent))
    try:
        responses = []
        bodies = []
        model_url = (
            f"https://huggingface.co/api/models/{MODEL_REPOSITORY}/revision/{revision}"
            "?expand=cardData&expand=sha"
        )
        response, body, _ = _fetch(model_url, MODEL_API_PURPOSE, "model-api.json")
        responses.append(response)
        bodies.append((response["body_path"], body))

        model_card_url = (
            f"https://huggingface.co/{MODEL_REPOSITORY}/raw/{revision}/README.md"
        )
        response, body, _ = _fetch(
            model_card_url, MODEL_CARD_PURPOSE, "model-card.md"
        )
        model_card_response = response
        responses.append(response)
        bodies.append((response["body_path"], body))

        next_url = (
            f"https://huggingface.co/api/models/{MODEL_REPOSITORY}/tree/{revision}"
            "?recursive=true&expand=true"
        )
        for page in range(MAX_TREE_PAGES):
            response, body, next_link = _fetch(
                next_url, TREE_API_PURPOSE, f"tree-api-{page + 1:04d}.json"
            )
            responses.append(response)
            bodies.append((response["body_path"], body))
            if next_link is None:
                break
            commit_base = (
                f"https://huggingface.co/api/models/{MODEL_REPOSITORY}/tree/{revision}?"
            )
            if not next_link.startswith(commit_base):
                raise ContractError("tree pagination left the commit-specific endpoint")
            next_url = next_link
        else:
            raise ContractError("commit-specific Hugging Face tree exceeds the page bound")

        if sum(len(body) for _, body in bodies) > MAX_ACQUISITION_TOTAL_BODY_BYTES:
            raise ContractError("commit-specific response aggregate exceeds 64 MiB")

        for relative, body in bodies:
            (temporary / relative).write_bytes(body)
        tool_bytes = Path(__file__).read_bytes()
        (temporary / "acquisition-tool.py").write_bytes(tool_bytes)
        record = {
            "$comment": SPDX,
            "schema": ACQUISITION_SCHEMA,
            "status": "captured",
            "evidence_kind": ACQUISITION_EVIDENCE_KIND,
            "repository": MODEL_REPOSITORY,
            "revision": revision,
            "captured_at_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "capture": {
                "tool": "acquire_evidence.py",
                "tool_path": "acquisition-tool.py",
                "tool_sha256": hashlib.sha256(tool_bytes).hexdigest(),
                "https_validation": "python-default-context-hostname-and-certificate",
                "authentication": "public-read-no-credentials",
            },
            "responses": responses,
        }
        record_path = temporary / "acquisition.json"
        record_path.write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
        model_body = json.loads(bodies[0][1])
        card_data = model_body.get("cardData") if isinstance(model_body, dict) else None
        legal_template = {
            "$comment": SPDX,
            "schema": LEGAL_EVIDENCE_SCHEMA,
            "status": "review-required",
            "repository": MODEL_REPOSITORY,
            "revision": revision,
            "acquisition_record_sha256": hashlib.sha256(record_path.read_bytes()).hexdigest(),
            "declared_license": {
                "source": "hugging-face-model-api.cardData.license",
                "upstream_value": card_data.get("license") if isinstance(card_data, dict) else None,
                "spdx": "Apache-2.0" if isinstance(card_data, dict) and card_data.get("license") == "apache-2.0" else None,
                "model_metadata_body_sha256": responses[0]["body_sha256"],
            },
            "model_card": {
                "path": "README.md",
                "source_url": model_card_response["response_url"],
                "body_sha256": model_card_response["body_sha256"],
            },
            "bundled_license_text": {"status": "unreviewed", "path": None, "sha256": None},
            "policy": {
                "license_text_required_for_product_claims": True,
                "product_claims_allowed": False,
                "determination": "Legal review has not been completed.",
            },
            "review": {"reviewer": None, "reviewed_at_utc": None, "notes": None},
        }
        (temporary / "legal-evidence.review-required.json").write_text(
            json.dumps(legal_template, indent=2) + "\n", encoding="utf-8"
        )
        os.replace(temporary, output_dir)
    except Exception:
        for child in temporary.iterdir():
            child.unlink(missing_ok=True)
        temporary.rmdir()
        raise


def main() -> int:
    arguments = parse_args()
    try:
        acquire(arguments.revision, arguments.output_dir)
    except (ContractError, OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"retained acquisition evidence written to {arguments.output_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
