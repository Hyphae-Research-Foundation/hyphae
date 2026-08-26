# SPDX-License-Identifier: Apache-2.0

"""Optional model-provider layer with declared-provider attestation records.

Every provider call returns its payload together with a
``DeclaredProviderRecord`` whose envelope replicates the core ``HYATTS01``
attestation format byte-exactly, so the engine's pure verifier accepts it.
The record proves what was sent and received — never that the provider
computed it deterministically. That distinction is the point: locally
attested models (``hyphae-embed``) carry the replayable ``AttestedLocal``
class instead.

This module needs the optional ``providers`` extra (``hyphae-sdk[providers]``)
for the ``blake3`` digest; importing it without the extra raises a clear
error. Transport is the standard library only.
"""

from __future__ import annotations

import json
import os
import urllib.request
from dataclasses import dataclass

try:
    from blake3 import blake3 as _blake3
except ImportError as _error:  # pragma: no cover - exercised without extras
    _blake3 = None
    _IMPORT_ERROR = _error
else:
    _IMPORT_ERROR = None

_ATTESTATION_MAGIC = b"HYATTS01"
_MAX_NAME_BYTES = 256


class ProviderError(Exception):
    """Fail-closed provider-layer failure."""


def _digest(data: bytes) -> bytes:
    if _blake3 is None:
        raise ProviderError(
            "the providers extra is not installed: pip install 'hyphae-sdk[providers]'"
        ) from _IMPORT_ERROR
    return _blake3(data).digest()


def _name(value: str) -> bytes:
    encoded = value.encode("utf-8")
    if not encoded or len(encoded) > _MAX_NAME_BYTES:
        raise ProviderError("attestation name is unbounded")
    return len(encoded).to_bytes(2, "little") + encoded


@dataclass(frozen=True)
class DeclaredProviderRecord:
    """One declared-provider attestation, envelope-compatible with HYATTS01."""

    provider: str
    model: str
    request_digest: bytes
    response_digest: bytes

    def envelope(self) -> bytes:
        """Returns the canonical HYATTS01 declared-provider envelope."""
        if len(self.request_digest) != 32 or len(self.response_digest) != 32:
            raise ProviderError("attestation digests must be 32 bytes")
        return (
            _ATTESTATION_MAGIC
            + b"\x02"
            + _name(self.provider)
            + _name(self.model)
            + self.request_digest
            + self.response_digest
        )

    def envelope_hex(self) -> str:
        """Returns the canonical envelope as lowercase hex."""
        return self.envelope().hex()


def _record(provider: str, model: str, request: bytes, response: bytes) -> DeclaredProviderRecord:
    return DeclaredProviderRecord(
        provider=provider,
        model=model,
        request_digest=_digest(request),
        response_digest=_digest(response),
    )


def _post_json(url: str, payload: dict, headers: dict[str, str], timeout: float) -> tuple[bytes, bytes]:
    request_bytes = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=request_bytes,
        headers={"Content-Type": "application/json", **headers},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return request_bytes, response.read()
    except OSError as error:
        raise ProviderError(f"provider request failed: {error}") from error


class OllamaProvider:
    """Local Ollama embeddings with declared-provider attestation records."""

    def __init__(self, base_url: str = "http://127.0.0.1:11434", *, timeout: float = 120.0) -> None:
        self._base_url = base_url.rstrip("/")
        self._timeout = timeout

    def embed(self, model: str, texts: list[str]) -> tuple[list[list[float]], DeclaredProviderRecord]:
        """Embeds ``texts`` and returns vectors with the attestation record."""
        if not texts:
            raise ProviderError("no texts to embed")
        request_bytes, response_bytes = _post_json(
            f"{self._base_url}/api/embed",
            {"model": model, "input": texts},
            {},
            self._timeout,
        )
        decoded = json.loads(response_bytes)
        vectors = decoded.get("embeddings")
        if not isinstance(vectors, list) or len(vectors) != len(texts):
            raise ProviderError("provider returned an unexpected embedding shape")
        record = DeclaredProviderRecord(
            provider="ollama",
            model=model,
            request_digest=_digest(request_bytes),
            response_digest=_digest(response_bytes),
        )
        return vectors, record


class OpenAiProvider:
    """OpenAI embeddings with declared-provider attestation records."""

    def __init__(
        self,
        api_key: str | None = None,
        *,
        base_url: str = "https://api.openai.com/v1",
        timeout: float = 120.0,
    ) -> None:
        key = api_key if api_key is not None else os.environ.get("OPENAI_API_KEY", "")
        if not key:
            raise ProviderError("an OpenAI API key is required")
        self._api_key = key
        self._base_url = base_url.rstrip("/")
        self._timeout = timeout

    def embed(self, model: str, texts: list[str]) -> tuple[list[list[float]], DeclaredProviderRecord]:
        """Embeds ``texts`` and returns vectors with the attestation record."""
        if not texts:
            raise ProviderError("no texts to embed")
        request_bytes, response_bytes = _post_json(
            f"{self._base_url}/embeddings",
            {"model": model, "input": texts},
            {"Authorization": f"Bearer {self._api_key}"},
            self._timeout,
        )
        decoded = json.loads(response_bytes)
        data = decoded.get("data")
        if not isinstance(data, list) or len(data) != len(texts):
            raise ProviderError("provider returned an unexpected embedding shape")
        vectors = [entry.get("embedding") for entry in data]
        if any(not isinstance(vector, list) for vector in vectors):
            raise ProviderError("provider returned an unexpected embedding shape")
        record = DeclaredProviderRecord(
            provider="openai",
            model=model,
            request_digest=_digest(request_bytes),
            response_digest=_digest(response_bytes),
        )
        return vectors, record


class DigitalOceanProvider:
    """DigitalOcean Inference embeddings with declared-provider records."""

    def __init__(
        self,
        api_key: str | None = None,
        *,
        base_url: str = "https://inference.do-ai.run/v1",
        timeout: float = 120.0,
    ) -> None:
        key = api_key if api_key is not None else os.environ.get("DIGITALOCEAN_INFERENCE_KEY", "")
        if not key:
            raise ProviderError("a DigitalOcean inference key is required")
        self._api_key = key
        self._base_url = base_url.rstrip("/")
        self._timeout = timeout

    def embed(self, model: str, texts: list[str]) -> tuple[list[list[float]], DeclaredProviderRecord]:
        """Embeds ``texts`` and returns vectors with the attestation record."""
        if not texts:
            raise ProviderError("no texts to embed")
        request_bytes, response_bytes = _post_json(
            f"{self._base_url}/embeddings",
            {"model": model, "input": texts},
            {"Authorization": f"Bearer {self._api_key}"},
            self._timeout,
        )
        decoded = json.loads(response_bytes)
        data = decoded.get("data")
        if not isinstance(data, list) or len(data) != len(texts):
            raise ProviderError("provider returned an unexpected embedding shape")
        vectors = [entry.get("embedding") for entry in data]
        if any(not isinstance(vector, list) for vector in vectors):
            raise ProviderError("provider returned an unexpected embedding shape")
        return vectors, DeclaredProviderRecord(
            provider="digitalocean-inference",
            model=model,
            request_digest=_digest(request_bytes),
            response_digest=_digest(response_bytes),
        )


__all__ = [
    "DeclaredProviderRecord",
    "DigitalOceanProvider",
    "OllamaProvider",
    "OpenAiProvider",
    "ProviderError",
]
