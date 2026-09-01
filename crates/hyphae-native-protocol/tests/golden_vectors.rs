// SPDX-License-Identifier: Apache-2.0

//! Shared protocol golden vectors and strict completion controls.

use hyphae_native_product::{
    AccessControlMutationReceipt, AccessControlStatus, ApiKeyConfirmationDigest, ApiKeyId,
    ApiKeySecretDelivery, ApiKeyStartReceipt, AuthorizationEpoch, BackupRequest, BuiltInRole,
    CatalogListRequest, CatalogVisibleCursor, CatalogVisibleListFilter, CatalogVisibleListRequest,
    CatalogVisiblePage, CustomRoleGrant, CustomRoleMutationReceipt, DoctorRequest, ObjectId,
    ProductAuthorization, ProductCommitReceipt, ProductDocValue, ProductDocument,
    ProductDurability, ProductDurabilityPolicy, ProductError, ProductErrorCode,
    ProductExplicitTransactionStatus, ProductLexicalBranch, ProductLimits, ProductOperation,
    ProductPermission, ProductResponse, ProductScope, ProductSearchFilter,
    ProductSearchIngestBatch, ProductSearchIngestReceipt, ProductSearchOperator,
    ProductSearchRequest, ProductTransactionHandle, ProductTransactionId,
    ProductTransactionSearchMutation, ProductTransactionSqlMutation,
    ProductTransactionVectorMutation, ProductValue, ProductVector, RoleAssignmentMutationReceipt,
    SecurityAssignmentListRequest, SecurityAssignmentPage, SecurityAssignmentSummary,
    SecurityAuditAction, SecurityAuditEvent, SecurityAuditMetadata, SecurityAuditPage,
    SecurityAuditReadRequest, SecurityAuditResult, SecurityAuditTarget, SecurityCursor,
    SecurityCursorId, SecurityId, SecurityKeyListRequest, SecurityKeyPage, SecurityKeySummary,
    SecurityKeySummaryInput, SecurityPrincipalListRequest, SecurityPrincipalMutationReceipt,
    SecurityPrincipalPage, SecurityPrincipalSummary, SecurityRoleListRequest, SecurityRolePage,
    SecurityRoleSummary, SnapshotIdentity,
};
use hyphae_native_protocol::{
    API_KEY_AUTH_TRAILER_BYTES, AsyncFrameIo, FrameKind, GOLDEN_STRUCTURE_FRAME_BLAKE3,
    HandshakeError, Hello, NegotiationPolicy, ProductCodecError, ProtocolCapabilities,
    ProvisionalStream, StreamCompletion, WireRequest, decode_authenticated_hello, decode_end,
    decode_failure, decode_hello, decode_product_request, decode_product_request_for_minor,
    decode_product_response, decode_product_response_for_minor, decode_welcome,
    encode_authenticated_hello, encode_end, encode_failure, encode_frame, encode_hello,
    encode_product_request, encode_product_request_for_minor, encode_product_response,
    encode_product_response_for_minor, encode_welcome, golden_structure_frame, negotiate,
};
use tokio::io::AsyncWriteExt as _;

#[test]
fn shared_frame_and_handshake_vectors_are_stable() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        blake3::hash(&golden_structure_frame()?).to_hex().as_str(),
        GOLDEN_STRUCTURE_FRAME_BLAKE3
    );
    let hello = Hello {
        maximum_minor: 0,
        ..Hello::default()
    };
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
        "2c12fe5eb05cdf749d69e37060cd125535f069b2d0e1587bf8841cde674d7203"
    );
    Ok(())
}

#[test]
fn security_read_plane_requests_use_append_only_tags_and_round_trip()
-> Result<(), Box<dyn std::error::Error>> {
    let epoch = AuthorizationEpoch::new(7);
    let principal_id = SecurityId::new(1).ok_or("nonzero principal")?;
    let assignment_id = SecurityId::new(2).ok_or("nonzero assignment")?;
    let key_id = ApiKeyId::from_bytes([3; 16]).ok_or("nonzero key")?;
    let operations = [
        ProductOperation::SecurityStatus,
        ProductOperation::SecurityPrincipalList(SecurityPrincipalListRequest::new(
            Some(SecurityCursor::new(
                epoch,
                SecurityCursorId::Principal(principal_id),
            )),
            1,
        )?),
        ProductOperation::SecurityRoleList(SecurityRoleListRequest::new(
            Some(SecurityCursor::new(
                epoch,
                SecurityCursorId::BuiltInRole(BuiltInRole::Reader),
            )),
            1,
        )?),
        ProductOperation::SecurityAssignmentList(SecurityAssignmentListRequest::new(
            Some(SecurityCursor::new(
                epoch,
                SecurityCursorId::Assignment(assignment_id),
            )),
            1,
        )?),
        ProductOperation::SecurityKeyList(SecurityKeyListRequest::new(
            Some(SecurityCursor::new(epoch, SecurityCursorId::Key(key_id))),
            1,
        )?),
        ProductOperation::SecurityAuditRead(SecurityAuditReadRequest::new(Some(principal_id), 1)?),
    ];

    for (offset, operation) in operations.into_iter().enumerate() {
        let expected = format!("{operation:?}");
        let request = WireRequest {
            operation,
            logical_time_micros: 10,
            deadline_micros: None,
            idempotency_token: None,
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::MEMORY,
        };
        let encoded = encode_product_request(&request)?;
        assert_eq!(
            u16::from_le_bytes(encoded[12..14].try_into()?),
            42 + u16::try_from(offset)?
        );
        assert_eq!(
            format!("{:?}", decode_product_request(&encoded)?.operation),
            expected
        );
    }
    Ok(())
}

#[test]
fn security_read_plane_responses_use_append_only_tags_and_round_trip()
-> Result<(), Box<dyn std::error::Error>> {
    let epoch = AuthorizationEpoch::new(7);
    let responses = [
        ProductResponse::SecurityStatus(AccessControlStatus {
            bootstrapped: true,
            epoch,
            principals: 1,
            assignments: 2,
            custom_roles: 3,
            custom_assignments: 4,
            keys: 5,
            pending_keys: 4,
            audit_events: 7,
        }),
        ProductResponse::SecurityPrincipalPage(SecurityPrincipalPage {
            authorization_epoch: epoch,
            items: Box::new([]),
            next_cursor: None,
        }),
        ProductResponse::SecurityRolePage(SecurityRolePage {
            authorization_epoch: epoch,
            items: Box::new([]),
            next_cursor: None,
        }),
        ProductResponse::SecurityAssignmentPage(SecurityAssignmentPage {
            authorization_epoch: epoch,
            items: Box::new([]),
            next_cursor: None,
        }),
        ProductResponse::SecurityKeyPage(SecurityKeyPage {
            authorization_epoch: epoch,
            items: Box::new([]),
            next_cursor: None,
        }),
        ProductResponse::SecurityAuditPage(SecurityAuditPage {
            events: Box::new([]),
            next_cursor: None,
        }),
    ];

    for (offset, response) in responses.into_iter().enumerate() {
        let encoded = encode_product_response(&response)?;
        if let Some(bound) = security_page_size_bound(&response) {
            assert!(encoded.len() <= bound);
        }
        assert_eq!(
            u16::from_le_bytes(encoded[12..14].try_into()?),
            32 + u16::try_from(offset)?
        );
        assert_eq!(decode_product_response(&encoded)?, response);
    }
    Ok(())
}

#[test]
fn security_read_plane_is_available_only_after_minor_one() -> Result<(), Box<dyn std::error::Error>>
{
    let request = security_wire_request(ProductOperation::SecurityStatus);
    assert!(matches!(
        encode_product_request_for_minor(&request, 0),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded_request = encode_product_request_for_minor(&request, 1)?;
    assert!(matches!(
        decode_product_request_for_minor(&encoded_request, 0),
        Err(ProductCodecError::Unsupported)
    ));
    assert!(matches!(
        decode_product_request_for_minor(&encoded_request, 1)?.operation,
        ProductOperation::SecurityStatus
    ));

    let response = ProductResponse::SecurityStatus(AccessControlStatus {
        bootstrapped: true,
        epoch: AuthorizationEpoch::new(1),
        principals: 1,
        assignments: 1,
        custom_roles: 0,
        custom_assignments: 0,
        keys: 1,
        pending_keys: 0,
        audit_events: 1,
    });
    assert!(matches!(
        encode_product_response_for_minor(&response, 0),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded_response = encode_product_response_for_minor(&response, 1)?;
    assert!(encoded_response.len() <= AccessControlStatus::encoded_size_bound());
    assert!(matches!(
        decode_product_response_for_minor(&encoded_response, 0),
        Err(ProductCodecError::Unsupported)
    ));
    assert_eq!(
        decode_product_response_for_minor(&encoded_response, 1)?,
        response
    );
    Ok(())
}

#[test]
fn security_read_plane_nonempty_pages_have_a_redacted_golden_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let responses = nonempty_security_responses()?;

    let mut transcript = Vec::new();
    for response in responses {
        let encoded = encode_product_response(&response)?;
        let bound = security_page_size_bound(&response).ok_or("security page response expected")?;
        assert!(encoded.len() <= bound);
        assert_eq!(decode_product_response(&encoded)?, response);
        let diagnostic = String::from_utf8_lossy(&encoded);
        assert!(!diagnostic.contains("hyp1_"));
        assert!(!diagnostic.contains("verifier"));
        assert!(!diagnostic.contains("secret"));
        transcript.extend_from_slice(&encoded);
    }
    assert_eq!(
        blake3::hash(&transcript).to_hex().as_str(),
        "67c752f3f510e5b4805e097b284b2ef70fd308fa71dd0778aafc42acdf24dfe8"
    );
    Ok(())
}

fn security_page_size_bound(response: &ProductResponse) -> Option<usize> {
    match response {
        ProductResponse::SecurityPrincipalPage(page) => Some(page.encoded_size_bound()),
        ProductResponse::SecurityRolePage(page) => Some(page.encoded_size_bound()),
        ProductResponse::SecurityAssignmentPage(page) => Some(page.encoded_size_bound()),
        ProductResponse::SecurityKeyPage(page) => Some(page.encoded_size_bound()),
        ProductResponse::SecurityAuditPage(page) => Some(page.encoded_size_bound()),
        _ => None,
    }
}

fn nonempty_security_responses() -> Result<[ProductResponse; 5], Box<dyn std::error::Error>> {
    let epoch = AuthorizationEpoch::new(7);
    let principal_id = SecurityId::new(1).ok_or("nonzero principal")?;
    let custom_role_id = SecurityId::new(2).ok_or("nonzero custom role")?;
    let assignment_id = SecurityId::new(3).ok_or("nonzero assignment")?;
    let event_id = SecurityId::new(4).ok_or("nonzero event")?;
    let key_id = ApiKeyId::from_bytes([5; 16]).ok_or("nonzero key")?;
    let custom_role = SecurityRoleSummary::custom(
        custom_role_id,
        "analytics reader",
        vec![
            CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
                .ok_or("valid grant")?,
        ],
    )?;
    let key = SecurityKeySummary::try_from_wire(SecurityKeySummaryInput {
        id: key_id,
        principal_id,
        label: "analytics sdk".to_owned(),
        active: true,
        roles: vec![BuiltInRole::Reader],
        custom_roles: Vec::new(),
        permission_ceiling: ProductAuthorization::from_permissions([ProductPermission::DataRead]),
        scope_ceiling: vec![ProductScope::Instance],
        created_at_micros: 10,
        expires_at_micros: Some(20),
        revoked: false,
        published_epoch: epoch,
        predecessor_id: None,
        successor_id: None,
        overlap_until_micros: None,
        rotation_overlap_micros: None,
    })?;
    let event = SecurityAuditEvent::try_from_wire(
        event_id,
        9,
        Some(principal_id),
        Some(key_id),
        SecurityAuditAction::CreatePrincipal,
        SecurityAuditResult::Succeeded,
        vec![SecurityAuditTarget::Principal(principal_id)],
        vec![SecurityAuditMetadata::ExpiresAtMicros(20)],
    )?;
    Ok([
        ProductResponse::SecurityPrincipalPage(SecurityPrincipalPage {
            authorization_epoch: epoch,
            items: vec![SecurityPrincipalSummary::new(
                principal_id,
                "analytics",
                true,
            )?]
            .into_boxed_slice(),
            next_cursor: Some(SecurityCursor::new(
                epoch,
                SecurityCursorId::Principal(principal_id),
            )),
        }),
        ProductResponse::SecurityRolePage(SecurityRolePage {
            authorization_epoch: epoch,
            items: vec![
                SecurityRoleSummary::built_in(BuiltInRole::Reader),
                custom_role,
            ]
            .into_boxed_slice(),
            next_cursor: Some(SecurityCursor::new(
                epoch,
                SecurityCursorId::CustomRole(custom_role_id),
            )),
        }),
        ProductResponse::SecurityAssignmentPage(SecurityAssignmentPage {
            authorization_epoch: epoch,
            items: vec![SecurityAssignmentSummary::new(
                assignment_id,
                principal_id,
                Some(BuiltInRole::Reader),
                None,
                Some(ProductScope::CatalogObject(ObjectId::new(8)?)),
            )?]
            .into_boxed_slice(),
            next_cursor: Some(SecurityCursor::new(
                epoch,
                SecurityCursorId::Assignment(assignment_id),
            )),
        }),
        ProductResponse::SecurityKeyPage(SecurityKeyPage {
            authorization_epoch: epoch,
            items: vec![key].into_boxed_slice(),
            next_cursor: Some(SecurityCursor::new(epoch, SecurityCursorId::Key(key_id))),
        }),
        ProductResponse::SecurityAuditPage(SecurityAuditPage {
            events: vec![event].into_boxed_slice(),
            next_cursor: Some(event_id),
        }),
    ])
}

#[test]
fn security_read_plane_rejects_truncation_trailing_unknown_and_invalid_cursors()
-> Result<(), Box<dyn std::error::Error>> {
    let epoch = AuthorizationEpoch::new(7);
    let principal_id = SecurityId::new(1).ok_or("nonzero principal")?;
    let operation = ProductOperation::SecurityPrincipalList(SecurityPrincipalListRequest::new(
        Some(SecurityCursor::new(
            epoch,
            SecurityCursorId::Principal(principal_id),
        )),
        1,
    )?);
    let encoded = encode_product_request(&security_wire_request(operation))?;
    for prefix_length in 0..encoded.len() {
        assert!(
            decode_product_request(&encoded[..prefix_length]).is_err(),
            "request prefix length {prefix_length}"
        );
    }

    let mut trailing = encoded.clone();
    trailing.push(0);
    let trailing_length = u32::try_from(trailing.len())?;
    trailing[8..12].copy_from_slice(&trailing_length.to_le_bytes());
    assert!(matches!(
        decode_product_request(&trailing),
        Err(ProductCodecError::Malformed)
    ));

    let mut unknown = encoded.clone();
    unknown[12..14].copy_from_slice(&69_u16.to_le_bytes());
    assert!(matches!(
        decode_product_request(&unknown),
        Err(ProductCodecError::Unsupported)
    ));

    let mut zero_epoch = encoded.clone();
    zero_epoch[88..96].fill(0);
    assert!(matches!(
        decode_product_request(&zero_epoch),
        Err(ProductCodecError::InvalidValue)
    ));

    let mut zero_id = encoded.clone();
    zero_id[104..120].fill(0);
    assert!(matches!(
        decode_product_request(&zero_id),
        Err(ProductCodecError::InvalidValue)
    ));

    let mut wrong_cursor_family = encoded.clone();
    wrong_cursor_family[96] = 4;
    assert!(matches!(
        decode_product_request(&wrong_cursor_family),
        Err(ProductCodecError::InvalidValue)
    ));

    for invalid_limit in [0_u64, 1_001] {
        let mut invalid_count = encoded.clone();
        invalid_count[120..128].copy_from_slice(&invalid_limit.to_le_bytes());
        assert!(matches!(
            decode_product_request(&invalid_count),
            Err(ProductCodecError::LimitExceeded)
        ));
    }

    assert_invalid_security_request_encoders(epoch, principal_id);
    Ok(())
}

fn assert_invalid_security_request_encoders(epoch: AuthorizationEpoch, principal_id: SecurityId) {
    for limit in [0_usize, 1_001] {
        let request = security_wire_request(ProductOperation::SecurityPrincipalList(
            SecurityPrincipalListRequest {
                cursor: None,
                limit,
            },
        ));
        assert!(matches!(
            encode_product_request(&request),
            Err(ProductCodecError::LimitExceeded)
        ));
        let audit_request = security_wire_request(ProductOperation::SecurityAuditRead(
            SecurityAuditReadRequest {
                cursor: None,
                limit,
            },
        ));
        assert!(matches!(
            encode_product_request(&audit_request),
            Err(ProductCodecError::LimitExceeded)
        ));
    }
    let zero_epoch_request = security_wire_request(ProductOperation::SecurityPrincipalList(
        SecurityPrincipalListRequest {
            cursor: Some(SecurityCursor::new(
                AuthorizationEpoch::UNMANAGED,
                SecurityCursorId::Principal(principal_id),
            )),
            limit: 1,
        },
    ));
    assert!(matches!(
        encode_product_request(&zero_epoch_request),
        Err(ProductCodecError::InvalidValue)
    ));
    let cross_family_request = security_wire_request(ProductOperation::SecurityPrincipalList(
        SecurityPrincipalListRequest {
            cursor: Some(SecurityCursor::new(
                epoch,
                SecurityCursorId::Assignment(principal_id),
            )),
            limit: 1,
        },
    ));
    assert!(matches!(
        encode_product_request(&cross_family_request),
        Err(ProductCodecError::InvalidValue)
    ));
}

#[test]
fn security_read_plane_rejects_malformed_response_pages() -> Result<(), Box<dyn std::error::Error>>
{
    let response = ProductResponse::SecurityPrincipalPage(SecurityPrincipalPage {
        authorization_epoch: AuthorizationEpoch::new(7),
        items: Box::new([]),
        next_cursor: None,
    });
    let encoded = encode_product_response(&response)?;
    for prefix_length in 0..encoded.len() {
        assert!(
            decode_product_response(&encoded[..prefix_length]).is_err(),
            "response prefix length {prefix_length}"
        );
    }

    let mut trailing = encoded.clone();
    trailing.push(0);
    let trailing_length = u32::try_from(trailing.len())?;
    trailing[8..12].copy_from_slice(&trailing_length.to_le_bytes());
    assert!(matches!(
        decode_product_response(&trailing),
        Err(ProductCodecError::Malformed)
    ));

    let mut unknown = encoded.clone();
    unknown[12..14].copy_from_slice(&45_u16.to_le_bytes());
    assert!(matches!(
        decode_product_response(&unknown),
        Err(ProductCodecError::Unsupported)
    ));

    let mut zero_epoch = encoded.clone();
    zero_epoch[16..24].fill(0);
    assert!(matches!(
        decode_product_response(&zero_epoch),
        Err(ProductCodecError::InvalidValue)
    ));

    let mut excessive_count = encoded;
    excessive_count[24..28].copy_from_slice(&1_001_u32.to_le_bytes());
    assert!(matches!(
        decode_product_response(&excessive_count),
        Err(ProductCodecError::InvalidValue)
    ));

    let [principal_page, ..] = nonempty_security_responses()?;
    let mut zero_principal = encode_product_response(&principal_page)?;
    zero_principal[72..88].fill(0);
    assert!(matches!(
        decode_product_response(&zero_principal),
        Err(ProductCodecError::InvalidValue)
    ));
    Ok(())
}

#[test]
fn search_content_at_every_current_shape_is_minor_zero() -> Result<(), Box<dyn std::error::Error>> {
    // Every currently expressible search request body — all filter nodes,
    // all operators, all doc-value types — is minor-0 content. The content
    // walk exists so future operators, typed values, and fusion methods
    // raise the requirement without new operation variants.
    let request = security_wire_request(ProductOperation::SearchCollection {
        collection: ObjectId::new(13)?,
        request: ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust".to_owned(),
                candidate_limit: 8,
                weight: 1,
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::All(vec![
                ProductSearchFilter::MatchAll,
                ProductSearchFilter::Exists("category".to_owned()),
                ProductSearchFilter::Not(Box::new(ProductSearchFilter::Any(vec![
                    ProductSearchFilter::Compare {
                        field: "price".to_owned(),
                        operator: ProductSearchOperator::LessOrEqual,
                        value: ProductDocValue::Integer(40),
                    },
                    ProductSearchFilter::Compare {
                        field: "category".to_owned(),
                        operator: ProductSearchOperator::Equal,
                        value: ProductDocValue::String("book".to_owned()),
                    },
                ]))),
            ]),
            sort: Vec::new(),
            facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 4,
            fusion: None,
            parent_dedupe: None,
            rerank: None,
            highlight: None,
        },
    });
    let encoded = encode_product_request_for_minor(&request, 0)?;
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 0)?.operation,
        ProductOperation::SearchCollection { .. }
    ));

    let ingest = security_wire_request(ProductOperation::SearchIngest {
        collection: ObjectId::new(13)?,
        batch: ProductSearchIngestBatch {
            idempotency_id: 1,
            documents: vec![ProductDocument {
                object_id: ObjectId::new(201)?,
                text: "rust database".to_owned(),
                doc_values: [
                    ("flag".to_owned(), ProductDocValue::Boolean(true)),
                    ("rank".to_owned(), ProductDocValue::Integer(3)),
                    ("name".to_owned(), ProductDocValue::String("a".to_owned())),
                    ("blob".to_owned(), ProductDocValue::Bytes(vec![7])),
                ]
                .into_iter()
                .collect(),
                vectors: std::collections::BTreeMap::new(),
            }],
        },
    });
    let encoded = encode_product_request_for_minor(&ingest, 0)?;
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 0)?.operation,
        ProductOperation::SearchIngest { .. }
    ));
    Ok(())
}

#[test]
fn membership_null_and_pattern_operators_require_minor_four()
-> Result<(), Box<dyn std::error::Error>> {
    for filter in [
        ProductSearchFilter::In {
            field: "category".to_owned(),
            values: vec![
                ProductDocValue::String("book".to_owned()),
                ProductDocValue::String("gear".to_owned()),
            ],
        },
        ProductSearchFilter::IsNull("category".to_owned()),
        ProductSearchFilter::Like {
            field: "category".to_owned(),
            pattern: "bo%".to_owned(),
        },
    ] {
        let request = security_wire_request(ProductOperation::SearchCollection {
            collection: ObjectId::new(13)?,
            request: ProductSearchRequest {
                lexical: None,
                vectors: Vec::new(),
                filter,
                sort: Vec::new(),
                facets: Vec::new(),
                aggregations: Vec::new(),
                limit: 4,
                fusion: None,
                parent_dedupe: None,
                rerank: None,
                highlight: None,
            },
        });
        assert!(matches!(
            encode_product_request_for_minor(&request, 3),
            Err(ProductCodecError::Unsupported)
        ));
        let encoded = encode_product_request_for_minor(&request, 4)?;
        assert!(matches!(
            decode_product_request_for_minor(&encoded, 3),
            Err(ProductCodecError::Unsupported)
        ));
        let decoded = decode_product_request_for_minor(&encoded, 4)?;
        assert_eq!(encode_product_request(&decoded)?, encoded);
    }
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn weighted_score_fusion_requires_minor_four_and_default_bytes_are_unchanged()
-> Result<(), Box<dyn std::error::Error>> {
    let collection = ObjectId::new(13)?;
    let request = |fusion| {
        security_wire_request(ProductOperation::SearchCollection {
            collection,
            request: ProductSearchRequest {
                lexical: Some(ProductLexicalBranch {
                    query: "rust".to_owned(),
                    candidate_limit: 4,
                    weight: 1,
                }),
                vectors: Vec::new(),
                filter: ProductSearchFilter::MatchAll,
                sort: Vec::new(),
                facets: Vec::new(),
                aggregations: Vec::new(),
                limit: 4,
                fusion,
                parent_dedupe: None,
                rerank: None,
                highlight: None,
            },
        })
    };
    // The default fusion keeps the exact historical bytes: no trailing
    // selector, decodable at minor 3.
    let default_encoded = encode_product_request_for_minor(&request(None), 3)?;
    assert!(matches!(
        decode_product_request_for_minor(&default_encoded, 3)?.operation,
        ProductOperation::SearchCollection { request, .. } if request.fusion.is_none()
    ));

    let weighted = request(Some(
        hyphae_native_product::ProductFusionMethod::WeightedScore,
    ));
    assert!(matches!(
        encode_product_request_for_minor(&weighted, 3),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_request_for_minor(&weighted, 4)?;
    assert_eq!(encoded.len(), default_encoded.len() + 2);
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 3),
        Err(ProductCodecError::Unsupported)
    ));
    let decoded = decode_product_request_for_minor(&encoded, 4)?;
    assert_eq!(encode_product_request(&decoded)?, encoded);

    // Parent deduplication is a tagged section with the same discipline.
    let deduped = security_wire_request(ProductOperation::SearchCollection {
        collection,
        request: ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust".to_owned(),
                candidate_limit: 4,
                weight: 1,
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 4,
            fusion: Some(hyphae_native_product::ProductFusionMethod::WeightedScore),
            parent_dedupe: Some(hyphae_native_product::ProductParentDedupe {
                field: "parent".to_owned(),
                first_k: 2,
            }),
            rerank: None,
            highlight: None,
        },
    });
    assert!(matches!(
        encode_product_request_for_minor(&deduped, 3),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_request_for_minor(&deduped, 4)?;
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 3),
        Err(ProductCodecError::Unsupported)
    ));
    let decoded = decode_product_request_for_minor(&encoded, 4)?;
    assert_eq!(encode_product_request(&decoded)?, encoded);

    // The attested rerank stage is section tag three: envelope plus scores.
    let attestation =
        hyphae_native_product::proof::attestation::ModelAttestation::DeclaredProvider {
            provider: "openai".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            request_digest: [3; 32],
            response_digest: [4; 32],
        }
        .encode()
        .map_err(|error| format!("attestation encode failed: {error}"))?;
    let reranked = security_wire_request(ProductOperation::SearchCollection {
        collection,
        request: ProductSearchRequest {
            lexical: Some(ProductLexicalBranch {
                query: "rust".to_owned(),
                candidate_limit: 4,
                weight: 1,
            }),
            vectors: Vec::new(),
            filter: ProductSearchFilter::MatchAll,
            sort: Vec::new(),
            facets: Vec::new(),
            aggregations: Vec::new(),
            limit: 4,
            fusion: None,
            parent_dedupe: None,
            rerank: Some(hyphae_native_product::ProductRerankStage {
                attestation,
                scores: vec![(ObjectId::new(201)?, 0.75), (ObjectId::new(202)?, 0.25)],
            }),
            highlight: None,
        },
    });
    assert!(matches!(
        encode_product_request_for_minor(&reranked, 3),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_request_for_minor(&reranked, 4)?;
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 3),
        Err(ProductCodecError::Unsupported)
    ));
    let decoded = decode_product_request_for_minor(&encoded, 4)?;
    assert_eq!(encode_product_request(&decoded)?, encoded);
    // Cross-language golden: the Python and TypeScript suites pin this same
    // digest for the identically composed reranked request.
    assert_eq!(
        blake3::hash(&encoded).to_hex().as_str(),
        "f61fd68c170b8cf0841678aeda0819f7ff98869486b51ea10c104e8e2d4cee04",
    );
    Ok(())
}

#[test]
fn budgeted_highlighting_requires_minor_five_and_default_bytes_are_unchanged()
-> Result<(), Box<dyn std::error::Error>> {
    let collection = ObjectId::new(13)?;
    let request = |highlight| {
        security_wire_request(ProductOperation::SearchCollection {
            collection,
            request: ProductSearchRequest {
                lexical: Some(ProductLexicalBranch {
                    query: "rust".to_owned(),
                    candidate_limit: 4,
                    weight: 1,
                }),
                vectors: Vec::new(),
                filter: ProductSearchFilter::MatchAll,
                sort: Vec::new(),
                facets: Vec::new(),
                aggregations: Vec::new(),
                limit: 4,
                fusion: None,
                parent_dedupe: None,
                rerank: None,
                highlight,
            },
        })
    };
    // A request without highlight keeps the exact historical bytes and
    // still decodes at minor 4.
    let default_encoded = encode_product_request_for_minor(&request(None), 4)?;
    assert!(matches!(
        decode_product_request_for_minor(&default_encoded, 4)?.operation,
        ProductOperation::SearchCollection { request, .. } if request.highlight.is_none()
    ));
    let highlighted = request(Some(hyphae_native_product::ProductHighlight {
        max_fragments: 2,
        fragment_bytes: 64,
    }));
    assert!(matches!(
        encode_product_request_for_minor(&highlighted, 4),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_request_for_minor(&highlighted, 5)?;
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 4),
        Err(ProductCodecError::Unsupported)
    ));
    let decoded = decode_product_request_for_minor(&encoded, 5)?;
    assert_eq!(encode_product_request(&decoded)?, encoded);
    // Cross-language golden: the Python and TypeScript suites pin this same
    // digest for the identically composed highlighted request.
    assert_eq!(
        blake3::hash(&encoded).to_hex().as_str(),
        "1438488e4d12a342a71d1cab17bad2fecf6ddc46ecb8e73970fc6f037e5e1443",
    );

    // A result with fragments carries the content-derived response tail —
    // admitted at minor 5, refused at minor 4 — and a fragment-free result
    // keeps the exact historical bytes.
    let object = ObjectId::new(201)?;
    let catalog_version = hyphae_native_product::CatalogVersion::new(3)?;
    let result = |fragments: Vec<String>| {
        ProductResponse::IntegratedSearch(hyphae_native_product::ProductSearchResult {
            snapshot: SnapshotIdentity {
                directory_lineage: [1; 24],
                visible_csn: None,
                catalog_version,
                root_digest: [4; 32],
                logical_time_micros: 5,
            },
            hits: vec![hyphae_native_product::ProductIntegratedSearchHit {
                object_id: object,
                score: 1.5,
                doc_values: std::collections::BTreeMap::default(),
                fragments,
            }],
            facets: Vec::new(),
            aggregations: Vec::new(),
            vector_branches: Vec::new(),
            approximate: false,
            total_documents: 1,
            eligible_documents: 1,
            lexical_candidates: 1,
            retrieval_candidates: 1,
            matched_candidates: 1,
        })
    };
    let plain = encode_product_response_for_minor(&result(Vec::new()), 4)?;
    let plain_decoded = decode_product_response(&plain)?;
    assert_eq!(encode_product_response(&plain_decoded)?, plain);
    let fragmented = result(vec!["rust database".to_owned()]);
    assert!(matches!(
        encode_product_response_for_minor(&fragmented, 4),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_response_for_minor(&fragmented, 5)?;
    let decoded = decode_product_response(&encoded)?;
    assert!(matches!(
        &decoded,
        ProductResponse::IntegratedSearch(result)
            if result.hits[0].fragments == vec!["rust database".to_owned()]
    ));
    assert_eq!(encode_product_response(&decoded)?, encoded);
    Ok(())
}

fn security_wire_request(operation: ProductOperation) -> WireRequest {
    WireRequest {
        operation,
        logical_time_micros: 10,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::MEMORY,
    }
}

#[test]
fn security_write_plane_requests_use_minor_two_append_only_tags_and_redacted_goldens()
-> Result<(), Box<dyn std::error::Error>> {
    let principal_id = SecurityId::new(1).ok_or("nonzero principal")?;
    let role_id = SecurityId::new(2).ok_or("nonzero role")?;
    let assignment_id = SecurityId::new(3).ok_or("nonzero assignment")?;
    let operations = [
        ProductOperation::SecurityPrincipalCreate {
            display_name: "analytics".to_owned(),
        },
        ProductOperation::SecurityPrincipalSetEnabled {
            principal_id,
            enabled: true,
        },
        ProductOperation::SecurityCustomRoleCreate {
            display_name: "analytics reader".to_owned(),
            grants: vec![
                CustomRoleGrant::new(
                    ProductPermission::DataRead,
                    ProductScope::CatalogSubtree(ObjectId::new(9)?),
                )
                .ok_or("valid grant")?,
            ],
        },
        ProductOperation::SecurityBuiltInAssignmentCreate {
            principal_id,
            role: BuiltInRole::Owner,
            scope: ProductScope::Instance,
        },
        ProductOperation::SecurityCustomAssignmentCreate {
            principal_id,
            role_id,
        },
        ProductOperation::SecurityAssignmentRevoke { assignment_id },
    ];

    let mut transcript = Vec::new();
    for (offset, operation) in operations.into_iter().enumerate() {
        let expected = format!("{operation:?}");
        let request = security_mutation_wire_request(operation);
        let encoded = encode_product_request_for_minor(&request, 2)?;
        assert_eq!(
            u16::from_le_bytes(encoded[12..14].try_into()?),
            48 + u16::try_from(offset)?
        );
        assert_eq!(
            format!(
                "{:?}",
                decode_product_request_for_minor(&encoded, 2)?.operation
            ),
            expected
        );
        let diagnostic = String::from_utf8_lossy(&encoded);
        assert!(!diagnostic.contains("hyp1_"));
        assert!(!diagnostic.contains("verifier"));
        assert!(!diagnostic.contains("secret"));
        transcript.extend_from_slice(&encoded);
    }
    assert_eq!(
        blake3::hash(&transcript).to_hex().as_str(),
        "94b3aade7ed46f3608da3b30a5516db04a7de0e9013b33ebb3752162f17f1afc"
    );
    Ok(())
}

#[test]
fn security_write_plane_responses_use_minor_two_append_only_tags_and_redacted_goldens()
-> Result<(), Box<dyn std::error::Error>> {
    let principal_id = SecurityId::new(1).ok_or("nonzero principal")?;
    let role_id = SecurityId::new(2).ok_or("nonzero role")?;
    let assignment_id = SecurityId::new(3).ok_or("nonzero assignment")?;
    let epoch = AuthorizationEpoch::new(7);
    let commit = security_commit_receipt()?;
    let responses = [
        ProductResponse::SecurityPrincipalMutated(SecurityPrincipalMutationReceipt {
            principal_id,
            authorization_epoch: epoch,
            commit,
        }),
        ProductResponse::SecurityCustomRoleMutated(CustomRoleMutationReceipt {
            role_id,
            authorization_epoch: epoch,
            commit,
        }),
        ProductResponse::SecurityAssignmentMutated(RoleAssignmentMutationReceipt {
            assignment_id,
            authorization_epoch: epoch,
            commit,
        }),
        ProductResponse::SecurityMutated(AccessControlMutationReceipt {
            authorization_epoch: epoch,
            commit,
        }),
    ];

    let mut transcript = Vec::new();
    for (offset, response) in responses.into_iter().enumerate() {
        for minor in [0, 1] {
            assert!(matches!(
                encode_product_response_for_minor(&response, minor),
                Err(ProductCodecError::Unsupported)
            ));
        }
        let encoded = encode_product_response_for_minor(&response, 2)?;
        assert_eq!(
            u16::from_le_bytes(encoded[12..14].try_into()?),
            38 + u16::try_from(offset)?
        );
        assert_eq!(decode_product_response_for_minor(&encoded, 2)?, response);
        for minor in [0, 1] {
            assert!(matches!(
                decode_product_response_for_minor(&encoded, minor),
                Err(ProductCodecError::Unsupported)
            ));
        }
        let diagnostic = String::from_utf8_lossy(&encoded);
        assert!(!diagnostic.contains("hyp1_"));
        assert!(!diagnostic.contains("verifier"));
        assert!(!diagnostic.contains("secret"));
        transcript.extend_from_slice(&encoded);
    }
    assert_eq!(
        blake3::hash(&transcript).to_hex().as_str(),
        "797963aee6cc4aa65f38b40e08c82a2ff63e71f1e96ef28e2466bc3862e0ce34"
    );
    Ok(())
}

#[test]
fn api_key_start_wire_size_matches_product_preflight_bound()
-> Result<(), Box<dyn std::error::Error>> {
    let credential = format!("hyp1_{}_{}", "1".repeat(32), "a".repeat(64));
    let key_id = "11111111111111111111111111111111".parse()?;
    let response = ProductResponse::SecurityApiKeyStarted(ApiKeyStartReceipt {
        key_id,
        principal_id: SecurityId::new(1).ok_or("nonzero principal")?,
        predecessor_key_id: Some(key_id),
        authorization_epoch: AuthorizationEpoch::new(7),
        commit: security_commit_receipt()?,
        secret: ApiKeySecretDelivery::from_bytes(credential.as_bytes())?,
    });
    assert_eq!(
        encode_product_response(&response)?.len(),
        ApiKeyStartReceipt::wire_size_bound()
    );
    Ok(())
}

#[test]
fn security_write_plane_requires_minor_two_and_nonzero_context_idempotency()
-> Result<(), Box<dyn std::error::Error>> {
    let operation = ProductOperation::SecurityPrincipalCreate {
        display_name: "analytics".to_owned(),
    };
    let request = security_mutation_wire_request(operation);
    for minor in [0, 1] {
        assert!(matches!(
            encode_product_request_for_minor(&request, minor),
            Err(ProductCodecError::Unsupported)
        ));
    }
    let encoded = encode_product_request_for_minor(&request, 2)?;
    let mut different_token = request.clone();
    different_token.idempotency_token = Some(18);
    let encoded_different_token = encode_product_request_for_minor(&different_token, 2)?;
    assert_eq!(&encoded[..32], &encoded_different_token[..32]);
    assert_ne!(&encoded[32..48], &encoded_different_token[32..48]);
    assert_eq!(&encoded[48..], &encoded_different_token[48..]);
    for minor in [0, 1] {
        assert!(matches!(
            decode_product_request_for_minor(&encoded, minor),
            Err(ProductCodecError::Unsupported)
        ));
    }

    let mut absent = request.clone();
    absent.idempotency_token = None;
    assert!(matches!(
        encode_product_request(&absent),
        Err(ProductCodecError::InvalidValue)
    ));
    let legacy_wire = strip_request_idempotency(&encoded)?;
    assert!(matches!(
        decode_product_request(&legacy_wire),
        Err(ProductCodecError::InvalidValue)
    ));

    let mut zero = encoded;
    zero[32..48].fill(0);
    assert!(matches!(
        decode_product_request(&zero),
        Err(ProductCodecError::InvalidValue)
    ));
    Ok(())
}

#[test]
fn security_write_plane_rejects_noncanonical_bodies() -> Result<(), Box<dyn std::error::Error>> {
    let principal_id = SecurityId::new(1).ok_or("nonzero principal")?;
    for invalid_name in [String::new(), "bad\nname".to_owned(), "x".repeat(129)] {
        let request = security_mutation_wire_request(ProductOperation::SecurityPrincipalCreate {
            display_name: invalid_name,
        });
        assert!(encode_product_request(&request).is_err());
    }

    let grant = CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
        .ok_or("valid grant")?;
    for grants in [Vec::new(), vec![grant, grant]] {
        let request = security_mutation_wire_request(ProductOperation::SecurityCustomRoleCreate {
            display_name: "analytics".to_owned(),
            grants,
        });
        assert!(encode_product_request(&request).is_err());
    }

    let oversized_grants =
        security_mutation_wire_request(ProductOperation::SecurityCustomRoleCreate {
            display_name: "analytics".to_owned(),
            grants: vec![grant],
        });
    let mut oversized_grants = encode_product_request(&oversized_grants)?;
    oversized_grants[109..113].copy_from_slice(&257_u32.to_le_bytes());
    assert!(decode_product_request(&oversized_grants).is_err());

    let request = security_mutation_wire_request(ProductOperation::SecurityPrincipalSetEnabled {
        principal_id,
        enabled: true,
    });
    let encoded = encode_product_request(&request)?;
    for prefix_length in 0..encoded.len() {
        assert!(decode_product_request(&encoded[..prefix_length]).is_err());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    let trailing_length = u32::try_from(trailing.len())?;
    trailing[8..12].copy_from_slice(&trailing_length.to_le_bytes());
    assert!(matches!(
        decode_product_request(&trailing),
        Err(ProductCodecError::Malformed)
    ));
    let mut unknown = encoded.clone();
    unknown[12..14].copy_from_slice(&69_u16.to_le_bytes());
    assert!(matches!(
        decode_product_request(&unknown),
        Err(ProductCodecError::Unsupported)
    ));
    let mut zero_principal = encoded;
    zero_principal[96..112].fill(0);
    assert!(matches!(
        decode_product_request(&zero_principal),
        Err(ProductCodecError::InvalidValue)
    ));
    Ok(())
}

#[test]
fn security_write_plane_responses_reject_truncation_trailing_unknown_and_zero_fields()
-> Result<(), Box<dyn std::error::Error>> {
    let response = ProductResponse::SecurityPrincipalMutated(SecurityPrincipalMutationReceipt {
        principal_id: SecurityId::new(1).ok_or("nonzero principal")?,
        authorization_epoch: AuthorizationEpoch::new(7),
        commit: security_commit_receipt()?,
    });
    let encoded = encode_product_response(&response)?;
    for prefix_length in 0..encoded.len() {
        assert!(decode_product_response(&encoded[..prefix_length]).is_err());
    }

    let mut trailing = encoded.clone();
    trailing.push(0);
    let trailing_length = u32::try_from(trailing.len())?;
    trailing[8..12].copy_from_slice(&trailing_length.to_le_bytes());
    assert!(matches!(
        decode_product_response(&trailing),
        Err(ProductCodecError::Malformed)
    ));

    let mut unknown = encoded.clone();
    unknown[12..14].copy_from_slice(&45_u16.to_le_bytes());
    assert!(matches!(
        decode_product_response(&unknown),
        Err(ProductCodecError::Unsupported)
    ));

    let mut zero_id = encoded.clone();
    zero_id[16..32].fill(0);
    assert!(matches!(
        decode_product_response(&zero_id),
        Err(ProductCodecError::InvalidValue)
    ));

    let mut zero_epoch = encoded;
    zero_epoch[32..40].fill(0);
    assert!(matches!(
        decode_product_response(&zero_epoch),
        Err(ProductCodecError::InvalidValue)
    ));
    Ok(())
}

fn security_mutation_wire_request(operation: ProductOperation) -> WireRequest {
    WireRequest {
        operation,
        logical_time_micros: 10,
        deadline_micros: None,
        idempotency_token: Some(17),
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    }
}

fn security_commit_receipt() -> Result<ProductCommitReceipt, Box<dyn std::error::Error>> {
    Ok(ProductCommitReceipt {
        transaction_id: ProductTransactionId::new(29).ok_or("nonzero transaction")?,
        commit_csn: 31,
        catalog_version: 37,
        commit_lsn: 41,
        wal_block_digest: [43; 32],
        durability: ProductDurability::Strict,
        durability_cohort_size: 1,
        durability_cohort_position: 0,
    })
}

fn strip_request_idempotency(encoded: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut legacy = encoded.to_vec();
    legacy.drain(32..48);
    legacy[73..80].fill(0);
    let length = u32::try_from(legacy.len())?;
    legacy[8..12].copy_from_slice(&length.to_le_bytes());
    Ok(legacy)
}

#[test]
#[allow(clippy::too_many_lines)]
fn protocol_minor_negotiation_preserves_1_0_through_1_5_and_selects_1_6()
-> Result<(), Box<dyn std::error::Error>> {
    let legacy = Hello {
        maximum_minor: 0,
        ..Hello::default()
    };
    let minor_one = Hello {
        maximum_minor: 1,
        ..Hello::default()
    };
    let current = Hello::default();
    let minor_two = Hello {
        maximum_minor: 2,
        ..Hello::default()
    };
    assert_eq!(
        negotiate(
            &minor_two,
            NegotiationPolicy::default(),
            1,
            hyphae_native_product::capabilities(),
            1
        )?
        .minor,
        2
    );
    assert_eq!(
        negotiate(
            &legacy,
            NegotiationPolicy::default(),
            1,
            hyphae_native_product::capabilities(),
            1
        )?
        .minor,
        0
    );
    assert_eq!(
        negotiate(
            &minor_one,
            NegotiationPolicy::default(),
            1,
            hyphae_native_product::capabilities(),
            1
        )?
        .minor,
        1
    );
    let minor_three = Hello {
        maximum_minor: 3,
        ..Hello::default()
    };
    assert_eq!(
        negotiate(
            &minor_three,
            NegotiationPolicy::default(),
            1,
            hyphae_native_product::capabilities(),
            1
        )?
        .minor,
        3
    );
    let minor_four = Hello {
        maximum_minor: 4,
        ..Hello::default()
    };
    assert_eq!(
        negotiate(
            &minor_four,
            NegotiationPolicy::default(),
            1,
            hyphae_native_product::capabilities(),
            1
        )?
        .minor,
        4
    );
    let minor_five = Hello {
        maximum_minor: 5,
        ..Hello::default()
    };
    assert_eq!(
        negotiate(
            &minor_five,
            NegotiationPolicy::default(),
            1,
            hyphae_native_product::capabilities(),
            1
        )?
        .minor,
        5
    );
    assert_eq!(
        negotiate(
            &current,
            NegotiationPolicy::default(),
            1,
            hyphae_native_product::capabilities(),
            1
        )?
        .minor,
        6
    );

    let incompatible = Hello {
        minimum_minor: 7,
        maximum_minor: 7,
        ..Hello::default()
    };
    assert_eq!(
        negotiate(
            &incompatible,
            NegotiationPolicy::default(),
            1,
            hyphae_native_product::capabilities(),
            1
        )
        .err(),
        Some(HandshakeError::IncompatibleVersion)
    );
    Ok(())
}

#[test]
fn catalog_visible_list_uses_minor_three_append_only_tags_and_opaque_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let cursor = CatalogVisibleCursor::new(vec![7; 184])?;
    let request = WireRequest {
        operation: ProductOperation::CatalogVisibleList(CatalogVisibleListRequest {
            filter: CatalogVisibleListFilter {
                parent: Some(ObjectId::new(11)?),
                kind: None,
            },
            cursor: Some(cursor.clone()),
            item_limit: 2,
            visit_limit: 3,
            byte_limit: 4096,
        }),
        logical_time_micros: 0,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    };
    assert!(matches!(
        encode_product_request_for_minor(&request, 2),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_request_for_minor(&request, 3)?;
    assert_eq!(u16::from_le_bytes(encoded[12..14].try_into()?), 54);
    let decoded = decode_product_request_for_minor(&encoded, 3)?;
    let ProductOperation::CatalogVisibleList(decoded) = decoded.operation else {
        return Err("wrong visible catalog request variant".into());
    };
    assert_eq!(decoded.cursor, Some(cursor));

    let response = ProductResponse::CatalogVisiblePage(CatalogVisiblePage {
        items: Vec::new(),
        cursor: decoded.cursor,
    });
    assert!(matches!(
        encode_product_response_for_minor(&response, 2),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_response_for_minor(&response, 3)?;
    assert_eq!(u16::from_le_bytes(encoded[12..14].try_into()?), 42);
    assert_eq!(decode_product_response_for_minor(&encoded, 3)?, response);
    Ok(())
}

#[test]
fn api_key_lifecycle_uses_minor_three_append_only_tags_without_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let key_id = ApiKeyId::from_bytes([7; 16]).ok_or("zero key id")?;
    let request = WireRequest {
        operation: ProductOperation::SecurityApiKeyIssueSelfActivate {
            key_id,
            confirmation_digest: ApiKeyConfirmationDigest::from_bytes([9; 32]),
        },
        logical_time_micros: 0,
        deadline_micros: None,
        idempotency_token: Some(1),
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    };
    assert!(matches!(
        encode_product_request_for_minor(&request, 2),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_request_for_minor(&request, 3)?;
    assert_eq!(u16::from_le_bytes(encoded[12..14].try_into()?), 57);
    assert!(!encoded.windows(4).any(|window| window == b"path"));
    let decoded = decode_product_request_for_minor(&encoded, 3)?;
    assert!(matches!(
        decoded.operation,
        ProductOperation::SecurityApiKeyIssueSelfActivate { key_id: decoded, .. } if decoded == key_id
    ));
    Ok(())
}

#[test]
fn catalog_list_minor_zero_through_two_is_byte_identical() -> Result<(), Box<dyn std::error::Error>>
{
    let request = WireRequest {
        operation: ProductOperation::CatalogList(CatalogListRequest {
            parent: Some(ObjectId::new(11)?),
            kind: None,
            cursor: None,
            item_limit: 2,
            visit_limit: 3,
            byte_limit: 4_096,
        }),
        logical_time_micros: 0,
        deadline_micros: None,
        idempotency_token: None,
        limits: ProductLimits::default(),
        durability: ProductDurabilityPolicy::STRICT,
    };
    let minor_zero = encode_product_request_for_minor(&request, 0)?;
    assert_eq!(minor_zero, encode_product_request_for_minor(&request, 1)?);
    assert_eq!(minor_zero, encode_product_request_for_minor(&request, 2)?);
    assert_eq!(u16::from_le_bytes(minor_zero[12..14].try_into()?), 15);
    assert_eq!(
        blake3::hash(&minor_zero).to_hex().as_str(),
        "d2fb175427b7c9b6b28f2444b6494d72f736efffb90c56c35d79e3f30ca5561b"
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
        maximum_minor: 0,
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

#[test]
fn minor_six_structure_reads_gate_and_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    use hyphae_native_product::{
        ProductHashScanStop, ProductRead, ProductScoreBound, ProductSortedSetOrder,
        ProductStructureKey, ProductStructureReadRequest, ProductStructureReadResult,
    };

    let keyspace = ObjectId::new(9)?;
    let requests = [
        ProductStructureReadRequest::SortedSetScoreRange {
            key: ProductStructureKey {
                keyspace,
                key: b"board".to_vec(),
            },
            lower: ProductScoreBound::Exclusive(1.5),
            upper: ProductScoreBound::Unbounded,
            offset: 2,
            limit: 16,
            order: ProductSortedSetOrder::Descending,
        },
        ProductStructureReadRequest::HashScanReverse {
            key: ProductStructureKey {
                keyspace,
                key: b"profile".to_vec(),
            },
            start_before: Some(b"user:2".to_vec()),
            limit: 8,
        },
        ProductStructureReadRequest::HashScanMatch {
            key: ProductStructureKey {
                keyspace,
                key: b"profile".to_vec(),
            },
            pattern: b"user:*".to_vec(),
            start_after: None,
            output_limit: 8,
            visit_limit: 32,
            match_step_limit: 256,
        },
    ];
    for request in requests {
        let wire = security_wire_request(ProductOperation::StructureRead(request));
        // Below minor 6 the request is unsupported on both directions.
        assert!(matches!(
            encode_product_request_for_minor(&wire, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let encoded = encode_product_request_for_minor(&wire, 6)?;
        assert!(matches!(
            decode_product_request_for_minor(&encoded, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let decoded = decode_product_request_for_minor(&encoded, 6)?;
        let ProductOperation::StructureRead(decoded_read) = decoded.operation else {
            return Err("decoded operation is not a structure read".into());
        };
        let ProductOperation::StructureRead(original_read) = wire.operation else {
            return Err("original operation is not a structure read".into());
        };
        assert_eq!(format!("{decoded_read:?}"), format!("{original_read:?}"));
    }

    // HashPage response gates identically and round-trips.
    let response = ProductResponse::StructureRead(ProductRead {
        snapshot: SnapshotIdentity {
            directory_lineage: [7; 24],
            visible_csn: hyphae_native_product::Csn::new(3).ok(),
            catalog_version: hyphae_native_product::CatalogVersion::new(2)?,
            root_digest: [9; 32],
            logical_time_micros: 10,
        },
        value: ProductStructureReadResult::HashPage {
            entries: vec![hyphae_native_product::ProductHashEntry {
                field: b"user:1".to_vec(),
                value: b"ana".to_vec(),
            }],
            continuation: Some(b"user:1".to_vec()),
            stop: ProductHashScanStop::VisitLimit,
            visited: 3,
            match_steps: 12,
        },
    });
    assert!(matches!(
        encode_product_response_for_minor(&response, 5),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_response_for_minor(&response, 6)?;
    assert!(matches!(
        decode_product_response_for_minor(&encoded, 5),
        Err(ProductCodecError::Unsupported)
    ));
    let decoded = decode_product_response_for_minor(&encoded, 6)?;
    assert_eq!(decoded, response);
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn minor_six_key_scan_and_sorted_set_mutations_gate_and_round_trip()
-> Result<(), Box<dyn std::error::Error>> {
    use hyphae_native_product::{
        CanonicalF64, ProductHashScanStop, ProductRead, ProductStructureKey,
        ProductStructureMutation, ProductStructureMutationResult, ProductStructureReadRequest,
        ProductStructureReadResult, ProductTransactionHandle, ProductTransactionStageReceipt,
        ProductTransactionStageResult, StructureKind,
    };

    let keyspace = ObjectId::new(9)?;

    // KeyScanMatch request gates below minor 6 and round-trips at 6.
    let request = ProductStructureReadRequest::KeyScanMatch {
        keyspace,
        pattern: b"app:*".to_vec(),
        start_after: Some(b"app:flag".to_vec()),
        output_limit: 8,
        visit_limit: 32,
        match_step_limit: 256,
    };
    let wire = security_wire_request(ProductOperation::StructureRead(request));
    assert!(matches!(
        encode_product_request_for_minor(&wire, 5),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_request_for_minor(&wire, 6)?;
    assert!(matches!(
        decode_product_request_for_minor(&encoded, 5),
        Err(ProductCodecError::Unsupported)
    ));
    let decoded = decode_product_request_for_minor(&encoded, 6)?;
    assert_eq!(
        format!("{:?}", decoded.operation),
        format!("{:?}", wire.operation),
    );

    // SortedSetIncrement / SortedSetPop mutations gate on both surfaces.
    let mutations = [
        ProductStructureMutation::SortedSetIncrement {
            key: ProductStructureKey {
                keyspace,
                key: b"board".to_vec(),
            },
            member: b"alpha".to_vec(),
            delta: CanonicalF64::new(2.5),
        },
        ProductStructureMutation::SortedSetPop {
            key: ProductStructureKey {
                keyspace,
                key: b"board".to_vec(),
            },
            highest: true,
        },
    ];
    for mutation in mutations {
        let wire = security_wire_request(ProductOperation::StructureMutate {
            mutations: vec![mutation],
        });
        assert!(matches!(
            encode_product_request_for_minor(&wire, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let encoded = encode_product_request_for_minor(&wire, 6)?;
        assert!(matches!(
            decode_product_request_for_minor(&encoded, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let decoded = decode_product_request_for_minor(&encoded, 6)?;
        assert_eq!(
            format!("{:?}", decoded.operation),
            format!("{:?}", wire.operation),
        );
    }

    // KeyPage response gates and round-trips with families intact.
    let response = ProductResponse::StructureRead(ProductRead {
        snapshot: SnapshotIdentity {
            directory_lineage: [7; 24],
            visible_csn: hyphae_native_product::Csn::new(3).ok(),
            catalog_version: hyphae_native_product::CatalogVersion::new(2)?,
            root_digest: [9; 32],
            logical_time_micros: 10,
        },
        value: ProductStructureReadResult::KeyPage {
            entries: vec![
                hyphae_native_product::ProductKeyEntry {
                    key: b"app:board".to_vec(),
                    family: StructureKind::SortedSet,
                },
                hyphae_native_product::ProductKeyEntry {
                    key: b"app:flag".to_vec(),
                    family: StructureKind::String,
                },
            ],
            continuation: Some(b"app:flag".to_vec()),
            stop: ProductHashScanStop::OutputLimit,
            visited: 2,
            match_steps: 9,
        },
    });
    assert!(matches!(
        encode_product_response_for_minor(&response, 5),
        Err(ProductCodecError::Unsupported)
    ));
    let encoded = encode_product_response_for_minor(&response, 6)?;
    assert!(matches!(
        decode_product_response_for_minor(&encoded, 5),
        Err(ProductCodecError::Unsupported)
    ));
    let decoded = decode_product_response_for_minor(&encoded, 6)?;
    assert_eq!(decoded, response);

    // Staged Score and PoppedEntry results gate and round-trip.
    let handle = ProductTransactionHandle::new(4).ok_or("handle")?;
    let staged_results = [
        ProductTransactionStageResult::Structure(ProductStructureMutationResult::Score(
            CanonicalF64::new(4.0),
        )),
        ProductTransactionStageResult::Structure(ProductStructureMutationResult::PoppedEntry(
            Some(hyphae_native_product::ProductSortedSetEntry {
                member: b"bravo".to_vec(),
                score: CanonicalF64::new(1.5),
            }),
        )),
        ProductTransactionStageResult::Structure(ProductStructureMutationResult::PoppedEntry(None)),
    ];
    for result in staged_results {
        let response = ProductResponse::TransactionStaged(ProductTransactionStageReceipt {
            handle,
            operation_ordinal: 1,
            changed: true,
            result,
        });
        assert!(matches!(
            encode_product_response_for_minor(&response, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let encoded = encode_product_response_for_minor(&response, 6)?;
        assert!(matches!(
            decode_product_response_for_minor(&encoded, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let decoded = decode_product_response_for_minor(&encoded, 6)?;
        assert_eq!(decoded, response);
    }
    Ok(())
}

#[test]
fn minor_six_conditional_and_seeded_tags_gate_and_round_trip()
-> Result<(), Box<dyn std::error::Error>> {
    use hyphae_native_product::{
        ProductStructureKey, ProductStructureMutation, ProductStructureReadRequest,
    };

    let keyspace = ObjectId::new(9)?;
    let key = |name: &[u8]| ProductStructureKey {
        keyspace,
        key: name.to_vec(),
    };

    // New reads gate below minor 6 and round-trip at 6.
    let requests = [
        ProductStructureReadRequest::StringRange {
            key: key(b"greeting"),
            start: -5,
            end: -1,
        },
        ProductStructureReadRequest::SetRandomMembers {
            key: key(b"tags"),
            seed: 42,
            count: 3,
        },
    ];
    for request in requests {
        let wire = security_wire_request(ProductOperation::StructureRead(request));
        assert!(matches!(
            encode_product_request_for_minor(&wire, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let encoded = encode_product_request_for_minor(&wire, 6)?;
        assert!(matches!(
            decode_product_request_for_minor(&encoded, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let decoded = decode_product_request_for_minor(&encoded, 6)?;
        assert_eq!(
            format!("{:?}", decoded.operation),
            format!("{:?}", wire.operation),
        );
    }

    // New mutations gate identically on both surfaces.
    let mutations = [
        ProductStructureMutation::StringSetConditional {
            key: key(b"greeting"),
            value: b"hello".to_vec(),
            expires_at_micros: Some(99),
            if_present: true,
        },
        ProductStructureMutation::StringAppend {
            key: key(b"greeting"),
            suffix: b" world".to_vec(),
        },
        ProductStructureMutation::StringSetRange {
            key: key(b"padded"),
            offset: 4,
            patch: b"tail".to_vec(),
        },
        ProductStructureMutation::HashSetIfAbsent {
            key: key(b"profile"),
            field: b"city".to_vec(),
            value: b"lima".to_vec(),
        },
        ProductStructureMutation::SetPop {
            key: key(b"tags"),
            seed: 42,
        },
    ];
    for mutation in mutations {
        let wire = security_wire_request(ProductOperation::StructureMutate {
            mutations: vec![mutation],
        });
        assert!(matches!(
            encode_product_request_for_minor(&wire, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let encoded = encode_product_request_for_minor(&wire, 6)?;
        assert!(matches!(
            decode_product_request_for_minor(&encoded, 5),
            Err(ProductCodecError::Unsupported)
        ));
        let decoded = decode_product_request_for_minor(&encoded, 6)?;
        assert_eq!(
            format!("{:?}", decoded.operation),
            format!("{:?}", wire.operation),
        );
    }
    Ok(())
}
