// SPDX-License-Identifier: Apache-2.0
//! Cross-SDK golden vectors, minor negotiation and malformed memory responses.
use hyphae_native_product::{
    CatalogVersion, ObjectId, ProductDocument, ProductDurabilityPolicy, ProductIntegratedSearchHit,
    ProductLexicalBranch, ProductLimits, ProductMemoryEnrichRequest, ProductMemoryHit,
    ProductMemoryRecallRequest, ProductMemoryRecallResult, ProductMemorySearchResult,
    ProductOperation, ProductResponse, ProductSearchDocumentUpdate, ProductSearchFilter,
    ProductSearchRequest, ProductSearchResult, ProductVector, SnapshotIdentity,
};
use hyphae_native_protocol::{
    ProductCodecError, WireRequest, decode_product_request_for_minor,
    decode_product_response_for_minor, encode_product_request_for_minor,
    encode_product_response_for_minor,
};
use std::{collections::BTreeMap, error::Error, path::PathBuf};

fn golden(name: &str, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    let compatibility = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compatibility");
    if !compatibility.is_dir() {
        return Ok(());
    }
    let path = compatibility.join(name);
    if std::env::var_os("HYPHAE_REGENERATE_MEMORY_GOLDENS").is_some() {
        std::fs::write(&path, bytes)?;
    }
    assert_eq!(std::fs::read(path)?, bytes);
    Ok(())
}

fn wire(operation: ProductOperation) -> WireRequest {
    WireRequest {
        operation,
        logical_time_micros: 5,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    }
}

#[test]
fn memory_requests_pin_cross_sdk_bytes_and_require_minor_seven() -> Result<(), Box<dyn Error>> {
    let recall = ProductOperation::MemoryRecall(ProductMemoryRecallRequest {
        collections: vec![ObjectId::new(21)?, ObjectId::new(22)?],
        limit: 2,
        provenance: b"query-manifest".to_vec(),
        search: ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "decisions".into(),
                candidate_limit: 4,
                weight: 1,
                operator: None,
                prefix: false,
                fields: Vec::new(),
                fuzzy: None,
                phrase: false,
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
            range_facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 4,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
    });
    let enrich = ProductOperation::MemoryEnrich(ProductMemoryEnrichRequest {
        collection: ObjectId::new(21)?,
        expected_envelope_digest: [9; 32],
        update: ProductSearchDocumentUpdate {
            idempotency_id: 701,
            document: ProductDocument {
                object_id: ObjectId::new(201)?,
                text: "remember".into(),
                doc_values: BTreeMap::new(),
                vectors: BTreeMap::from([("memory".into(), ProductVector::new([1.0, 0.0])?)]),
            },
        },
    });
    for (name, operation) in [
        ("native-memory-recall-v1.bin", recall),
        ("native-memory-enrich-v1.bin", enrich),
    ] {
        let request = wire(operation.clone());
        assert!(matches!(
            encode_product_request_for_minor(&request, 6),
            Err(ProductCodecError::Unsupported)
        ));
        let bytes = encode_product_request_for_minor(&request, 7)?;
        golden(name, &bytes)?;
        assert!(matches!(
            decode_product_request_for_minor(&bytes, 6),
            Err(ProductCodecError::Unsupported)
        ));
        let decoded = decode_product_request_for_minor(&bytes, 7)?;
        assert_eq!(encode_product_request_for_minor(&decoded, 7)?, bytes);
        for end in 0..bytes.len() {
            assert!(decode_product_request_for_minor(&bytes[..end], 7).is_err());
        }
    }
    Ok(())
}

fn result() -> Result<ProductMemoryRecallResult, Box<dyn Error>> {
    let snapshot = SnapshotIdentity {
        directory_lineage: [1; 24],
        visible_csn: None,
        catalog_version: CatalogVersion::new(3)?,
        root_digest: [4; 32],
        logical_time_micros: 5,
    };
    let hit = ProductIntegratedSearchHit {
        object_id: ObjectId::new(201)?,
        score: 1.5,
        doc_values: BTreeMap::new(),
        fragments: Vec::new(),
    };
    let mut result = ProductMemoryRecallResult {
        snapshot,
        memories: Vec::new(),
        searches: Vec::new(),
        expired_filtered: 1,
    };
    for id in [21, 22] {
        let collection = ObjectId::new(id)?;
        let mut hits = vec![hit.clone()];
        if id == 21 {
            hits.insert(
                0,
                ProductIntegratedSearchHit {
                    object_id: ObjectId::new(202)?,
                    score: 2.0,
                    ..hit.clone()
                },
            );
        }
        let count = hits.len();
        result.searches.push(ProductMemorySearchResult {
            collection,
            result: ProductSearchResult {
                snapshot,
                hits,
                facets: Vec::new(),
                range_facets: Vec::new(),
                aggregations: Vec::new(),
                vector_branches: Vec::new(),
                approximate: false,
                total_documents: count,
                eligible_documents: count,
                lexical_candidates: count,
                retrieval_candidates: count,
                matched_candidates: count,
            },
        });
        result.memories.push(ProductMemoryHit {
            collection,
            hit: hit.clone(),
            envelope: br#"{"text":"remember"}"#.to_vec(),
        });
    }
    Ok(result)
}

#[test]
fn memory_response_rejects_truncation_mixed_snapshots_and_forged_membership()
-> Result<(), Box<dyn Error>> {
    let value = result()?;
    let response = ProductResponse::MemoryRecall(value.clone());
    let bytes = encode_product_response_for_minor(&response, 7)?;
    golden("native-memory-result-v1.bin", &bytes)?;
    assert!(matches!(
        decode_product_response_for_minor(&bytes, 6),
        Err(ProductCodecError::Unsupported)
    ));
    assert_eq!(decode_product_response_for_minor(&bytes, 7)?, response);
    for end in 0..bytes.len() {
        assert!(decode_product_response_for_minor(&bytes[..end], 7).is_err());
    }
    let mut bad = value.clone();
    bad.searches[0].result.snapshot.logical_time_micros += 1;
    assert!(encode_product_response_for_minor(&ProductResponse::MemoryRecall(bad), 7).is_err());
    let mut bad = value.clone();
    bad.memories[0].hit.object_id = ObjectId::new(999)?;
    assert!(bad.validate().is_err());
    let mut bad = value;
    bad.memories.swap(0, 1);
    assert!(bad.validate().is_err());
    Ok(())
}
