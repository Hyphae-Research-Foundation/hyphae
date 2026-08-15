// SPDX-License-Identifier: AGPL-3.0-only

//! Shared protocol golden vectors and strict completion controls.

use hyphae_native_product::{
    BackupRequest, DoctorRequest, ObjectId, ProductDocValue, ProductDocument,
    ProductDurabilityPolicy, ProductError, ProductErrorCode, ProductExplicitTransactionStatus,
    ProductLimits, ProductOperation, ProductResponse, ProductSearchIngestBatch,
    ProductSearchIngestReceipt, ProductTransactionHandle, ProductTransactionSearchMutation,
    ProductTransactionSqlMutation, ProductTransactionVectorMutation, ProductValue, ProductVector,
    SnapshotIdentity,
};
use hyphae_native_protocol::{
    API_KEY_AUTH_TRAILER_BYTES, AsyncFrameIo, FrameKind, GOLDEN_STRUCTURE_FRAME_BLAKE3,
    HandshakeError, Hello, NegotiationPolicy, ProductCodecError, ProtocolCapabilities,
    ProvisionalStream, StreamCompletion, WireRequest, decode_authenticated_hello, decode_end,
    decode_failure, decode_hello, decode_product_request, decode_product_response, decode_welcome,
    encode_authenticated_hello, encode_end, encode_failure, encode_frame, encode_hello,
    encode_product_request, encode_product_response, encode_welcome, golden_structure_frame,
    negotiate,
};
use tokio::io::AsyncWriteExt as _;

#[test]
fn shared_frame_and_handshake_vectors_are_stable() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        blake3::hash(&golden_structure_frame()?).to_hex().as_str(),
        GOLDEN_STRUCTURE_FRAME_BLAKE3
    );
    let hello = Hello::default();
    let encoded = encode_hello(&hello)?;
    assert_eq!(decode_hello(&encoded)?, hello);
    assert_eq!(
        blake3::hash(&encoded).to_hex().as_str(),
        "757d9ab8f303509c52b1d9ba842c6c66533abfe3b4bf21321d35bc84e8da5b18"
    );
    let welcome = negotiate(
        &hello,
        NegotiationPolicy::default(),
        7,
        hyphae_native_product::capabilities(),
        11,
    )?;
    let encoded = encode_welcome(welcome)?;
    assert_eq!(decode_welcome(&encoded)?, welcome);
    assert_eq!(
        blake3::hash(&encoded).to_hex().as_str(),
        "1aad5efb68acd602463954756eb2d56cc8c07bba1e48970e4d74f31b0918c310"
    );
    Ok(())
}

#[test]
fn authenticated_hello_has_a_stable_redacted_wire_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let hello = authenticated_hello();
    let api_key = api_key_fixture();
    let encoded = encode_authenticated_hello(&hello, &api_key)?;

    assert_eq!(encoded[49], 1);
    assert_eq!(
        usize::from(u16::from_le_bytes(encoded[50..52].try_into()?)),
        API_KEY_AUTH_TRAILER_BYTES
    );
    assert_eq!(
        blake3::hash(&encoded).to_hex().as_str(),
        "e11f972e4e670beb1523f1e56a034c3dc85af861cd88a2761f51cb590c9ea56b"
    );
    assert_eq!(decode_hello(&encoded), Err(HandshakeError::Malformed));

    let decoded = decode_authenticated_hello(&encoded)?;
    assert_eq!(decoded.hello(), &hello);
    let diagnostic = format!("{decoded:?}");
    assert!(diagnostic.contains("[REDACTED]"));
    assert!(!diagnostic.contains(&api_key));
    let (decoded_hello, credential) = decoded.into_parts();
    assert_eq!(decoded_hello, hello);
    assert_eq!(format!("{credential:?}"), "ApiKeyCredential([REDACTED])");

    let welcome = negotiate(
        &hello,
        NegotiationPolicy {
            capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
            ..NegotiationPolicy::default()
        },
        7,
        hyphae_native_product::capabilities(),
        11,
    )?;
    assert!(
        welcome
            .capabilities
            .contains(ProtocolCapabilities::API_KEY_AUTH)
    );
    Ok(())
}

#[test]
fn authenticated_hello_rejects_downgrade_and_noncanonical_trailers()
-> Result<(), Box<dyn std::error::Error>> {
    let hello = authenticated_hello();
    let api_key = api_key_fixture();
    let encoded = encode_authenticated_hello(&hello, &api_key)?;

    assert_eq!(
        encode_authenticated_hello(&Hello::default(), &api_key).err(),
        Some(HandshakeError::MissingCapability)
    );
    assert_eq!(
        encode_authenticated_hello(&hello, &api_key[..API_KEY_AUTH_TRAILER_BYTES - 1]).err(),
        Some(HandshakeError::InvalidLimit)
    );
    assert_eq!(
        encode_authenticated_hello(&hello, &format!("{api_key}x")).err(),
        Some(HandshakeError::InvalidLimit)
    );
    assert!(ProtocolCapabilities::from_bits(1 << 8).is_none());

    assert_eq!(
        decode_authenticated_hello(&encode_hello(&Hello::default())?).err(),
        Some(HandshakeError::Malformed)
    );

    let mut unsupported = encoded.clone();
    unsupported[28..36].copy_from_slice(&ProtocolCapabilities::G6.bits().to_le_bytes());
    assert_eq!(
        decode_authenticated_hello(&unsupported).err(),
        Some(HandshakeError::MissingCapability)
    );

    let mut unknown_kind = encoded.clone();
    unknown_kind[49] = 2;
    assert_eq!(
        decode_authenticated_hello(&unknown_kind).err(),
        Some(HandshakeError::Malformed)
    );

    let mut wrong_length = encoded.clone();
    wrong_length[50..52].copy_from_slice(&101_u16.to_le_bytes());
    assert_eq!(
        decode_authenticated_hello(&wrong_length).err(),
        Some(HandshakeError::InvalidLimit)
    );

    let mut invalid_utf8 = encoded.clone();
    let last = invalid_utf8
        .last_mut()
        .ok_or("authenticated HELLO unexpectedly empty")?;
    *last = 0xff;
    assert_eq!(
        decode_authenticated_hello(&invalid_utf8).err(),
        Some(HandshakeError::Malformed)
    );

    let mut trailing = encoded.clone();
    trailing.push(0);
    let trailing_length = u32::try_from(trailing.len())?;
    trailing[8..12].copy_from_slice(&trailing_length.to_le_bytes());
    assert_eq!(
        decode_authenticated_hello(&trailing).err(),
        Some(HandshakeError::Malformed)
    );
    Ok(())
}

#[test]
fn authenticated_hello_reports_every_truncated_prefix() -> Result<(), Box<dyn std::error::Error>> {
    let encoded = encode_authenticated_hello(&authenticated_hello(), &api_key_fixture())?;
    for prefix_length in 0..encoded.len() {
        assert_eq!(
            decode_authenticated_hello(&encoded[..prefix_length]).err(),
            Some(HandshakeError::Truncated),
            "prefix length {prefix_length}"
        );
    }
    Ok(())
}

fn authenticated_hello() -> Hello {
    Hello {
        capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
        required_capabilities: ProtocolCapabilities::G6_AUTHENTICATED,
        ..Hello::default()
    }
}

fn api_key_fixture() -> String {
    format!("hyp1_{}_{}", "1".repeat(32), "2".repeat(64))
}

#[test]
fn integrated_ingest_request_has_stable_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let request = WireRequest {
        operation: ProductOperation::SearchIngest {
            collection: ObjectId::new(7)?,
            batch: ProductSearchIngestBatch {
                idempotency_id: 9,
                documents: vec![ProductDocument {
                    object_id: ObjectId::new(11)?,
                    text: "golden".into(),
                    doc_values: std::collections::BTreeMap::from([(
                        "rank".into(),
                        ProductDocValue::Integer(3),
                    )]),
                    vectors: std::collections::BTreeMap::new(),
                }],
            },
        },
        logical_time_micros: 17,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    };
    let encoded = encode_product_request(&request)?;
    assert_eq!(decode_product_request(&encoded)?.logical_time_micros, 17);
    assert_eq!(
        blake3::hash(&encoded).to_hex().as_str(),
        "7268b6efce82b23b12f95ac4be9156b99c8e01d9770f9c87327561a9abc167e2"
    );
    Ok(())
}

#[test]
fn integrated_ingest_response_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let response = ProductResponse::SearchIngested(ProductSearchIngestReceipt {
        snapshot: SnapshotIdentity {
            directory_lineage: [1; 24],
            visible_csn: Some(hyphae_native_product::Csn::new(2)?),
            catalog_version: hyphae_native_product::CatalogVersion::new(3)?,
            root_digest: [4; 32],
            logical_time_micros: 5,
        },
        commit: None,
        documents: 1,
        idempotent_replay: true,
    });
    assert_eq!(
        decode_product_response(&encode_product_response(&response)?)?,
        response
    );
    Ok(())
}

#[test]
fn product_request_response_and_hyperr01_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let request = WireRequest {
        operation: ProductOperation::StructureSet {
            key: b"key".to_vec(),
            value: b"value".to_vec(),
            expires_at_micros: Some(123),
        },
        logical_time_micros: 100,
        deadline_micros: Some(200),
        idempotency_token: Some(77),
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::MEMORY,
    };
    let decoded = decode_product_request(&encode_product_request(&request)?)?;
    assert!(matches!(
        decoded.operation,
        ProductOperation::StructureSet { key, value, expires_at_micros: Some(123) }
            if key == b"key" && value == b"value"
    ));
    assert_eq!(decoded.deadline_micros, Some(200));

    let response = ProductResponse::StructureValue(Some(b"value".to_vec()));
    assert_eq!(
        decode_product_response(&encode_product_response(&response)?)?,
        response
    );
    let error = ProductError::from_code(ProductErrorCode::Cancelled).with_request_id(9);
    assert_eq!(decode_failure(&encode_failure(&error)?)?, error);
    Ok(())
}

#[test]
fn provisional_stream_requires_matching_end() -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = ProvisionalStream::new();
    stream.push(b"provisional", 64)?;
    assert!(stream.reject_incomplete().is_err());

    let mut stream = ProvisionalStream::new();
    stream.push(b"complete", 64)?;
    let completion = StreamCompletion::for_data(b"complete")?;
    assert_eq!(decode_end(&encode_end(completion))?, completion);
    assert_eq!(stream.complete(completion)?, b"complete");
    Ok(())
}

#[test]
fn every_typed_explain_variant_has_canonical_wire_round_trip() -> Result<(), ProductCodecError> {
    let object =
        hyphae_native_product::ObjectId::new(9).map_err(|_| ProductCodecError::InvalidValue)?;
    let lexical =
        hyphae_native_product::ObjectId::new(10).map_err(|_| ProductCodecError::InvalidValue)?;
    let vector =
        hyphae_native_product::ObjectId::new(11).map_err(|_| ProductCodecError::InvalidValue)?;
    let explanations = [
        hyphae_native_product::ProductExplain::SqlPlanText(hyphae_native_product::SqlPlanText {
            version: 1,
            text: "PrimaryKeyLookup(table=1)".into(),
            visible_csn: Some(7),
            catalog_version: 3,
            executed: false,
        }),
        hyphae_native_product::ProductExplain::Convergence(
            hyphae_native_product::ProductConvergenceExplanation {
                snapshot_csn: Some(7),
                strategies: vec![
                    hyphae_native_product::ProductConvergenceStrategy::ScalarLookup,
                    hyphae_native_product::ProductConvergenceStrategy::AnnTopK,
                ],
                inner_join_by_object_id: true,
                stable_object_id_order: true,
            },
        ),
        hyphae_native_product::ProductExplain::Ann(hyphae_native_product::ProductAnnExplanation {
            index: object,
            snapshot_csn: Some(7),
            approximate: true,
            build_identity: [3; 32],
            ef_search: 16,
            candidate_count: 8,
            eligible_candidate_count: 6,
            strategy: hyphae_native_product::ProductAnnStrategy::GraphTraversal,
            recall_risk: hyphae_native_product::ProductAnnRecallRisk::ApproximateTraversal,
            exact_reranked: true,
            visited_nodes: 11,
        }),
        hyphae_native_product::ProductExplain::Hybrid(
            hyphae_native_product::ProductHybridExplanation {
                lexical_index: lexical,
                lexical_limit: 5,
                vector_index: vector,
                vector_strategy: hyphae_native_product::ProductHybridVectorStrategy::Ann {
                    k: 5,
                    ef_search: 16,
                    exact_rerank: Some(8),
                },
                vector_limit: 5,
                lexical_weight: 2,
                vector_weight: 3,
                fusion_limit: 4,
                rrf_constant: 60,
            },
        ),
    ];
    for explanation in explanations {
        let response = ProductResponse::Explain(explanation);
        assert_eq!(
            decode_product_response(&encode_product_response(&response)?)?,
            response
        );
    }
    Ok(())
}

#[test]
fn telemetry_snapshot_round_trips_with_process_and_session_identity()
-> Result<(), ProductCodecError> {
    let registry = hyphae_native_product::TelemetryRegistry::default();
    registry.increment(hyphae_native_product::MetricId::Requests, 1);
    let response = ProductResponse::Telemetry(registry.snapshot(7, None));
    assert_eq!(
        decode_product_response(&encode_product_response(&response)?)?,
        response
    );
    Ok(())
}

#[test]
fn doctor_and_backup_requests_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    for operation in [
        ProductOperation::Doctor(DoctorRequest::new("/tmp/data", 17)?),
        ProductOperation::Backup(BackupRequest::new("/tmp/backup")?),
    ] {
        let request = WireRequest {
            operation,
            logical_time_micros: 17,
            deadline_micros: None,
            idempotency_token: None,
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::STRICT,
        };
        let encoded = encode_product_request(&request)?;
        let decoded = decode_product_request(&encoded)?;
        assert_eq!(decoded.logical_time_micros, 17);
    }
    Ok(())
}

#[test]
fn explicit_all_engine_transaction_family_has_canonical_round_trips()
-> Result<(), Box<dyn std::error::Error>> {
    let handle = ProductTransactionHandle::new(7).ok_or("nonzero handle")?;
    let index = ObjectId::new(11)?;
    let object = ObjectId::new(12)?;
    for operation in [
        ProductOperation::TransactionBegin,
        ProductOperation::TransactionStageSql {
            handle,
            mutation: ProductTransactionSqlMutation {
                statement: "INSERT INTO events (id) VALUES (?)".to_owned(),
                parameters: vec![ProductValue::Signed(1)],
            },
        },
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: ProductTransactionSearchMutation::Index {
                index,
                document_id: b"doc".to_vec(),
                text: "native".to_owned(),
            },
        },
        ProductOperation::TransactionStageVector {
            handle,
            mutation: ProductTransactionVectorMutation::Upsert {
                index,
                object_id: object,
                vector: ProductVector::new([1.0, 0.0])?,
            },
        },
        ProductOperation::TransactionCommit { handle },
        ProductOperation::TransactionRollback { handle },
        ProductOperation::ExplicitTransactionStatus { handle },
    ] {
        let request = WireRequest {
            operation,
            logical_time_micros: 10,
            deadline_micros: None,
            idempotency_token: Some(99),
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::MEMORY,
        };
        let decoded = decode_product_request(&encode_product_request(&request)?)?;
        assert_eq!(decoded.logical_time_micros, 10);
        assert_eq!(decoded.idempotency_token, Some(99));
    }
    let response =
        ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
            handle,
            read_csn: Some(3),
            staged_operations: 2,
            durability: hyphae_native_product::ProductDurability::Memory,
        });
    assert_eq!(
        decode_product_response(&encode_product_response(&response)?)?,
        response
    );
    Ok(())
}

#[tokio::test]
async fn negotiated_frame_payload_bounds_send_and_receive() -> Result<(), Box<dyn std::error::Error>>
{
    let hello = Hello {
        maximum_frame_payload: 64,
        ..Hello::default()
    };
    let welcome = negotiate(
        &hello,
        NegotiationPolicy::default(),
        7,
        hyphae_native_product::capabilities(),
        11,
    )?;
    assert_eq!(welcome.maximum_frame_payload, 64);

    let mut codec = AsyncFrameIo::new(usize::try_from(welcome.maximum_frame_payload)?)?;
    assert!(
        codec
            .send(&mut tokio::io::sink(), FrameKind::Ping, 1, 1, &[0; 65])
            .await
            .is_err()
    );

    let oversized = encode_frame(
        FrameKind::Ping,
        1,
        1,
        &[0; 65],
        hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD,
    )?;
    let (mut writer, mut reader) = tokio::io::duplex(oversized.len());
    writer.write_all(&oversized).await?;
    assert!(codec.receive(&mut reader).await.is_err());
    Ok(())
}
