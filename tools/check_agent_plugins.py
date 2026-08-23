#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fail-closed structural checker for the Claude Code and Codex plugins."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
CANONICAL_MCP_ARGS = ["mcp", "--base-url", "http://127.0.0.1:8787"]
API_KEY = re.compile(r"hyp1_[0-9a-f]{32}_[0-9a-f]{64}")
EXPECTED_TOOL_NAMES = (
    "hyphae_native_capabilities",
    "hyphae_native_security_status",
    "hyphae_native_security_principals",
    "hyphae_native_search_lexical",
    "hyphae_native_search_collection",
    "hyphae_native_prove_search",
    "hyphae_native_verify_proof",
    "hyphae_native_search_ingest",
    "hyphae_native_memory_store",
    "hyphae_native_memory_recall",
    "hyphae_native_memory_forget",
)
# Write-scoped tools are absent unless the operator starts the adapter
# with --allow-ingest; hosts and the shared corpus see this subset.
WRITE_TOOL_NAMES = (
    "hyphae_native_search_ingest",
    "hyphae_native_memory_store",
    "hyphae_native_memory_forget",
)
DEFAULT_VISIBLE_TOOL_NAMES = tuple(
    name for name in EXPECTED_TOOL_NAMES if name not in WRITE_TOOL_NAMES
)
def expected_annotations(tool_name: str) -> dict[str, bool]:
    return {
        "readOnlyHint": tool_name not in WRITE_TOOL_NAMES,
        "destructiveHint": False,
        "idempotentHint": True,
        "openWorldHint": False,
    }
MEMORY_STORE_INPUT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["collection", "text"],
    "properties": {
        "collection": {"type": "integer", "minimum": 1},
        "text": {"type": "string", "minLength": 1, "maxLength": 4096},
        "ttl_seconds": {"type": "integer", "minimum": 1, "maximum": 316224000},
    },
}
MEMORY_RECALL_INPUT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["collection", "query"],
    "properties": {
        "collection": {"type": "integer", "minimum": 1},
        "query": {"type": "string", "minLength": 1, "maxLength": 4096},
        "limit": {"type": "integer", "minimum": 1, "maximum": 64},
        "prove": {"type": "boolean"},
    },
}
MEMORY_FORGET_INPUT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["collection", "id"],
    "properties": {
        "collection": {"type": "integer", "minimum": 1},
        "id": {"type": "string", "pattern": "^[0-9]+$"},
    },
}
EXPECTED_EXECUTION = {"taskSupport": "forbidden"}
EXPECTED_MCP_CASES = [
    {
        "id": "capabilities-read",
        "tool": "hyphae_native_capabilities",
        "arguments": {},
        "expect": "success",
        "assert": {"pointer": "/product_api_version", "type": "integer"},
    },
    {
        "id": "security-status-read",
        "tool": "hyphae_native_security_status",
        "arguments": {},
        "expect": "success",
        "assert": {
            "pointer": "/schema",
            "equals": "hyphae-native-access-control-status-v1",
        },
    },
    {
        "id": "principal-page-read",
        "tool": "hyphae_native_security_principals",
        "arguments": {"limit": 1},
        "expect": "success",
        "assert": {
            "pointer": "/schema",
            "equals": "hyphae-native-security-principals-v1",
        },
    },
    {
        "id": "prompt-authority-rejected",
        "tool": "hyphae_native_security_status",
        "arguments": {"role": "owner"},
        "expect": "invalid_request",
        "assert": {"pointer": "/error/code", "equals": "invalid_request"},
    },
    {
        "id": "search-lexical-requires-search-authority",
        "tool": "hyphae_native_search_lexical",
        "arguments": {"index": 1, "kind": "term", "query": "rust"},
        "expect": "authorization_denied",
        "assert": {"pointer": "/error/code", "equals": "authorization_denied"},
    },
    {
        "id": "search-collection-requires-search-authority",
        "tool": "hyphae_native_search_collection",
        "arguments": {"collection": 1, "lexical": {"query": "rust"}},
        "expect": "authorization_denied",
        "assert": {"pointer": "/error/code", "equals": "authorization_denied"},
    },
    {
        "id": "prove-search-requires-proof-authority",
        "tool": "hyphae_native_prove_search",
        "arguments": {"collection": 1, "lexical": {"query": "rust"}},
        "expect": "authorization_denied",
        "assert": {"pointer": "/error/code", "equals": "authorization_denied"},
    },
    {
        "id": "verify-proof-rejects-malformed-artifacts",
        "tool": "hyphae_native_verify_proof",
        "arguments": {"proof_hex": "00", "witness_hex": "00", "anchor_hex": "00"},
        "expect": "invalid_request",
        "assert": {"pointer": "/error/code", "equals": "invalid_request"},
    },
]
EMPTY_INPUT_SCHEMA = {
    "type": "object",
    "properties": {},
    "additionalProperties": False,
}
CURSOR_SCHEMA = {
    "type": ["string", "null"],
    "maxLength": 128,
    "pattern": r"^hysec1:[1-9][0-9]*:principal:[0-9a-f]{32}$",
}
PRINCIPAL_INPUT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "properties": {
        "cursor": CURSOR_SCHEMA,
        "limit": {
            "type": "integer",
            "minimum": 1,
            "maximum": 1000,
            "default": 100,
        },
    },
}
VERIFY_PROOF_INPUT_SCHEMA = {'type': 'object',
 'additionalProperties': False,
 'required': ['proof_hex', 'witness_hex', 'anchor_hex'],
 'properties': {'proof_hex': {'type': 'string', 'pattern': '^([0-9a-f]{2})*$'},
                'witness_hex': {'type': 'string', 'pattern': '^([0-9a-f]{2})*$'},
                'anchor_hex': {'type': 'string', 'pattern': '^[0-9a-f]{64}$'}}}
SEARCH_INGEST_INPUT_SCHEMA = {'type': 'object',
 'additionalProperties': False,
 'required': ['collection', 'idempotency_id', 'documents'],
 'properties': {'collection': {'type': 'integer', 'minimum': 1},
                'idempotency_id': {'type': 'integer', 'minimum': 1},
                'documents': {'type': 'array',
                              'minItems': 1,
                              'maxItems': 256,
                              'items': {'type': 'object',
                                        'additionalProperties': False,
                                        'required': ['id', 'text'],
                                        'properties': {'id': {'oneOf': [{'type': 'integer',
                                                                         'minimum': 1},
                                                                        {'type': 'string',
                                                                         'pattern': '^[0-9]+$'}]},
                                                       'text': {'type': 'string'},
                                                       'doc_values': {'type': 'object',
                                                                      'additionalProperties': {'oneOf': [{'type': 'boolean'},
                                                                                                         {'type': 'integer'},
                                                                                                         {'type': 'string'},
                                                                                                         {'type': 'object',
                                                                                                          'additionalProperties': False,
                                                                                                          'required': ['bytes_hex'],
                                                                                                          'properties': {'bytes_hex': {'type': 'string',
                                                                                                                                       'pattern': '^([0-9a-f]{2})*$'}}}]}},
                                                       'vectors': {'type': 'object',
                                                                   'additionalProperties': {'type': 'array',
                                                                                            'minItems': 1,
                                                                                            'maxItems': 65535,
                                                                                            'items': {'type': 'number'}}}}}}}}
SEARCH_LEXICAL_INPUT_SCHEMA = {'type': 'object',
 'additionalProperties': False,
 'required': ['index', 'kind', 'query'],
 'properties': {'index': {'type': 'integer', 'minimum': 1},
                'kind': {'type': 'string',
                         'enum': ['term', 'phrase', 'prefix', 'fuzzy']},
                'query': {'type': 'string', 'minLength': 1, 'maxLength': 4096},
                'max_distance': {'type': 'integer',
                                 'minimum': 1,
                                 'maximum': 2,
                                 'default': 1},
                'limit': {'type': 'integer',
                          'minimum': 1,
                          'maximum': 1024,
                          'default': 10}}}
SEARCH_COLLECTION_INPUT_SCHEMA = {'type': 'object',
 'additionalProperties': False,
 'required': ['collection'],
 'properties': {'collection': {'type': 'integer', 'minimum': 1},
                'lexical': {'type': ['object', 'null'],
                            'additionalProperties': False,
                            'required': ['query'],
                            'properties': {'query': {'type': 'string',
                                                     'minLength': 1,
                                                     'maxLength': 4096},
                                           'candidate_limit': {'type': 'integer',
                                                               'minimum': 1,
                                                               'maximum': 10000,
                                                               'default': 10},
                                           'weight': {'type': 'integer',
                                                      'minimum': 1,
                                                      'maximum': 1000000,
                                                      'default': 1}}},
                'vectors': {'type': 'array',
                            'maxItems': 16,
                            'items': {'type': 'object',
                                      'additionalProperties': False,
                                      'required': ['target', 'values'],
                                      'properties': {'target': {'type': 'string',
                                                                'minLength': 1,
                                                                'maxLength': 1024},
                                                     'values': {'type': 'array',
                                                                'minItems': 1,
                                                                'maxItems': 65535,
                                                                'items': {'type': 'number'}},
                                                     'candidate_limit': {'type': 'integer',
                                                                         'minimum': 1,
                                                                         'maximum': 10000,
                                                                         'default': 10},
                                                     'weight': {'type': 'integer',
                                                                'minimum': 1,
                                                                'maximum': 1000000,
                                                                'default': 1}}}},
                'filter': {'type': ['object', 'null'],
                           'additionalProperties': True,
                           'required': ['operation'],
                           'properties': {'operation': {'type': 'string',
                                                        'enum': ['match_all',
                                                                 'exists',
                                                                 'compare',
                                                                 'all',
                                                                 'any',
                                                                 'not',
                                                                 'in',
                                                                 'is_null',
                                                                 'like']}},
                           'description': 'Typed doc-value filter: match_all; exists '
                                          '{field}; compare {field, operator: '
                                          'equal|not_equal|less|less_or_equal|greater|greater_or_equal, '
                                          'value}; all/any {filters: [...]}; not '
                                          '{filter}; in {field, values: [...]}; '
                                          'is_null {field}; like {field, pattern '
                                          'with _ and % wildcards}. Unknown keys '
                                          'for the declared operation fail '
                                          'closed.'},
                'sort': {'type': 'array',
                         'maxItems': 8,
                         'items': {'type': 'object',
                                   'additionalProperties': True,
                                   'required': ['source', 'direction', 'missing'],
                                   'properties': {'source': {'type': 'string',
                                                             'enum': ['score',
                                                                      'field']},
                                                  'field': {'type': 'string'},
                                                  'direction': {'type': 'string',
                                                                'enum': ['ascending',
                                                                         'descending']},
                                                  'missing': {'type': 'string',
                                                              'enum': ['first',
                                                                       'last']}}}},
                'facets': {'type': 'array',
                           'maxItems': 8,
                           'items': {'type': 'object',
                                     'additionalProperties': False,
                                     'required': ['field', 'limit'],
                                     'properties': {'field': {'type': 'string',
                                                              'minLength': 1},
                                                    'limit': {'type': 'integer',
                                                              'minimum': 1,
                                                              'maximum': 10000}}}},
                'aggregations': {'type': 'array',
                                 'maxItems': 16,
                                 'items': {'type': 'object',
                                           'additionalProperties': True,
                                           'required': ['name', 'operation'],
                                           'properties': {'name': {'type': 'string',
                                                                   'minLength': 1},
                                                          'operation': {'type': 'string',
                                                                        'enum': ['count',
                                                                                 'sum',
                                                                                 'min',
                                                                                 'max']},
                                                          'field': {'type': 'string'}}}},
                'limit': {'type': 'integer',
                          'minimum': 1,
                          'maximum': 1024,
                          'default': 10},
                'fusion': {'type': ['string', 'null'],
                           'enum': ['weighted_score', None],
                           'description': 'Branch-combination method. Absent '
                                          'means deterministic weighted '
                                          'reciprocal-rank fusion; '
                                          'weighted_score blends branch '
                                          'weights with normalized branch '
                                          'scores.'},
                'parent_dedupe': {'type': ['object', 'null'],
                                  'additionalProperties': False,
                                  'required': ['field', 'first_k'],
                                  'properties': {'field': {'type': 'string',
                                                           'minLength': 1},
                                                 'first_k': {'type': 'integer',
                                                             'minimum': 1,
                                                             'maximum': 100}},
                                  'description': 'First-k-per-parent '
                                                 'deduplication over the final '
                                                 'ranking; hits missing the '
                                                 'field are never '
                                                 'deduplicated.'}}}
ERROR_OUTPUT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["schema", "error"],
    "properties": {
        "schema": {"const": "hyphae-native-mcp-tool-error-v2"},
        "error": {
            "type": "object",
            "additionalProperties": False,
            "required": [
                "code",
                "category",
                "message",
                "retry",
                "transaction_state",
                "request_id",
                "trace_id",
                "object_id",
                "transaction_id",
            ],
            "properties": {
                "code": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "pattern": r"^[a-z][a-z0-9_]*$",
                },
                "category": {
                    "enum": [
                        "invalid-request",
                        "not-found",
                        "conflict",
                        "limit",
                        "deadline",
                        "cancelled",
                        "authorization",
                        "corruption",
                        "unavailable",
                        "io",
                        "internal",
                    ]
                },
                "message": {"type": "string", "minLength": 1, "maxLength": 256},
                "retry": {
                    "enum": [
                        "never",
                        "same-request",
                        "new-snapshot",
                        "after-backoff",
                        "after-recovery",
                        "unknown-commit",
                    ]
                },
                "transaction_state": {
                    "enum": [
                        "none",
                        "active",
                        "rolled-back",
                        "committed",
                        "outcome-unknown",
                    ]
                },
                "request_id": {
                    "type": ["string", "null"],
                    "maxLength": 39,
                    "pattern": r"^(0|[1-9][0-9]{0,38})$",
                },
                "trace_id": {
                    "type": ["string", "null"],
                    "maxLength": 39,
                    "pattern": r"^(0|[1-9][0-9]{0,38})$",
                },
                "object_id": {
                    "type": ["string", "null"],
                    "maxLength": 20,
                    "pattern": r"^[1-9][0-9]{0,19}$",
                },
                "transaction_id": {
                    "type": ["string", "null"],
                    "maxLength": 20,
                    "pattern": r"^[1-9][0-9]{0,19}$",
                },
            },
        },
    },
}


def success_schemas() -> dict[str, dict[str, Any]]:
    positive_integer = {"type": "integer", "minimum": 1}
    nonnegative_integer = {"type": "integer", "minimum": 0}
    return {
        "hyphae_native_capabilities": {
            "type": "object",
            "additionalProperties": False,
            "required": [
                "product_api_version",
                "native_directory_format",
                "logical_catalog_codec_version",
                "catalog_tree_format_version",
                "limits",
            ],
            "properties": {
                "product_api_version": positive_integer,
                "native_directory_format": positive_integer,
                "logical_catalog_codec_version": positive_integer,
                "catalog_tree_format_version": positive_integer,
                "limits": {
                    "type": "object",
                    "additionalProperties": False,
                    "required": [
                        "catalog_items",
                        "catalog_visits",
                        "catalog_bytes",
                        "sql_statement_bytes",
                        "sql_parameters",
                        "sql_rows",
                    ],
                    "properties": {
                        field: positive_integer
                        for field in (
                            "catalog_items",
                            "catalog_visits",
                            "catalog_bytes",
                            "sql_statement_bytes",
                            "sql_parameters",
                            "sql_rows",
                        )
                    },
                },
            },
        },
        "hyphae_native_security_status": {
            "type": "object",
            "additionalProperties": False,
            "required": [
                "schema",
                "bootstrapped",
                "authorization_epoch",
                "principals",
                "assignments",
                "custom_roles",
                "custom_assignments",
                "keys",
                "pending_keys",
                "audit_events",
            ],
            "properties": {
                "schema": {"const": "hyphae-native-access-control-status-v1"},
                "bootstrapped": {"type": "boolean"},
                **{
                    field: nonnegative_integer
                    for field in (
                        "authorization_epoch",
                        "principals",
                        "assignments",
                        "custom_roles",
                        "custom_assignments",
                        "keys",
                        "pending_keys",
                        "audit_events",
                    )
                },
            },
        },
        "hyphae_native_security_principals": {
            "type": "object",
            "additionalProperties": False,
            "required": ["schema", "authorization_epoch", "items", "next_cursor"],
            "properties": {
                "schema": {"const": "hyphae-native-security-principals-v1"},
                "authorization_epoch": nonnegative_integer,
                "items": {
                    "type": "array",
                    "maxItems": 1000,
                    "items": {
                        "type": "object",
                        "additionalProperties": False,
                        "required": ["id", "display_name", "enabled"],
                        "properties": {
                            "id": {"type": "string", "pattern": r"^[0-9a-f]{32}$"},
                            "display_name": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 128,
                            },
                            "enabled": {"type": "boolean"},
                        },
                    },
                },
                "next_cursor": CURSOR_SCHEMA,
            },
        },
        "hyphae_native_search_lexical": {'type': 'object',
         'additionalProperties': False,
         'required': ['hits',
                      'documents_examined',
                      'source_bytes',
                      'token_visits',
                      'token_comparisons',
                      'fuzzy_steps'],
         'properties': {'hits': {'type': 'array',
                                 'maxItems': 1024,
                                 'items': {'type': 'object',
                                           'additionalProperties': False,
                                           'required': ['document_id_hex', 'score'],
                                           'properties': {'document_id_hex': {'type': 'string',
                                                                              'pattern': '^([0-9a-f]{2})*$'},
                                                          'score': {'type': 'number'}}}},
                        'documents_examined': {'type': 'integer', 'minimum': 0},
                        'source_bytes': {'type': 'integer', 'minimum': 0},
                        'token_visits': {'type': 'integer', 'minimum': 0},
                        'token_comparisons': {'type': 'integer', 'minimum': 0},
                        'fuzzy_steps': {'type': 'integer', 'minimum': 0}}},
        "hyphae_native_search_collection": {'type': 'object',
         'additionalProperties': False,
         'required': ['snapshot',
                      'hits',
                      'facets',
                      'aggregations',
                      'vector_branches',
                      'approximate',
                      'total_documents',
                      'eligible_documents',
                      'lexical_candidates',
                      'retrieval_candidates',
                      'matched_candidates'],
         'properties': {'snapshot': {'type': 'object'},
                        'hits': {'type': 'array',
                                 'maxItems': 1024,
                                 'items': {'type': 'object',
                                           'additionalProperties': False,
                                           'required': ['object_id', 'score', 'doc_values'],
                                           'properties': {'object_id': {'type': 'string',
                                                                        'pattern': '^[0-9]+$'},
                                                          'score': {'type': 'number'},
                                                          'doc_values': {'type': 'object',
                                                                         'additionalProperties': {'oneOf': [{'type': 'boolean'},
                                                                                                            {'type': 'integer'},
                                                                                                            {'type': 'string'},
                                                                                                            {'type': 'object',
                                                                                                             'additionalProperties': False,
                                                                                                             'required': ['bytes_hex'],
                                                                                                             'properties': {'bytes_hex': {'type': 'string',
                                                                                                                                          'pattern': '^([0-9a-f]{2})*$'}}}]}}}}},
                        'facets': {'type': 'array'},
                        'aggregations': {'type': 'array'},
                        'vector_branches': {'type': 'array',
                                            'maxItems': 16,
                                            'items': {'type': 'object',
                                                      'additionalProperties': False,
                                                      'required': ['target',
                                                                   'strategy',
                                                                   'approximate',
                                                                   'eligible_documents',
                                                                   'candidate_count',
                                                                   'visited_nodes',
                                                                   'exact_reranked'],
                                                      'properties': {'target': {'type': 'string'},
                                                                     'strategy': {'type': 'string'},
                                                                     'approximate': {'type': 'boolean'},
                                                                     'eligible_documents': {'type': 'integer',
                                                                                            'minimum': 0},
                                                                     'candidate_count': {'type': 'integer',
                                                                                         'minimum': 0},
                                                                     'visited_nodes': {'type': 'integer',
                                                                                       'minimum': 0},
                                                                     'exact_reranked': {'type': 'integer',
                                                                                        'minimum': 0}}}},
                        'approximate': {'type': 'boolean'},
                        'total_documents': {'type': 'integer', 'minimum': 0},
                        'eligible_documents': {'type': 'integer', 'minimum': 0},
                        'lexical_candidates': {'type': 'integer', 'minimum': 0},
                        'retrieval_candidates': {'type': 'integer', 'minimum': 0},
                        'matched_candidates': {'type': 'integer', 'minimum': 0}}},
        "hyphae_native_search_ingest": {'type': 'object',
         'additionalProperties': False,
         'required': ['status', 'snapshot', 'commit', 'documents', 'idempotent_replay'],
         'properties': {'status': {'type': 'string', 'enum': ['committed', 'existing']},
                        'snapshot': {'type': 'object'},
                        'commit': {'type': ['object', 'null']},
                        'documents': {'type': 'integer', 'minimum': 0},
                        'idempotent_replay': {'type': 'boolean'}}},
        "hyphae_native_prove_search": {'type': 'object',
         'additionalProperties': False,
         'required': ['status',
                      'kind',
                      'response',
                      'proof_hex',
                      'witness_hex',
                      'anchor_hex',
                      'proof_bytes',
                      'witness_bytes'],
         'properties': {'status': {'const': 'generated'},
                        'kind': {'type': 'string'},
                        'response': {'type': 'object'},
                        'proof_hex': {'type': 'string', 'pattern': '^([0-9a-f]{2})*$'},
                        'witness_hex': {'type': 'string', 'pattern': '^([0-9a-f]{2})*$'},
                        'anchor_hex': {'type': 'string', 'pattern': '^[0-9a-f]{64}$'},
                        'proof_bytes': {'type': 'integer', 'minimum': 0},
                        'witness_bytes': {'type': 'integer', 'minimum': 0}}},
        "hyphae_native_verify_proof": {'type': 'object',
         'additionalProperties': False,
         'required': ['status',
                      'scope',
                      'kind',
                      'anchor_digest',
                      'proof_digest',
                      'witness_digest',
                      'request_digest',
                      'result_digest',
                      'evidence_digest',
                      'file_count',
                      'directory_count',
                      'total_file_bytes',
                      'semantic_reexecution_performed'],
         'properties': {'status': {'const': 'verified'},
                        'scope': {'type': 'string',
                                  'enum': ['semantic_reexecution', 'artifact_integrity']},
                        'kind': {'type': 'string'},
                        'anchor_digest': {'type': 'string', 'pattern': '^[0-9a-f]{64}$'},
                        'proof_digest': {'type': 'string', 'pattern': '^[0-9a-f]{64}$'},
                        'witness_digest': {'type': 'string', 'pattern': '^[0-9a-f]{64}$'},
                        'request_digest': {'type': 'string', 'pattern': '^[0-9a-f]{64}$'},
                        'result_digest': {'type': 'string', 'pattern': '^[0-9a-f]{64}$'},
                        'evidence_digest': {'type': 'string', 'pattern': '^[0-9a-f]{64}$'},
                        'file_count': {'type': 'integer', 'minimum': 0},
                        'directory_count': {'type': 'integer', 'minimum': 0},
                        'total_file_bytes': {'type': 'integer', 'minimum': 0},
                        'semantic_reexecution_performed': {'type': 'boolean'}}},
        "hyphae_native_memory_store": {
            "type": "object",
            "additionalProperties": False,
            "required": ["status", "id", "expires_at_micros"],
            "properties": {
                "status": {"type": "string", "enum": ["stored"]},
                "id": {"type": "string", "pattern": "^[0-9]+$"},
                "expires_at_micros": {"type": ["integer", "null"]},
            },
        },
        "hyphae_native_memory_recall": {
            "type": "object",
            "additionalProperties": False,
            "required": ["memories", "expired_filtered", "proof"],
            "properties": {
                "memories": {
                    "type": "array",
                    "maxItems": 64,
                    "items": {
                        "type": "object",
                        "additionalProperties": False,
                        "required": ["id", "score", "text"],
                        "properties": {
                            "id": {"type": "string", "pattern": "^[0-9]+$"},
                            "score": {"type": "number"},
                            "text": {"type": "string"},
                        },
                    },
                },
                "expired_filtered": {"type": "integer", "minimum": 0},
                "proof": {
                    "type": ["object", "null"],
                    "additionalProperties": False,
                    "required": ["proof_hex", "witness_hex", "anchor_hex"],
                    "properties": {
                        "proof_hex": {"type": "string", "pattern": "^([0-9a-f]{2})*$"},
                        "witness_hex": {"type": "string", "pattern": "^([0-9a-f]{2})*$"},
                        "anchor_hex": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                    },
                },
            },
        },
        "hyphae_native_memory_forget": {
            "type": "object",
            "additionalProperties": False,
            "required": ["status", "id"],
            "properties": {
                "status": {"type": "string", "enum": ["forgotten"]},
                "id": {"type": "string", "pattern": "^[0-9]+$"},
            },
        },
    }


class AgentPluginValidationError(ValueError):
    """A checked-in agent plugin is incomplete, unsafe, or divergent."""


def fail(message: str) -> None:
    raise AgentPluginValidationError(message)


def load_object(path: Path, root: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"{path.relative_to(root)} is not valid UTF-8 JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{path.relative_to(root)} must contain one JSON object")
    return value


def validate_mcp(value: dict[str, Any]) -> None:
    if set(value) != {"mcpServers"}:
        fail("shared MCP config must contain only mcpServers")
    servers = value["mcpServers"]
    if not isinstance(servers, dict) or set(servers) != {"hyphae"}:
        fail("shared MCP config must define exactly one hyphae server")
    server = servers["hyphae"]
    expected = {
        "type": "stdio",
        "command": "hyphae",
        "args": CANONICAL_MCP_ARGS,
        "env_vars": ["HYPHAE_NATIVE_API_KEY_FILE"],
    }
    if server != expected:
        fail("Claude Code and Codex must use the canonical hyphae stdio server")


def validate_codex(value: dict[str, Any]) -> str:
    if value.get("name") != "hyphae" or value.get("mcpServers") != "./.mcp.json":
        fail("Codex plugin identity or MCP binding is invalid")
    version = value.get("version")
    if not isinstance(version, str) or re.fullmatch(r"\d+\.\d+\.\d+", version) is None:
        fail("Codex plugin version must be strict semver")
    if value.get("license") != "Apache-2.0":
        fail("Codex plugin license must match the repository")
    interface = value.get("interface")
    if not isinstance(interface, dict) or interface.get("developerName") != "Celiums Solutions LLC":
        fail("Codex plugin interface metadata is incomplete")
    if interface.get("capabilities") != ["Read"]:
        fail("Codex plugin must advertise the exact read-only MCP capability")
    long_description = str(interface.get("longDescription", ""))
    if "Auditor" not in long_description or "Instance" not in long_description:
        fail("Codex plugin must recommend an Instance-scoped Auditor API key")
    if "Reader" in long_description:
        fail("Codex plugin must not recommend a Reader API key")
    prompts = interface.get("defaultPrompt")
    if (
        not isinstance(prompts, list)
        or not 1 <= len(prompts) <= 3
        or any(not isinstance(prompt, str) or not prompt or len(prompt) > 128 for prompt in prompts)
    ):
        fail("Codex starter prompts must be one to three bounded strings")
    return version


def validate_claude(value: dict[str, Any], version: str) -> None:
    if value.get("name") != "hyphae" or value.get("version") != version:
        fail("Claude Code plugin identity must match the Codex bundle")
    if value.get("license") != "Apache-2.0":
        fail("Claude Code plugin license must match the repository")
    description = str(value.get("description", ""))
    if "Auditor" not in description or "Instance" not in description:
        fail("Claude Code plugin must recommend an Instance-scoped Auditor API key")
    if "Reader" in description:
        fail("Claude Code plugin must not recommend a Reader API key")


def validate_marketplaces(root: Path, version: str) -> None:
    claude = load_object(root / ".claude-plugin/marketplace.json", root)
    plugins = claude.get("plugins")
    if (
        claude.get("name") != "hyphae"
        or not isinstance(plugins, list)
        or len(plugins) != 1
        or plugins[0].get("name") != "hyphae"
        or plugins[0].get("source") != "./plugins/hyphae"
        or plugins[0].get("version") != version
        or plugins[0].get("license") != "Apache-2.0"
    ):
        fail("Claude Code marketplace is not bound to the checked-in plugin")
    description = str(plugins[0].get("description", ""))
    if "Auditor" not in description or "Instance" not in description:
        fail("Claude Code marketplace must recommend an Instance-scoped Auditor API key")
    if "Reader" in description:
        fail("Claude Code marketplace must not recommend a Reader API key")

    codex = load_object(root / ".agents/plugins/marketplace.json", root)
    entries = codex.get("plugins")
    if (
        not isinstance(entries, list)
        or len(entries) != 1
        or entries[0].get("name") != "hyphae"
        or entries[0].get("source")
        != {"source": "local", "path": "./plugins/hyphae"}
        or entries[0].get("policy")
        != {"installation": "AVAILABLE", "authentication": "ON_INSTALL"}
    ):
        fail("Codex marketplace is not bound to the checked-in plugin")


def validate_skill(plugin: Path, expected_tools: set[str]) -> None:
    skill = plugin / "skills/use-hyphae/SKILL.md"
    text = skill.read_text(encoding="utf-8")
    if not text.startswith("---\n") or "name: use-hyphae" not in text:
        fail("shared Hyphae skill metadata is invalid")
    mentioned_tools = set(re.findall(r"hyphae_[a-z_]+", text))
    if mentioned_tools != expected_tools:
        fail("shared Hyphae skill diverges from the Native MCP tool registry")
    if any(term in text for term in ("hyphae_put", "hyphae_delete", "hyphae_query")):
        fail("shared Hyphae skill advertises an unavailable MCP mutation or query")
    if "Auditor" not in text or "Instance" not in text:
        fail("shared Hyphae skill must recommend an Auditor API key")
    if "search.execute" not in text or "Reader" not in text:
        fail("shared Hyphae skill must document the search-tool authority")


def validate_plugin_readme(plugin: Path) -> None:
    text = (plugin / "README.md").read_text(encoding="utf-8")
    if (
        "HYPHAE_NATIVE_API_KEY_FILE" not in text
        or "Native HTTP v2" not in text
        or "read-only" not in text
        or "HYPHAE_BEARER_TOKEN_FILE" in text
        or "targets the shipped `/v1`" in text
    ):
        fail("plugin setup must document the managed Native v2 read-only boundary")
    if "Auditor" not in text or "Instance" not in text:
        fail("plugin setup must recommend an Instance-scoped Auditor API key")
    if "search.execute" not in text or "Reader" not in text:
        fail("plugin setup must document the search-tool authority")


def validate_contract(contract: dict[str, Any]) -> tuple[str, ...]:
    if set(contract) != {
        "schema",
        "mcp_protocol",
        "tool_schema_version",
        "tool_page_size",
        "resource_limits",
        "cancellation",
        "tools",
    }:
        fail("Native MCP contract envelope is invalid")
    if (
        contract.get("schema") != "hyphae-native-mcp-contract-v2"
        or contract.get("mcp_protocol") != "2025-06-18"
        or contract.get("tool_schema_version") != "hyphae-native-mcp-tools-v4"
    ):
        fail("Native MCP contract versions are invalid")
    if type(contract.get("tool_page_size")) is not int or contract.get("tool_page_size") != 100:
        fail("Native MCP tool page size must be exactly one hundred")
    if contract.get("resource_limits") != {
        "input_bytes": 4 * 1024 * 1024,
        "output_bytes": 4 * 1024 * 1024,
        "active_tool_calls": 1,
        "pending_responses": 1,
    }:
        fail("Native MCP resource limits must be exact and bounded")
    if contract.get("cancellation") != {
        "method": "notifications/cancelled",
        "idempotent": True,
    }:
        fail("Native MCP cancellation contract is invalid")

    tools = contract.get("tools")
    if not isinstance(tools, list) or len(tools) != len(EXPECTED_TOOL_NAMES):
        fail("Native MCP contract tool registry is invalid")
    schemas = success_schemas()
    for index, expected_name in enumerate(EXPECTED_TOOL_NAMES):
        tool = tools[index]
        if not isinstance(tool, dict) or set(tool) != {
            "name",
            "description",
            "inputSchema",
            "outputSchema",
            "annotations",
            "execution",
        }:
            fail("Native MCP tool definition is invalid")
        description = tool.get("description")
        if (
            tool.get("name") != expected_name
            or not isinstance(description, str)
            or not 1 <= len(description) <= 256
        ):
            fail("Native MCP contract tool identities are invalid")
        if tool.get("annotations") != expected_annotations(expected_name):
            fail("Native MCP tool annotations must be exact read-only hints")
        if tool.get("execution") != EXPECTED_EXECUTION:
            fail("Native MCP tasks must be forbidden")
        expected_input = {
            "hyphae_native_security_principals": PRINCIPAL_INPUT_SCHEMA,
            "hyphae_native_search_lexical": SEARCH_LEXICAL_INPUT_SCHEMA,
            "hyphae_native_search_collection": SEARCH_COLLECTION_INPUT_SCHEMA,
            "hyphae_native_prove_search": SEARCH_COLLECTION_INPUT_SCHEMA,
            "hyphae_native_verify_proof": VERIFY_PROOF_INPUT_SCHEMA,
            "hyphae_native_search_ingest": SEARCH_INGEST_INPUT_SCHEMA,
            "hyphae_native_memory_store": MEMORY_STORE_INPUT_SCHEMA,
            "hyphae_native_memory_recall": MEMORY_RECALL_INPUT_SCHEMA,
            "hyphae_native_memory_forget": MEMORY_FORGET_INPUT_SCHEMA,
        }.get(expected_name, EMPTY_INPUT_SCHEMA)
        if tool.get("inputSchema") != expected_input:
            fail(f"Native MCP {expected_name} input schema is invalid")
        expected_output = {
            "type": "object",
            "oneOf": [schemas[expected_name], ERROR_OUTPUT_SCHEMA],
        }
        if tool.get("outputSchema") != expected_output:
            fail(f"Native MCP {expected_name} output schema is invalid or unredacted")
    return EXPECTED_TOOL_NAMES


def validate(root: Path = ROOT) -> dict[str, Any]:
    plugin = root / "plugins/hyphae"
    files = [
        plugin / ".mcp.json",
        plugin / ".codex-plugin/plugin.json",
        plugin / ".claude-plugin/plugin.json",
        plugin / "README.md",
        plugin / "skills/use-hyphae/SKILL.md",
        root / ".claude-plugin/marketplace.json",
        root / ".agents/plugins/marketplace.json",
        root / "contracts/native-mcp-v2.json",
        root / "conformance/mcp/corpus.json",
        root / "conformance/mcp/receipt.schema.json",
    ]
    for path in files:
        if not path.is_file():
            fail(f"required plugin file is missing: {path.relative_to(root)}")
        if API_KEY.search(path.read_text(encoding="utf-8")) is not None:
            fail(f"credential material is forbidden in {path.relative_to(root)}")
    validate_mcp(load_object(plugin / ".mcp.json", root))
    version = validate_codex(load_object(plugin / ".codex-plugin/plugin.json", root))
    if version != "2.0.1":
        fail("agent plugin version must match the bounded 2.0 MCP slice")
    validate_claude(load_object(plugin / ".claude-plugin/plugin.json", root), version)
    validate_marketplaces(root, version)
    validate_contract(load_object(root / "contracts/native-mcp-v2.json", root))
    expected_tools = set(DEFAULT_VISIBLE_TOOL_NAMES)
    corpus = load_object(root / "conformance/mcp/corpus.json", root)
    if (
        set(corpus) != {"schema", "mcp_config", "tool_schema_version", "tools", "cases"}
        or
        corpus.get("schema") != "hyphae-mcp-host-corpus-v1"
        or corpus.get("mcp_config") != "plugins/hyphae/.mcp.json"
        or corpus.get("tool_schema_version") != "hyphae-native-mcp-tools-v4"
        or corpus.get("tools") != list(DEFAULT_VISIBLE_TOOL_NAMES)
        or corpus.get("cases") != EXPECTED_MCP_CASES
    ):
        fail("shared MCP host conformance corpus is invalid")
    if len({case["id"] for case in corpus["cases"]}) != 8:
        fail("shared MCP host conformance case IDs must be unique")
    validate_skill(plugin, expected_tools)
    validate_plugin_readme(plugin)
    return {
        "status": "passed",
        "hosts": ["claude-code", "codex"],
        "mcp_servers": 1,
        "tools": len(expected_tools),
    }


def main() -> int:
    print(json.dumps(validate(), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
