// SPDX-License-Identifier: Apache-2.0

//! Protocol-minor and bounded-codec coverage for catalogued embedding.

use std::collections::BTreeMap;

use hyphae_native_catalog::{
    CatalogName, CatalogObjectKind, CatalogObjectV2, DefinitionVersion,
    EmbeddingArtifactManifestDigest, EmbeddingPipelineVersion, EmbeddingProfileDefinition,
    LogicalCatalogObject, ObjectHeaderV2, QWEN3_EMBEDDING_QUERY_INSTRUCTION, QualifiedName,
};
use hyphae_native_product::{
    CatalogListRequest, CatalogVersion, ObjectId, ProductCommitReceipt, ProductDocValue,
    ProductDurability, ProductDurabilityPolicy, ProductEmbedAndIngestBatch,
    ProductEmbedAndIngestDocument, ProductEmbedAndIngestReceipt, ProductEmbeddingBackend,
    ProductEmbeddingExecutionProfile, ProductEmbeddingPrecision, ProductLimits, ProductOperation,
    ProductResponse, ProductTransactionId, SnapshotIdentity,
};
use hyphae_native_protocol::{
    ProductCodecError, WireRequest, decode_product_request_for_minor,
    decode_product_response_for_minor, encode_product_request_for_minor, encode_product_response,
    encode_product_response_for_minor,
};
use hyphae_native_types::{EngineKind, VectorElement, VectorType};

const REQUEST_BODY_OFFSET: usize = 16 + 64;
const SNAPSHOT_BYTES: usize = 80;

#[test]
fn embed_and_ingest_uses_minor_nine_request_tag_73_and_bounded_counts()
-> Result<(), Box<dyn std::error::Error>> {
    let request = embed_request()?;
    assert!(matches!(
        encode_product_request_for_minor(&request, 8),
        Err(ProductCodecError::Unsupported)
    ));

    let encoded = encode_product_request_for_minor(&request, 9)?;
    assert_eq!(u16::from_le_bytes(encoded[12..14].try_into()?), 73);
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 8),
        Err(ProductCodecError::Unsupported)
    ));
    let decoded = decode_product_request_for_minor(&encoded, 9)?;
    let ProductOperation::EmbedAndIngest { collection, batch } = decoded.operation else {
        return Err("embed-and-ingest operation expected".into());
    };
    assert_eq!(collection, ObjectId::new(13)?);
    assert_eq!(batch, embed_batch()?);
    assert_eq!(
        batch.documents[0]
            .doc_values
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["\u{e000}", "\u{1f600}"]
    );

    let count_offset = REQUEST_BODY_OFFSET + 16 + 16;
    let mut excessive = encoded.clone();
    excessive[count_offset..count_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        decode_product_request_for_minor(&excessive, 9),
        Err(ProductCodecError::LimitExceeded)
    ));

    let mut truncated_count = encoded;
    truncated_count[count_offset..count_offset + 4].copy_from_slice(&256_u32.to_le_bytes());
    assert!(matches!(
        decode_product_request_for_minor(&truncated_count, 9),
        Err(ProductCodecError::Truncated)
    ));
    Ok(())
}

#[test]
fn embed_and_ingest_response_tag_47_requires_commit_and_bounds_profile()
-> Result<(), Box<dyn std::error::Error>> {
    let response = ProductResponse::EmbedAndIngested(ProductEmbedAndIngestReceipt {
        snapshot: snapshot()?,
        commit: commit()?,
        documents: 1,
        idempotent_replay: true,
        execution_profile: execution_profile()?,
    });
    assert!(matches!(
        encode_product_response_for_minor(&response, 8),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_response_for_minor(&response, 9)?;
    assert_eq!(u16::from_le_bytes(encoded[12..14].try_into()?), 47);
    assert_eq!(decode_product_response_for_minor(&encoded, 9)?, response);
    assert!(matches!(
        decode_product_response_for_minor(&encoded, 8),
        Err(ProductCodecError::Unsupported)
    ));

    let mut missing_commit = encoded.clone();
    missing_commit[16 + SNAPSHOT_BYTES] = 0;
    assert!(matches!(
        decode_product_response_for_minor(&missing_commit, 9),
        Err(ProductCodecError::InvalidValue)
    ));

    let kernel_count_offset = 16 + SNAPSHOT_BYTES + 16 + 24 + 3 * (4 + 1);
    let mut excessive_kernels = encoded;
    excessive_kernels[kernel_count_offset..kernel_count_offset + 4]
        .copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        decode_product_response_for_minor(&excessive_kernels, 9),
        Err(ProductCodecError::LimitExceeded)
    ));
    Ok(())
}

#[test]
fn embedding_profile_catalog_content_requires_minor_eight() -> Result<(), Box<dyn std::error::Error>>
{
    let profile = embedding_profile()?;
    let create = WireRequest {
        operation: ProductOperation::CatalogCreate {
            object: profile.clone(),
        },
        logical_time_micros: 1,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    };
    assert!(matches!(
        encode_product_request_for_minor(&create, 7),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_request_for_minor(&create, 8)?;
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 7),
        Err(ProductCodecError::Unsupported)
    ));
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 8)?.operation,
        ProductOperation::CatalogCreate { .. }
    ));

    let list = WireRequest {
        operation: ProductOperation::CatalogList(CatalogListRequest {
            parent: None,
            kind: Some(CatalogObjectKind::EmbeddingProfile),
            cursor: None,
            item_limit: 1,
            visit_limit: 1,
            byte_limit: 1_024,
        }),
        ..create
    };
    assert!(matches!(
        encode_product_request_for_minor(&list, 7),
        Err(ProductCodecError::Unsupported)
    ));
    encode_product_request_for_minor(&list, 8)?;

    let response = ProductResponse::CatalogDefinition(Some(profile));
    assert!(matches!(
        encode_product_response_for_minor(&response, 7),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_response(&response)?;
    assert!(matches!(
        decode_product_response_for_minor(&encoded, 7),
        Err(ProductCodecError::Unsupported)
    ));
    assert_eq!(decode_product_response_for_minor(&encoded, 8)?, response);
    Ok(())
}

fn embed_request() -> Result<WireRequest, Box<dyn std::error::Error>> {
    Ok(WireRequest {
        operation: ProductOperation::EmbedAndIngest {
            collection: ObjectId::new(13)?,
            batch: embed_batch()?,
        },
        logical_time_micros: 1,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    })
}

fn embed_batch() -> Result<ProductEmbedAndIngestBatch, Box<dyn std::error::Error>> {
    Ok(ProductEmbedAndIngestBatch {
        idempotency_id: 7,
        documents: vec![ProductEmbedAndIngestDocument {
            object_id: ObjectId::new(201)?,
            text: "rust".to_owned(),
            doc_values: BTreeMap::from([
                (
                    "\u{1f600}".to_owned(),
                    ProductDocValue::String("supplementary".to_owned()),
                ),
                (
                    "\u{e000}".to_owned(),
                    ProductDocValue::String("private-use".to_owned()),
                ),
            ]),
        }],
    })
}

fn execution_profile() -> Result<ProductEmbeddingExecutionProfile, Box<dyn std::error::Error>> {
    Ok(ProductEmbeddingExecutionProfile {
        embedding_profile: ObjectId::new(17)?,
        backend: ProductEmbeddingBackend::Cpu,
        device: "c".to_owned(),
        driver: "d".to_owned(),
        runtime: "r".to_owned(),
        precision: ProductEmbeddingPrecision::F32,
        kernels: vec!["k".to_owned()],
        fallback: false,
    })
}

fn snapshot() -> Result<SnapshotIdentity, Box<dyn std::error::Error>> {
    Ok(SnapshotIdentity {
        directory_lineage: [1; 24],
        visible_csn: Some(hyphae_native_product::Csn::new(7)?),
        catalog_version: CatalogVersion::new(8)?,
        root_digest: [2; 32],
        logical_time_micros: 10,
    })
}

fn commit() -> Result<ProductCommitReceipt, Box<dyn std::error::Error>> {
    Ok(ProductCommitReceipt {
        transaction_id: ProductTransactionId::new(9).ok_or("nonzero transaction")?,
        commit_csn: 7,
        catalog_version: 8,
        commit_lsn: 9,
        wal_block_digest: [3; 32],
        durability: ProductDurability::Strict,
        durability_cohort_size: 1,
        durability_cohort_position: 0,
    })
}

fn embedding_profile() -> Result<LogicalCatalogObject, Box<dyn std::error::Error>> {
    Ok(LogicalCatalogObject::V2(CatalogObjectV2::EmbeddingProfile(
        EmbeddingProfileDefinition {
            header: ObjectHeaderV2 {
                id: ObjectId::new(17)?,
                owner: EngineKind::Search,
                name: QualifiedName::new(
                    CatalogName::unquoted("main")?,
                    CatalogName::unquoted("public")?,
                    CatalogName::unquoted("embedding")?,
                ),
                parent: Some(ObjectId::new(2)?),
                definition_version: DefinitionVersion::FIRST,
            },
            artifact_manifest_digest: EmbeddingArtifactManifestDigest::new([4; 32])?,
            artifact_manifest_byte_length: 8_323,
            pipeline_version: EmbeddingPipelineVersion::Qwen3EmbeddingV1,
            vector_type: VectorType::new(VectorElement::Float32, 384)?,
            max_input_tokens: 256,
            query_instruction: QWEN3_EMBEDDING_QUERY_INSTRUCTION.to_owned(),
        },
    )))
}
