// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used)]

//! Cross-crate G6 native proof and witness conformance tests.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_catalog::{
    CatalogName, CatalogObjectV2, DefinitionVersion, LogicalCatalogObject, ObjectHeaderV2,
    QualifiedName,
};
use hyphae_native_product::proof::{
    AdmittedProofLimits, AnnFilterStrategy, AnnProofMetadata, ApproximationLabel, CanonicalBytes,
    CompletionStatus, ExternalTrustedAnchor, HybridBranchBinding, HybridDuplicatePolicy,
    HybridFailurePolicy, HybridFusionMethod, HybridProofMetadata, NativeProof, NativeProofAnchor,
    NativeProofContent, NativeProofError, NativeProofGenerationLimits, NativeProofKind,
    NativeVerificationLimits, NativeVerificationScope, NativeWitnessReference, ProofCodecLimits,
    ProofObjectBinding, VectorMetric, WitnessCodecLimits, bundle_native_witness,
    decode_native_proof, decode_native_witness, encode_native_proof,
    generate_native_operation_proof, verify_native_proof_offline,
};
use hyphae_native_product::{
    CatalogListRequest, NativeProduct, ProductAuthorization, ProductOperation, ProductPrincipal,
    ProductRequestContext, ProductSession, ProductSessionId, ProductValue,
};
use hyphae_native_runtime::BoundedSearchQuery;
use hyphae_native_types::{EngineKind, ObjectId};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-native-proof-{name}-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ))
}

fn anchor(seed: u8) -> NativeProofAnchor {
    NativeProofAnchor {
        directory_lineage: [seed; 24],
        history_epoch: 3,
        visible_csn: 17,
        catalog_version: 9,
        root_digest: [seed.wrapping_add(1); 32],
        checkpoint_sequence: 17,
        checkpoint_digest: [seed.wrapping_add(2); 32],
    }
}

fn proof_content(
    kind: NativeProofKind,
    anchor: NativeProofAnchor,
    witness: NativeWitnessReference,
) -> NativeProofContent {
    let ann = (kind == NativeProofKind::Ann).then_some(AnnProofMetadata {
        metric: VectorMetric::Cosine,
        index_definition_digest: [11; 32],
        graph_generation_digest: [12; 32],
        search_breadth: 64,
        filter_strategy: AnnFilterStrategy::Iterative,
        eligible_set_digest: [13; 32],
        visited_count: 40,
        candidate_count: 20,
        rerank_count: 10,
        approximation: ApproximationLabel::ApproximateWithExactOracle,
        exact_oracle_digest: Some([14; 32]),
    });
    let hybrid = (kind == NativeProofKind::Hybrid).then_some(HybridProofMetadata {
        branches: vec![
            HybridBranchBinding {
                proof_digest: [21; 32],
                weight_millionths: 600_000,
                candidate_limit: 20,
            },
            HybridBranchBinding {
                proof_digest: [22; 32],
                weight_millionths: 400_000,
                candidate_limit: 30,
            },
        ],
        failure_policy: HybridFailurePolicy::FailClosed,
        fusion_method: HybridFusionMethod::WeightedReciprocalRank,
        duplicate_policy: HybridDuplicatePolicy::MergeByObjectId,
    });
    NativeProofContent {
        kind,
        anchor,
        semantics_version: 1,
        ordering_version: 1,
        objects: vec![
            ProofObjectBinding {
                object_id: 7,
                definition_digest: [7; 32],
            },
            ProofObjectBinding {
                object_id: 8,
                definition_digest: [8; 32],
            },
        ],
        request: CanonicalBytes::new(b"canonical request".to_vec()),
        result: CanonicalBytes::new(b"canonical ordered result".to_vec()),
        evidence: CanonicalBytes::new(b"canonical execution evidence".to_vec()),
        limits: AdmittedProofLimits {
            result_items: 100,
            candidate_items: 100,
            evidence_bytes: 1_024,
        },
        completion: CompletionStatus::Complete,
        witness,
        ann,
        hybrid,
    }
}

#[allow(clippy::type_complexity)]
fn artifact(
    kind: NativeProofKind,
) -> Result<(Vec<u8>, Vec<u8>, NativeProofAnchor), Box<dyn std::error::Error>> {
    let origin = temporary("origin");
    fs::create_dir_all(origin.join("pages/empty"))?;
    fs::write(origin.join("ROOT"), b"root manifest")?;
    fs::write(origin.join("pages/0001.hyp"), b"immutable page bytes")?;
    let anchor = anchor(3);
    let witness = bundle_native_witness(&origin, anchor, &WitnessCodecLimits::default())?;
    let proof = NativeProof::new(proof_content(kind, anchor, witness.reference()?))?;
    let proof_bytes = encode_native_proof(&proof, &ProofCodecLimits::default())?;
    fs::remove_dir_all(origin)?;
    Ok((proof_bytes, witness.bytes, anchor))
}

#[test]
fn every_proof_kind_has_a_canonical_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    for kind in [
        NativeProofKind::Point,
        NativeProofKind::Sql,
        NativeProofKind::Lexical,
        NativeProofKind::ExactVector,
        NativeProofKind::Ann,
        NativeProofKind::Hybrid,
        NativeProofKind::Catalog,
    ] {
        let (proof_bytes, witness_bytes, anchor) = artifact(kind)?;
        assert!(proof_bytes.starts_with(b"HYNPRF02"));
        assert!(witness_bytes.starts_with(b"HYNWIT02"));
        let mut legacy_magic = proof_bytes.clone();
        legacy_magic[..8].copy_from_slice(b"HYNPRF01");
        assert!(decode_native_proof(&legacy_magic, &ProofCodecLimits::default()).is_err());
        let proof = decode_native_proof(&proof_bytes, &ProofCodecLimits::default())?;
        assert_eq!(proof.content().kind, kind);
        assert_eq!(
            encode_native_proof(&proof, &ProofCodecLimits::default())?,
            proof_bytes
        );
        let witness = decode_native_witness(&witness_bytes, &WitnessCodecLimits::default())?;
        assert_eq!(witness.entries().len(), 4);
        let report = verify_native_proof_offline(
            &proof_bytes,
            &witness_bytes,
            ExternalTrustedAnchor::new(anchor.digest()),
            &NativeVerificationLimits::default(),
        )?;
        assert_eq!(report.scope, NativeVerificationScope::ArtifactIntegrity);
        assert_eq!(report.kind, kind);
        assert_eq!(report.file_count, 2);
        assert_eq!(report.directory_count, 2);
        assert!(!report.semantic_reexecution_performed);
    }
    Ok(())
}

fn principal() -> ProductPrincipal {
    ProductPrincipal::new("proof-test").expect("bounded principal")
}

fn session() -> ProductSession {
    ProductSession::new(
        ProductSessionId::new(1).expect("nonzero session"),
        principal(),
        ProductAuthorization::ALL,
    )
}

fn context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    )
}

#[test]
fn generated_sql_proof_reexecutes_after_origin_deletion_and_rejects_semantic_forgery()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("semantic-sql-origin");
    let mut product = NativeProduct::create(&path)?;
    let mut session = session();
    let create_context = context(&session, 1);
    product.dispatch(
        &mut session,
        &create_context,
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE items (id BIGINT PRIMARY KEY, label TEXT NOT NULL)".into(),
            parameters: Vec::new(),
        },
    )?;
    let insert_context = context(&session, 2);
    product.dispatch(
        &mut session,
        &insert_context,
        ProductOperation::ExecuteSql {
            statement: "INSERT INTO items (id, label) VALUES (?, ?)".into(),
            parameters: vec![ProductValue::Signed(7), ProductValue::Text("seven".into())],
        },
    )?;
    let proof_context = context(&session, 3);
    let (response, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &proof_context,
        &ProductOperation::ExecuteSql {
            statement: "SELECT label FROM items WHERE id = ?".into(),
            parameters: vec![ProductValue::Signed(7)],
        },
        NativeProofGenerationLimits::default(),
    )?;
    assert!(matches!(
        response,
        hyphae_native_product::ProductResponse::Sql { .. }
    ));
    let anchor = artifact.trusted_anchor;
    let witness = artifact.witness_bytes.clone();
    let mut forged = artifact.proof.content().clone();
    forged.result = CanonicalBytes::new(b"self-consistent but false result".to_vec());
    let forged = NativeProof::new(forged)?;
    let forged = encode_native_proof(&forged, &ProofCodecLimits::default())?;

    drop(product);
    fs::remove_dir_all(&path)?;
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &witness,
        anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert_eq!(report.scope, NativeVerificationScope::SemanticReexecution);
    assert!(report.semantic_reexecution_performed);
    assert!(
        verify_native_proof_offline(
            &forged,
            &witness,
            anchor,
            &NativeVerificationLimits::default(),
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn catalog_proofs_retain_full_128_bit_object_ids_and_reexecute_list_and_describe()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("semantic-catalog-high-id");
    let mut product = NativeProduct::create(&path)?;
    let high = (u128::from(u64::MAX) << 32) | 0xfeed;
    let object = LogicalCatalogObject::V2(CatalogObjectV2::Database(ObjectHeaderV2 {
        id: ObjectId::new(high)?,
        owner: EngineKind::Kernel,
        name: QualifiedName::new(
            CatalogName::unquoted("main")?,
            CatalogName::unquoted("public")?,
            CatalogName::unquoted("high_id")?,
        ),
        parent: None,
        definition_version: DefinitionVersion::FIRST,
    }));
    product.create_catalog_object_v2(object, hyphae_native_product::ProductDurability::Strict)?;
    let mut session = session();

    for (request_id, operation) in [
        (
            1,
            ProductOperation::CatalogDescribe {
                id: ObjectId::new(high)?,
            },
        ),
        (
            2,
            ProductOperation::CatalogList(CatalogListRequest {
                parent: None,
                kind: None,
                cursor: None,
                item_limit: 10,
                visit_limit: 10,
                byte_limit: 16 * 1024,
            }),
        ),
    ] {
        let proof_context = context(&session, request_id);
        let (_, artifact) = generate_native_operation_proof(
            &mut product,
            &mut session,
            &proof_context,
            &operation,
            NativeProofGenerationLimits::default(),
        )?;
        assert!(
            artifact
                .proof
                .content()
                .objects
                .iter()
                .any(|binding| binding.object_id == high)
        );
        let report = verify_native_proof_offline(
            &artifact.proof_bytes,
            &artifact.witness_bytes,
            artifact.trusted_anchor,
            &NativeVerificationLimits::default(),
        )?;
        assert!(report.semantic_reexecution_performed);
    }
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn lexical_proof_reexecutes_ordered_hits_and_work_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("semantic-lexical");
    let index = ObjectId::new((u128::from(u64::MAX) << 1) | 1)?;
    let mut runtime = hyphae_native_runtime::NativeDatabase::create(&path)?;
    let mut transaction = runtime.begin(0, hyphae_native_types::DurabilityClass::Strict)?;
    transaction.create_search_index(index, "documents")?;
    transaction.index_document(index, b"first".to_vec(), "rust database")?;
    transaction.index_document(index, b"second".to_vec(), "rust rust")?;
    transaction.commit()?;
    drop(runtime);
    let mut product = NativeProduct::open(&path)?;
    let mut session = session();
    let point_context = context(&session, 1);
    let (_, point) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &point_context,
        &ProductOperation::CatalogObject { id: index },
        NativeProofGenerationLimits::default(),
    )?;
    assert_eq!(point.proof.content().kind, NativeProofKind::Point);
    assert!(
        point
            .proof
            .content()
            .objects
            .iter()
            .any(|binding| binding.object_id == index.get())
    );
    assert!(
        verify_native_proof_offline(
            &point.proof_bytes,
            &point.witness_bytes,
            point.trusted_anchor,
            &NativeVerificationLimits::default(),
        )?
        .semantic_reexecution_performed
    );
    let proof_context = context(&session, 2);
    let (_, artifact) = generate_native_operation_proof(
        &mut product,
        &mut session,
        &proof_context,
        &ProductOperation::Search {
            index,
            query: BoundedSearchQuery::Term("rust".into()),
            limit: 2,
        },
        NativeProofGenerationLimits::default(),
    )?;
    let report = verify_native_proof_offline(
        &artifact.proof_bytes,
        &artifact.witness_bytes,
        artifact.trusted_anchor,
        &NativeVerificationLimits::default(),
    )?;
    assert_eq!(report.kind, NativeProofKind::Lexical);
    assert!(report.semantic_reexecution_performed);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn verification_is_origin_independent_and_anchor_is_external()
-> Result<(), Box<dyn std::error::Error>> {
    let (proof, witness, anchor) = artifact(NativeProofKind::Sql)?;
    let report = verify_native_proof_offline(
        &proof,
        &witness,
        ExternalTrustedAnchor::new(anchor.digest()),
        &NativeVerificationLimits::default(),
    )?;
    assert_eq!(report.anchor_digest, anchor.digest());

    let error = verify_native_proof_offline(
        &proof,
        &witness,
        ExternalTrustedAnchor::new([99; 32]),
        &NativeVerificationLimits::default(),
    )
    .err()
    .ok_or("untrusted proof anchor was accepted")?;
    assert!(matches!(error, NativeProofError::TrustedAnchorMismatch));
    Ok(())
}

#[test]
fn every_proof_and_witness_truncation_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let (proof, witness, _) = artifact(NativeProofKind::Ann)?;
    for length in 0..proof.len() {
        assert!(decode_native_proof(&proof[..length], &ProofCodecLimits::default()).is_err());
    }
    for length in 0..witness.len() {
        assert!(decode_native_witness(&witness[..length], &WitnessCodecLimits::default()).is_err());
    }
    Ok(())
}

#[test]
fn envelope_and_payload_tampering_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let (proof, witness, _) = artifact(NativeProofKind::Hybrid)?;
    for index in [0, 8, 12, 16, 24, 32, 63, 64, proof.len() - 1] {
        let mut changed = proof.clone();
        changed[index] ^= 0x5a;
        assert!(decode_native_proof(&changed, &ProofCodecLimits::default()).is_err());
    }
    for index in [0, 8, 12, 16, 24, 32, 63, 64, witness.len() - 1] {
        let mut changed = witness.clone();
        changed[index] ^= 0xa5;
        assert!(decode_native_witness(&changed, &WitnessCodecLimits::default()).is_err());
    }
    let mut proof_with_trailing = proof;
    proof_with_trailing.push(0);
    assert!(decode_native_proof(&proof_with_trailing, &ProofCodecLimits::default()).is_err());
    let mut witness_with_trailing = witness;
    witness_with_trailing.push(0);
    assert!(decode_native_witness(&witness_with_trailing, &WitnessCodecLimits::default()).is_err());
    Ok(())
}

#[test]
fn proof_and_witness_references_cannot_be_substituted() -> Result<(), Box<dyn std::error::Error>> {
    let (proof, witness, trusted_state) = artifact(NativeProofKind::Point)?;
    let other_origin = temporary("other-origin");
    fs::create_dir(&other_origin)?;
    fs::write(other_origin.join("different"), b"different bytes")?;
    let other =
        bundle_native_witness(&other_origin, trusted_state, &WitnessCodecLimits::default())?;
    fs::remove_dir_all(other_origin)?;
    let error = verify_native_proof_offline(
        &proof,
        &other.bytes,
        ExternalTrustedAnchor::new(trusted_state.digest()),
        &NativeVerificationLimits::default(),
    )
    .err()
    .ok_or("substituted witness was accepted")?;
    assert!(matches!(error, NativeProofError::WitnessReferenceMismatch));

    let foreign_anchor = anchor(31);
    let foreign_origin = temporary("foreign-origin");
    fs::create_dir(&foreign_origin)?;
    let foreign = bundle_native_witness(
        &foreign_origin,
        foreign_anchor,
        &WitnessCodecLimits::default(),
    )?;
    fs::remove_dir_all(foreign_origin)?;
    let error = verify_native_proof_offline(
        &proof,
        &foreign.bytes,
        ExternalTrustedAnchor::new(trusted_state.digest()),
        &NativeVerificationLimits::default(),
    )
    .err()
    .ok_or("foreign witness anchor was accepted")?;
    assert!(matches!(error, NativeProofError::WitnessAnchorMismatch));
    assert_ne!(witness, foreign.bytes);
    Ok(())
}

#[test]
fn codecs_enforce_encoded_decoded_count_and_file_limits() -> Result<(), Box<dyn std::error::Error>>
{
    let (proof, witness, _) = artifact(NativeProofKind::Hybrid)?;
    let proof_length = u64::try_from(proof.len())?;
    let error = decode_native_proof(
        &proof,
        &ProofCodecLimits {
            max_proof_bytes: proof_length - 1,
            ..ProofCodecLimits::default()
        },
    )
    .err()
    .ok_or("proof byte limit was ignored")?;
    assert!(matches!(error, NativeProofError::LimitExceeded { .. }));
    assert!(
        decode_native_proof(
            &proof,
            &ProofCodecLimits {
                max_section_bytes: 4,
                ..ProofCodecLimits::default()
            }
        )
        .is_err()
    );
    assert!(
        decode_native_proof(
            &proof,
            &ProofCodecLimits {
                max_objects: 1,
                ..ProofCodecLimits::default()
            }
        )
        .is_err()
    );
    assert!(
        decode_native_proof(
            &proof,
            &ProofCodecLimits {
                max_hybrid_branches: 1,
                ..ProofCodecLimits::default()
            }
        )
        .is_err()
    );

    let witness_length = u64::try_from(witness.len())?;
    for limits in [
        WitnessCodecLimits {
            max_witness_bytes: witness_length - 1,
            ..WitnessCodecLimits::default()
        },
        WitnessCodecLimits {
            max_entries: 1,
            ..WitnessCodecLimits::default()
        },
        WitnessCodecLimits {
            max_files: 1,
            ..WitnessCodecLimits::default()
        },
        WitnessCodecLimits {
            max_directories: 1,
            ..WitnessCodecLimits::default()
        },
        WitnessCodecLimits {
            max_file_bytes: 4,
            ..WitnessCodecLimits::default()
        },
        WitnessCodecLimits {
            max_total_file_bytes: 4,
            ..WitnessCodecLimits::default()
        },
        WitnessCodecLimits {
            max_decoded_bytes: 4,
            ..WitnessCodecLimits::default()
        },
        WitnessCodecLimits {
            max_path_bytes: 3,
            ..WitnessCodecLimits::default()
        },
    ] {
        assert!(decode_native_witness(&witness, &limits).is_err());
    }
    Ok(())
}

#[test]
fn ann_and_hybrid_metadata_are_required_and_bounded() -> Result<(), Box<dyn std::error::Error>> {
    let origin = temporary("metadata-origin");
    fs::create_dir(&origin)?;
    let native_anchor = anchor(8);
    let witness = bundle_native_witness(&origin, native_anchor, &WitnessCodecLimits::default())?;
    fs::remove_dir_all(origin)?;
    let reference = witness.reference()?;

    let mut ann = proof_content(NativeProofKind::Ann, native_anchor, reference);
    ann.ann
        .as_mut()
        .ok_or("missing ANN metadata")?
        .candidate_count = 101;
    assert!(NativeProof::new(ann).is_err());
    let mut mislabeled = proof_content(NativeProofKind::Ann, native_anchor, reference);
    let metadata = mislabeled.ann.as_mut().ok_or("missing ANN metadata")?;
    metadata.approximation = ApproximationLabel::Approximate;
    assert!(NativeProof::new(mislabeled).is_err());

    let mut hybrid = proof_content(NativeProofKind::Hybrid, native_anchor, reference);
    hybrid
        .hybrid
        .as_mut()
        .ok_or("missing hybrid metadata")?
        .branches[1]
        .proof_digest = [21; 32];
    assert!(NativeProof::new(hybrid).is_err());
    let mut over_candidates = proof_content(NativeProofKind::Hybrid, native_anchor, reference);
    over_candidates
        .hybrid
        .as_mut()
        .ok_or("missing hybrid metadata")?
        .branches[1]
        .candidate_limit = 81;
    assert!(NativeProof::new(over_candidates).is_err());

    let mut wrong_kind = proof_content(NativeProofKind::Sql, native_anchor, reference);
    wrong_kind.ann = proof_content(NativeProofKind::Ann, native_anchor, reference).ann;
    assert!(NativeProof::new(wrong_kind).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn witness_bundler_rejects_symbolic_links() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::symlink;

    let origin = temporary("symlink-origin");
    let outside = temporary("symlink-outside");
    fs::create_dir(&origin)?;
    fs::write(&outside, b"outside")?;
    symlink(&outside, origin.join("escape"))?;
    let error = bundle_native_witness(&origin, anchor(7), &WitnessCodecLimits::default())
        .err()
        .ok_or("symbolic link was bundled")?;
    assert!(matches!(error, NativeProofError::Invalid(_)));
    fs::remove_dir_all(origin)?;
    fs::remove_file(outside)?;
    Ok(())
}
