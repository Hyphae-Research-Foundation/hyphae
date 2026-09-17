// SPDX-License-Identifier: Apache-2.0

//! Adversarial count and dimension regression coverage for product decoders.

use std::collections::BTreeMap;

use hyphae_native_product::{
    CanonicalF64, CatalogVersion, ObjectId, ProductDocument, ProductDurabilityPolicy,
    ProductHashEntry, ProductLimits, ProductOperation, ProductRead, ProductResponse,
    ProductSearchFilter, ProductSearchIngestBatch, ProductSearchRequest, ProductSearchResult,
    ProductSetAlgebraOperation, ProductSortedSetEntry, ProductStructureKey,
    ProductStructureMutation, ProductStructureReadRequest, ProductStructureReadResult,
    ProductVector, ProductVectorBranch, SnapshotIdentity,
};
use hyphae_native_protocol::{
    ProductCodecError, WireRequest, decode_product_request, decode_product_response,
    encode_product_request, encode_product_response,
};

const REQUEST_BODY_OFFSET: usize = 16 + 64;
const SNAPSHOT_BYTES: usize = 24 + 8 + 8 + 32 + 8;

#[test]
fn set_algebra_counts_are_bounded_before_request_allocation()
-> Result<(), Box<dyn std::error::Error>> {
    let request = wire_request(ProductOperation::StructureRead(
        ProductStructureReadRequest::SetAlgebra {
            keyspace: ObjectId::new(1)?,
            operation: ProductSetAlgebraOperation::Union,
            keys: vec![b"key".to_vec()],
            output_member_limit: 1,
            visit_limit: 1,
        },
    ));
    let encoded = encode_product_request(&request)?;
    let count_offset = REQUEST_BODY_OFFSET + 1 + 16 + 1;
    assert_eq!(read_u32(&encoded, count_offset), 1);

    assert_request_limit_error(&encoded, count_offset, u32::MAX);
    assert_truncated_request_count(&encoded, count_offset, 1)?;
    Ok(())
}

#[test]
fn structure_batch_and_stream_field_counts_are_bounded_before_allocation()
-> Result<(), Box<dyn std::error::Error>> {
    let key = b"events";
    let request = wire_request(ProductOperation::StructureMutate {
        mutations: vec![ProductStructureMutation::StreamAdd {
            key: ProductStructureKey {
                keyspace: ObjectId::new(1)?,
                key: key.to_vec(),
            },
            fields: vec![ProductHashEntry {
                field: b"kind".to_vec(),
                value: b"created".to_vec(),
            }],
        }],
    });
    let encoded = encode_product_request(&request)?;
    let mutation_count_offset = REQUEST_BODY_OFFSET;
    assert_eq!(read_u32(&encoded, mutation_count_offset), 1);
    assert_request_limit_error(&encoded, mutation_count_offset, u32::MAX);

    let field_count_offset = REQUEST_BODY_OFFSET + 4 + 1 + 16 + 4 + key.len();
    assert_eq!(read_u32(&encoded, field_count_offset), 1);
    assert_request_limit_error(&encoded, field_count_offset, u32::MAX);
    assert_truncated_request_count(&encoded, field_count_offset, 1)?;
    Ok(())
}

#[test]
fn integrated_search_dimensions_are_bounded_before_request_allocation()
-> Result<(), Box<dyn std::error::Error>> {
    let target = "abcdefghijklmnopqrst";
    let request = wire_request(ProductOperation::SearchCollection {
        collection: ObjectId::new(1)?,
        request: ProductSearchRequest {
            lexical: None,
            vectors: vec![ProductVectorBranch {
                target: target.to_owned(),
                query: ProductVector::new([1.0])?,
                candidate_limit: 1,
                weight: 1,
                execution: None,
                max_distance: None,
            }],
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
            range_facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 1,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
            autocut: None,
            offset: 0,
        },
    });
    let encoded = encode_product_request(&request)?;
    let dimension_offset = REQUEST_BODY_OFFSET + 16 + 8 + 4 + 4 + target.len();
    assert_eq!(read_u32(&encoded, dimension_offset), 1);

    assert_request_limit_error(&encoded, dimension_offset, u32::MAX);
    assert_truncated_request_count(&encoded, dimension_offset, 1)?;
    Ok(())
}

#[test]
fn ingested_document_dimensions_are_bounded_before_request_allocation()
-> Result<(), Box<dyn std::error::Error>> {
    let vector_name = "vector";
    let mut vectors = BTreeMap::new();
    vectors.insert(vector_name.to_owned(), ProductVector::new([1.0])?);
    let request = wire_request(ProductOperation::SearchIngest {
        collection: ObjectId::new(1)?,
        batch: ProductSearchIngestBatch {
            idempotency_id: 1,
            documents: vec![ProductDocument {
                object_id: ObjectId::new(2)?,
                text: "x".to_owned(),
                doc_values: BTreeMap::new(),
                vectors,
            }],
        },
    });
    let encoded = encode_product_request(&request)?;
    let dimension_offset =
        REQUEST_BODY_OFFSET + 16 + 16 + 4 + 16 + 4 + 1 + 4 + 4 + 4 + vector_name.len();
    assert_eq!(read_u32(&encoded, dimension_offset), 1);

    assert_request_limit_error(&encoded, dimension_offset, u32::MAX);
    assert_truncated_request_count(&encoded, dimension_offset, 1)?;
    Ok(())
}

#[test]
fn response_counts_are_bounded_and_feasible_before_allocation()
-> Result<(), Box<dyn std::error::Error>> {
    let response = ProductResponse::IntegratedSearch(ProductSearchResult {
        snapshot: snapshot()?,
        hits: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        vector_branches: Vec::new(),
        approximate: false,
        total_documents: 0,
        eligible_documents: 0,
        lexical_candidates: 0,
        retrieval_candidates: 0,
        matched_candidates: 0,
    });
    let encoded = encode_product_response(&response)?;
    let count_offset = 16 + SNAPSHOT_BYTES;
    assert_eq!(read_u32(&encoded, count_offset), 0);

    let mut excessive = encoded.clone();
    write_u32(&mut excessive, count_offset, u32::MAX);
    assert!(matches!(
        decode_product_response(&excessive),
        Err(ProductCodecError::LimitExceeded)
    ));

    let mut impossible = encoded;
    write_u32(&mut impossible, count_offset, 1);
    truncate_wire(&mut impossible, count_offset + 4)?;
    assert!(matches!(
        decode_product_response(&impossible),
        Err(ProductCodecError::Truncated)
    ));
    Ok(())
}

#[test]
fn historically_valid_large_structure_responses_still_round_trip()
-> Result<(), Box<dyn std::error::Error>> {
    let values = ProductResponse::StructureRead(ProductRead {
        snapshot: snapshot()?,
        value: ProductStructureReadResult::Values(vec![Vec::new(); 65_537]),
    });
    assert_eq!(
        decode_product_response(&encode_product_response(&values)?)?,
        values
    );

    let hashes = ProductResponse::StructureRead(ProductRead {
        snapshot: snapshot()?,
        value: ProductStructureReadResult::HashEntries(vec![
            ProductHashEntry {
                field: Vec::new(),
                value: Vec::new(),
            };
            4_097
        ]),
    });
    assert_eq!(
        decode_product_response(&encode_product_response(&hashes)?)?,
        hashes
    );

    let sorted = ProductResponse::StructureRead(ProductRead {
        snapshot: snapshot()?,
        value: ProductStructureReadResult::SortedSetEntries(vec![
            ProductSortedSetEntry {
                member: Vec::new(),
                score: CanonicalF64::new(1.0),
            };
            4_097
        ]),
    });
    assert_eq!(
        decode_product_response(&encode_product_response(&sorted)?)?,
        sorted
    );
    Ok(())
}

fn wire_request(operation: ProductOperation) -> WireRequest {
    WireRequest {
        operation,
        logical_time_micros: 1,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::MEMORY,
    }
}

fn snapshot() -> Result<SnapshotIdentity, Box<dyn std::error::Error>> {
    Ok(SnapshotIdentity {
        directory_lineage: [1; 24],
        visible_csn: None,
        catalog_version: CatalogVersion::new(1)?,
        root_digest: [2; 32],
        logical_time_micros: 1,
    })
}

fn assert_request_limit_error(encoded: &[u8], offset: usize, count: u32) {
    let mut invalid = encoded.to_vec();
    write_u32(&mut invalid, offset, count);
    assert!(matches!(
        decode_product_request(&invalid),
        Err(ProductCodecError::LimitExceeded)
    ));
}

fn assert_truncated_request_count(
    encoded: &[u8],
    offset: usize,
    count: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut impossible = encoded.to_vec();
    write_u32(&mut impossible, offset, count);
    truncate_wire(&mut impossible, offset + 4)?;
    assert!(matches!(
        decode_product_request(&impossible),
        Err(ProductCodecError::Truncated)
    ));
    Ok(())
}

fn truncate_wire(encoded: &mut Vec<u8>, length: usize) -> Result<(), Box<dyn std::error::Error>> {
    encoded.truncate(length);
    let length = u32::try_from(encoded.len())?;
    encoded[8..12].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

fn read_u32(encoded: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(encoded[offset..offset + 4].try_into().unwrap_or([0; 4]))
}

fn write_u32(encoded: &mut [u8], offset: usize, value: u32) {
    encoded[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
