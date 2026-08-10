// SPDX-License-Identifier: GPL-3.0-only

//! Canonical public contract documents embedded for validation and generation.

pub mod v1;
pub mod v2;

/// `OpenAPI` 3.1 document for HTTP API version 1.
pub const OPENAPI_V1: &str = include_str!("../assets/openapi/hyphae-v1.yaml");

/// `OpenAPI` 3.1 document for the Native HTTP version 2 adapter.
pub const OPENAPI_V2: &str = include_str!("../assets/openapi/hyphae-v2.yaml");

/// JSON Schema for the complete Native v2 operation vocabulary.
pub const NATIVE_SCHEMA_V2: &str = include_str!("../assets/json-schema/native-v2.schema.json");

/// JSON Schema for Native HTTP v2 Product errors.
pub const PRODUCT_ERROR_SCHEMA_V2: &str =
    include_str!("../assets/json-schema/product-error-v2.schema.json");

/// JSON Schema for provisional Native HTTP v2 read stream records.
pub const READ_STREAM_SCHEMA_V2: &str =
    include_str!("../assets/json-schema/read-stream-v2.schema.json");

/// JSON Schema for the explicit Native `/v1` compatibility policy.
pub const V1_COMPATIBILITY_SCHEMA_V2: &str =
    include_str!("../assets/json-schema/v1-compatibility-v2.schema.json");

/// JSON Schema for the version 1 capability response.
pub const CAPABILITIES_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/capabilities-v1.schema.json");

/// JSON Schema for the version 1 error envelope.
pub const ERROR_SCHEMA_V1: &str = include_str!("../assets/json-schema/error-v1.schema.json");

/// JSON Schema for version 1 liveness and readiness responses.
pub const HEALTH_SCHEMA_V1: &str = include_str!("../assets/json-schema/health-v1.schema.json");

/// JSON Schema for version 1 atomic put requests.
pub const PUT_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/put-request-v1.schema.json");

/// JSON Schema for version 1 atomic delete requests.
pub const DELETE_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/delete-request-v1.schema.json");

/// JSON Schema for version 1 exact-get requests.
pub const GET_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/get-request-v1.schema.json");

/// JSON Schema for version 1 proof-bearing get responses.
pub const GET_RESPONSE_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/get-response-v1.schema.json");

/// JSON Schema for version 1 durable commit receipts.
pub const COMMIT_RECEIPT_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/commit-receipt-v1.schema.json");

/// JSON Schema for version 1 structured query requests.
pub const QUERY_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/query-request-v1.schema.json");

/// JSON Schema for version 1 proof-bearing query responses.
pub const QUERY_RESPONSE_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/query-response-v1.schema.json");

/// JSON Schema for version 1 result-proof transport.
pub const PROOF_SCHEMA_V1: &str = include_str!("../assets/json-schema/proof-v1.schema.json");

/// JSON Schema for immutable vector-space definition requests.
pub const DEFINE_VECTOR_SPACE_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/define-vector-space-request-v1.schema.json");

/// JSON Schema for atomic durable vector upsert requests.
pub const PUT_VECTORS_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/put-vectors-request-v1.schema.json");

/// JSON Schema for atomic durable vector deletion requests.
pub const DELETE_VECTORS_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/delete-vectors-request-v1.schema.json");

/// JSON Schema for durable exact-retrieval requests.
pub const EXACT_RETRIEVAL_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/exact-retrieval-request-v1.schema.json");

/// JSON Schema for proof-bearing exact-retrieval responses.
pub const EXACT_RETRIEVAL_RESPONSE_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/exact-retrieval-response-v1.schema.json");

/// JSON Schema for canonical retrieval-proof transport.
pub const RETRIEVAL_PROOF_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/retrieval-proof-v1.schema.json");

/// JSON Schema for immutable lexical-index definition requests.
pub const DEFINE_LEXICAL_INDEX_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/define-lexical-index-request-v1.schema.json");

/// JSON Schema for durable lexical-retrieval requests.
pub const LEXICAL_RETRIEVAL_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/lexical-retrieval-request-v1.schema.json");

/// JSON Schema for proof-bearing lexical-retrieval responses.
pub const LEXICAL_RETRIEVAL_RESPONSE_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/lexical-retrieval-response-v1.schema.json");

/// JSON Schema for deterministic hybrid-retrieval requests.
pub const HYBRID_RETRIEVAL_REQUEST_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/hybrid-retrieval-request-v1.schema.json");

/// JSON Schema for proof-bearing hybrid-retrieval responses.
pub const HYBRID_RETRIEVAL_RESPONSE_SCHEMA_V1: &str =
    include_str!("../assets/json-schema/hybrid-retrieval-response-v1.schema.json");

#[cfg(test)]
mod tests {
    use std::{error::Error, fs, io, path::Path};

    use serde_json::Value as JsonValue;
    use serde_yaml_ng::Value as YamlValue;

    use schemars::{JsonSchema, SchemaGenerator};

    use super::{
        CAPABILITIES_SCHEMA_V1, COMMIT_RECEIPT_SCHEMA_V1, DEFINE_LEXICAL_INDEX_REQUEST_SCHEMA_V1,
        DEFINE_VECTOR_SPACE_REQUEST_SCHEMA_V1, DELETE_REQUEST_SCHEMA_V1,
        DELETE_VECTORS_REQUEST_SCHEMA_V1, ERROR_SCHEMA_V1, EXACT_RETRIEVAL_REQUEST_SCHEMA_V1,
        EXACT_RETRIEVAL_RESPONSE_SCHEMA_V1, GET_REQUEST_SCHEMA_V1, GET_RESPONSE_SCHEMA_V1,
        HEALTH_SCHEMA_V1, HYBRID_RETRIEVAL_REQUEST_SCHEMA_V1, HYBRID_RETRIEVAL_RESPONSE_SCHEMA_V1,
        LEXICAL_RETRIEVAL_REQUEST_SCHEMA_V1, LEXICAL_RETRIEVAL_RESPONSE_SCHEMA_V1, OPENAPI_V1,
        PROOF_SCHEMA_V1, PUT_REQUEST_SCHEMA_V1, PUT_VECTORS_REQUEST_SCHEMA_V1,
        QUERY_REQUEST_SCHEMA_V1, QUERY_RESPONSE_SCHEMA_V1, RETRIEVAL_PROOF_SCHEMA_V1,
        v1::{
            CapabilitiesV1, CommitReceiptV1, DefineLexicalIndexRequestV1,
            DefineVectorSpaceRequestV1, DeleteRequestV1, DeleteVectorsRequestV1, ErrorV1,
            ExactRetrievalRequestV1, ExactRetrievalResponseV1, GetRequestV1, GetResponseV1,
            HealthV1, HybridRetrievalRequestV1, HybridRetrievalResponseV1,
            LexicalRetrievalRequestV1, LexicalRetrievalResponseV1, ProofV1, PutRequestV1,
            PutVectorsRequestV1, QueryRequestV1, QueryResponseV1, RetrievalProofV1,
        },
    };

    #[test]
    fn openapi_document_is_version_3_1() -> Result<(), Box<dyn Error>> {
        let document: YamlValue = serde_yaml_ng::from_str(OPENAPI_V1)?;
        assert_eq!(
            document.get("openapi").and_then(YamlValue::as_str),
            Some("3.1.0")
        );
        Ok(())
    }

    #[test]
    fn openapi_defines_version_one_surface_and_resolves_external_schemas()
    -> Result<(), Box<dyn Error>> {
        let document: YamlValue = serde_yaml_ng::from_str(OPENAPI_V1)?;
        let paths = document
            .get("paths")
            .and_then(YamlValue::as_mapping)
            .ok_or_else(|| io::Error::other("OpenAPI paths object is missing"))?;
        for (path, method) in [
            ("/v1/capabilities", "get"),
            ("/v1/health/live", "get"),
            ("/v1/health/ready", "get"),
            ("/v1/kv/put", "post"),
            ("/v1/kv/get", "post"),
            ("/v1/kv/delete", "post"),
            ("/v1/query", "post"),
            ("/v1/vector-spaces/define", "post"),
            ("/v1/vectors/put", "post"),
            ("/v1/vectors/delete", "post"),
            ("/v1/retrieve/exact", "post"),
            ("/v1/lexical-indexes/define", "post"),
            ("/v1/retrieve/lexical", "post"),
            ("/v1/retrieve/hybrid", "post"),
            (
                "/v1/witnesses/{checkpoint_sequence}/{snapshot_digest}",
                "get",
            ),
        ] {
            let operation = paths
                .get(YamlValue::String(path.to_owned()))
                .and_then(|item| item.get(method));
            assert!(operation.is_some(), "missing {method} {path}");
        }

        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/openapi");
        validate_external_schema_refs(&document, &base)?;
        Ok(())
    }

    #[test]
    fn packaged_contract_assets_match_workspace_canonical_files() -> Result<(), Box<dyn Error>> {
        let canonical_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts");
        if !canonical_root.is_dir() {
            return Ok(());
        }

        for (relative, packaged) in [
            ("openapi/hyphae-v1.yaml", OPENAPI_V1),
            (
                "json-schema/capabilities-v1.schema.json",
                CAPABILITIES_SCHEMA_V1,
            ),
            ("json-schema/error-v1.schema.json", ERROR_SCHEMA_V1),
            ("json-schema/health-v1.schema.json", HEALTH_SCHEMA_V1),
            (
                "json-schema/put-request-v1.schema.json",
                PUT_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/delete-request-v1.schema.json",
                DELETE_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/get-request-v1.schema.json",
                GET_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/get-response-v1.schema.json",
                GET_RESPONSE_SCHEMA_V1,
            ),
            (
                "json-schema/commit-receipt-v1.schema.json",
                COMMIT_RECEIPT_SCHEMA_V1,
            ),
            (
                "json-schema/query-request-v1.schema.json",
                QUERY_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/query-response-v1.schema.json",
                QUERY_RESPONSE_SCHEMA_V1,
            ),
            ("json-schema/proof-v1.schema.json", PROOF_SCHEMA_V1),
            (
                "json-schema/define-vector-space-request-v1.schema.json",
                DEFINE_VECTOR_SPACE_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/put-vectors-request-v1.schema.json",
                PUT_VECTORS_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/delete-vectors-request-v1.schema.json",
                DELETE_VECTORS_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/exact-retrieval-request-v1.schema.json",
                EXACT_RETRIEVAL_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/exact-retrieval-response-v1.schema.json",
                EXACT_RETRIEVAL_RESPONSE_SCHEMA_V1,
            ),
            (
                "json-schema/retrieval-proof-v1.schema.json",
                RETRIEVAL_PROOF_SCHEMA_V1,
            ),
            (
                "json-schema/define-lexical-index-request-v1.schema.json",
                DEFINE_LEXICAL_INDEX_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/lexical-retrieval-request-v1.schema.json",
                LEXICAL_RETRIEVAL_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/lexical-retrieval-response-v1.schema.json",
                LEXICAL_RETRIEVAL_RESPONSE_SCHEMA_V1,
            ),
            (
                "json-schema/hybrid-retrieval-request-v1.schema.json",
                HYBRID_RETRIEVAL_REQUEST_SCHEMA_V1,
            ),
            (
                "json-schema/hybrid-retrieval-response-v1.schema.json",
                HYBRID_RETRIEVAL_RESPONSE_SCHEMA_V1,
            ),
        ] {
            assert_eq!(
                fs::read_to_string(canonical_root.join(relative))?,
                packaged,
                "packaged contract asset differs from contracts/{relative}"
            );
        }
        Ok(())
    }

    #[test]
    fn json_schemas_use_draft_2020_12() -> Result<(), Box<dyn Error>> {
        for schema in [
            CAPABILITIES_SCHEMA_V1,
            ERROR_SCHEMA_V1,
            HEALTH_SCHEMA_V1,
            PUT_REQUEST_SCHEMA_V1,
            DELETE_REQUEST_SCHEMA_V1,
            GET_REQUEST_SCHEMA_V1,
            GET_RESPONSE_SCHEMA_V1,
            COMMIT_RECEIPT_SCHEMA_V1,
            QUERY_REQUEST_SCHEMA_V1,
            QUERY_RESPONSE_SCHEMA_V1,
            PROOF_SCHEMA_V1,
            DEFINE_VECTOR_SPACE_REQUEST_SCHEMA_V1,
            PUT_VECTORS_REQUEST_SCHEMA_V1,
            DELETE_VECTORS_REQUEST_SCHEMA_V1,
            EXACT_RETRIEVAL_REQUEST_SCHEMA_V1,
            EXACT_RETRIEVAL_RESPONSE_SCHEMA_V1,
            RETRIEVAL_PROOF_SCHEMA_V1,
            DEFINE_LEXICAL_INDEX_REQUEST_SCHEMA_V1,
            LEXICAL_RETRIEVAL_REQUEST_SCHEMA_V1,
            LEXICAL_RETRIEVAL_RESPONSE_SCHEMA_V1,
            HYBRID_RETRIEVAL_REQUEST_SCHEMA_V1,
            HYBRID_RETRIEVAL_RESPONSE_SCHEMA_V1,
        ] {
            let document: JsonValue = serde_json::from_str(schema)?;
            assert_eq!(
                document.get("$schema").and_then(JsonValue::as_str),
                Some("https://json-schema.org/draft/2020-12/schema")
            );
        }
        Ok(())
    }

    #[test]
    fn checked_in_json_schemas_match_rust_wire_models() -> Result<(), Box<dyn Error>> {
        assert_schema::<CapabilitiesV1>(CAPABILITIES_SCHEMA_V1)?;
        assert_schema::<ErrorV1>(ERROR_SCHEMA_V1)?;
        assert_schema::<HealthV1>(HEALTH_SCHEMA_V1)?;
        assert_schema::<PutRequestV1>(PUT_REQUEST_SCHEMA_V1)?;
        assert_schema::<DeleteRequestV1>(DELETE_REQUEST_SCHEMA_V1)?;
        assert_schema::<GetRequestV1>(GET_REQUEST_SCHEMA_V1)?;
        assert_schema::<GetResponseV1>(GET_RESPONSE_SCHEMA_V1)?;
        assert_schema::<CommitReceiptV1>(COMMIT_RECEIPT_SCHEMA_V1)?;
        assert_schema::<QueryRequestV1>(QUERY_REQUEST_SCHEMA_V1)?;
        assert_schema::<QueryResponseV1>(QUERY_RESPONSE_SCHEMA_V1)?;
        assert_schema::<ProofV1>(PROOF_SCHEMA_V1)?;
        assert_schema::<DefineVectorSpaceRequestV1>(DEFINE_VECTOR_SPACE_REQUEST_SCHEMA_V1)?;
        assert_schema::<PutVectorsRequestV1>(PUT_VECTORS_REQUEST_SCHEMA_V1)?;
        assert_schema::<DeleteVectorsRequestV1>(DELETE_VECTORS_REQUEST_SCHEMA_V1)?;
        assert_schema::<ExactRetrievalRequestV1>(EXACT_RETRIEVAL_REQUEST_SCHEMA_V1)?;
        assert_schema::<ExactRetrievalResponseV1>(EXACT_RETRIEVAL_RESPONSE_SCHEMA_V1)?;
        assert_schema::<RetrievalProofV1>(RETRIEVAL_PROOF_SCHEMA_V1)?;
        assert_schema::<DefineLexicalIndexRequestV1>(DEFINE_LEXICAL_INDEX_REQUEST_SCHEMA_V1)?;
        assert_schema::<LexicalRetrievalRequestV1>(LEXICAL_RETRIEVAL_REQUEST_SCHEMA_V1)?;
        assert_schema::<LexicalRetrievalResponseV1>(LEXICAL_RETRIEVAL_RESPONSE_SCHEMA_V1)?;
        assert_schema::<HybridRetrievalRequestV1>(HYBRID_RETRIEVAL_REQUEST_SCHEMA_V1)?;
        assert_schema::<HybridRetrievalResponseV1>(HYBRID_RETRIEVAL_RESPONSE_SCHEMA_V1)?;
        Ok(())
    }

    fn assert_schema<T: JsonSchema>(checked_in: &str) -> Result<(), Box<dyn Error>> {
        let generated = SchemaGenerator::default().into_root_schema_for::<T>();
        let checked_in: JsonValue = serde_json::from_str(checked_in)?;
        assert_eq!(serde_json::to_value(generated)?, checked_in);
        Ok(())
    }

    fn validate_external_schema_refs(value: &YamlValue, base: &Path) -> Result<(), Box<dyn Error>> {
        match value {
            YamlValue::Mapping(mapping) => {
                if let Some(reference) = mapping
                    .get(YamlValue::String("$ref".to_owned()))
                    .and_then(YamlValue::as_str)
                    .filter(|reference| reference.starts_with("../json-schema/"))
                {
                    let encoded = fs::read_to_string(base.join(reference))?;
                    let _schema: JsonValue = serde_json::from_str(&encoded)?;
                }
                for (key, value) in mapping {
                    validate_external_schema_refs(key, base)?;
                    validate_external_schema_refs(value, base)?;
                }
            }
            YamlValue::Sequence(values) => {
                for value in values {
                    validate_external_schema_refs(value, base)?;
                }
            }
            YamlValue::Tagged(value) => validate_external_schema_refs(&value.value, base)?,
            YamlValue::Null | YamlValue::Bool(_) | YamlValue::Number(_) | YamlValue::String(_) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod v2_tests {
    use std::{error::Error, fs, path::Path};

    use serde_json::Value as JsonValue;
    use serde_yaml_ng::Value as YamlValue;

    use super::{
        NATIVE_SCHEMA_V2, OPENAPI_V2, PRODUCT_ERROR_SCHEMA_V2, READ_STREAM_SCHEMA_V2,
        V1_COMPATIBILITY_SCHEMA_V2,
    };

    #[test]
    fn native_v2_assets_are_versioned_and_match_canonical_contracts() -> Result<(), Box<dyn Error>>
    {
        let document: YamlValue = serde_yaml_ng::from_str(OPENAPI_V2)?;
        assert_eq!(
            document.get("openapi").and_then(YamlValue::as_str),
            Some("3.1.0")
        );
        let paths = document
            .get("paths")
            .and_then(YamlValue::as_mapping)
            .ok_or("v2 paths are missing")?;
        for path in [
            "/v2/capabilities",
            "/v2/execute",
            "/v2/catalog",
            "/v2/sql",
            "/v2/structures",
            "/v2/search",
            "/v2/search/collection",
            "/v2/search/ingest",
            "/v2/admin",
            "/v2/telemetry",
            "/v2/doctor",
            "/v2/backup",
            "/v2/restore",
            "/v2/proofs/verify",
            "/v2/transactions/status",
            "/v2/read-stream",
        ] {
            assert!(
                paths.contains_key(YamlValue::String(path.to_owned())),
                "{path}"
            );
        }

        for schema in [
            NATIVE_SCHEMA_V2,
            PRODUCT_ERROR_SCHEMA_V2,
            READ_STREAM_SCHEMA_V2,
            V1_COMPATIBILITY_SCHEMA_V2,
        ] {
            let value: JsonValue = serde_json::from_str(schema)?;
            assert_eq!(
                value.get("$schema").and_then(JsonValue::as_str),
                Some("https://json-schema.org/draft/2020-12/schema")
            );
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts");
        if root.is_dir() {
            for (path, packaged) in [
                ("json-schema/native-v2.schema.json", NATIVE_SCHEMA_V2),
                (
                    "json-schema/product-error-v2.schema.json",
                    PRODUCT_ERROR_SCHEMA_V2,
                ),
                (
                    "json-schema/read-stream-v2.schema.json",
                    READ_STREAM_SCHEMA_V2,
                ),
                (
                    "json-schema/v1-compatibility-v2.schema.json",
                    V1_COMPATIBILITY_SCHEMA_V2,
                ),
            ] {
                let canonical = fs::read_to_string(root.join(path))?;
                let canonical: JsonValue = serde_json::from_str(&canonical)?;
                let packaged: JsonValue = serde_json::from_str(packaged)?;
                assert_eq!(canonical.get("$schema"), packaged.get("$schema"), "{path}");
                assert_eq!(canonical.get("title"), packaged.get("title"), "{path}");
            }

            let canonical: YamlValue =
                serde_yaml_ng::from_str(&fs::read_to_string(root.join("openapi/hyphae-v2.yaml"))?)?;
            assert_eq!(
                canonical.get("openapi").and_then(YamlValue::as_str),
                document.get("openapi").and_then(YamlValue::as_str)
            );
            assert_eq!(
                canonical
                    .get("paths")
                    .and_then(YamlValue::as_mapping)
                    .map(serde_yaml_ng::Mapping::len),
                document
                    .get("paths")
                    .and_then(YamlValue::as_mapping)
                    .map(serde_yaml_ng::Mapping::len)
            );
        }
        Ok(())
    }
}
