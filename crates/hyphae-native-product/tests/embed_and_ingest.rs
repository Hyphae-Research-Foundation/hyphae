// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used)]

//! Embedded-only catalog-bound embedding operation acceptance tests.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, CatalogName, CatalogObjectV2,
    DefinitionVersion, EmbeddingArtifactManifestDigest, EmbeddingPipelineVersion,
    EmbeddingProfileDefinition, FieldSourcePolicy, IncrementalVectorLifecycle, LexicalIndexPolicy,
    LogicalCatalogObject, NamedVectorDefinition, ObjectHeaderV2, QWEN3_EMBEDDING_QUERY_INSTRUCTION,
    QualifiedName, SearchCollectionDefinitionV2, SearchFieldDefinitionV2, SearchFieldOptions,
    VectorMetric, VectorSearchPolicy,
};
use hyphae_native_product::{
    CustomRoleGrant, NativeProduct, ProductAuthorization, ProductDocument, ProductDurability,
    ProductEmbedAndIngestBatch, ProductEmbedAndIngestBatchReceipt, ProductEmbedAndIngestDocument,
    ProductEmbedAndIngestReceipt, ProductEmbeddingBatchOutput, ProductEmbeddingExecutor,
    ProductEmbeddingExecutorRequest, ProductError, ProductErrorCode,
    ProductLocalEmbeddingExecutionProfile, ProductOperation, ProductPermission, ProductPrincipal,
    ProductRequestContext, ProductResponse, ProductScope, ProductSearchIngestBatch, ProductSession,
    ProductSessionId, ProductVector,
};
use hyphae_native_types::{EngineKind, FieldId, LogicalType, ObjectId, VectorElement, VectorType};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-embed-and-ingest-{name}-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ))
}

fn name(value: &str) -> Result<CatalogName, Box<dyn Error>> {
    Ok(CatalogName::unquoted(value)?)
}

fn header(
    id: u128,
    owner: EngineKind,
    object: &str,
    parent: Option<u128>,
) -> Result<ObjectHeaderV2, Box<dyn Error>> {
    Ok(ObjectHeaderV2 {
        id: ObjectId::new(id)?,
        owner,
        name: QualifiedName::new(name("main")?, name("public")?, name(object)?),
        parent: parent.map(ObjectId::new).transpose()?,
        definition_version: DefinitionVersion::FIRST,
    })
}

fn configured(path: &PathBuf) -> Result<NativeProduct, Box<dyn Error>> {
    let _ = fs::remove_dir_all(path);
    let mut product = NativeProduct::create(path)?;
    let profile = ObjectId::new(13)?;
    product.create_catalog_objects_v2(
        vec![
            LogicalCatalogObject::V2(CatalogObjectV2::Database(header(
                10,
                EngineKind::Kernel,
                "database",
                None,
            )?)),
            LogicalCatalogObject::V2(CatalogObjectV2::Schema(header(
                11,
                EngineKind::Kernel,
                "schema",
                Some(10),
            )?)),
            LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(AnalyzerDefinition {
                header: header(12, EngineKind::Search, "canonical", Some(11))?,
                tokenizer: AnalyzerTokenizer::UnicodeWord,
                filters: vec![AnalyzerFilter::Lowercase],
            })),
            LogicalCatalogObject::V2(CatalogObjectV2::EmbeddingProfile(
                EmbeddingProfileDefinition {
                    header: header(13, EngineKind::Search, "qwen", Some(11))?,
                    artifact_manifest_digest: EmbeddingArtifactManifestDigest::new([1; 32])?,
                    artifact_manifest_byte_length: 8_323,
                    pipeline_version: EmbeddingPipelineVersion::Qwen3EmbeddingV1,
                    vector_type: VectorType::new(VectorElement::Float32, 384)?,
                    max_input_tokens: 256,
                    query_instruction: QWEN3_EMBEDDING_QUERY_INSTRUCTION.to_owned(),
                },
            )),
            LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
                SearchCollectionDefinitionV2 {
                    header: header(14, EngineKind::Search, "documents", Some(11))?,
                    fields: vec![SearchFieldDefinitionV2 {
                        id: FieldId::new(1)?,
                        name: name("body")?,
                        logical_type: LogicalType::Text,
                        analyzer: Some(ObjectId::new(12)?),
                        options: SearchFieldOptions {
                            stored: true,
                            doc_values: false,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::Frequencies,
                        },
                    }],
                    vectors: vec![NamedVectorDefinition {
                        id: FieldId::new(2)?,
                        name: name("semantic")?,
                        vector_type: VectorType::new(VectorElement::Float32, 384)?,
                        metric: VectorMetric::Cosine,
                        policy: VectorSearchPolicy::Exact,
                        lifecycle: IncrementalVectorLifecycle {
                            delta_max_entries: 1_000,
                            consolidate_after_deltas: 4,
                            retain_generations: 2,
                        },
                        embedding_profile: Some(profile),
                    }],
                    bm25: None,
                },
            )),
        ],
        ProductDurability::Strict,
    )?;
    product.provision_search_collection(ObjectId::new(14)?, 0, ProductDurability::Strict)?;
    Ok(product)
}

fn session(id: u128) -> Result<ProductSession, Box<dyn Error>> {
    Ok(ProductSession::new(
        ProductSessionId::new(id).ok_or("zero session")?,
        ProductPrincipal::new("embed-owner").ok_or("invalid principal")?,
        ProductAuthorization::ALL,
    ))
}

fn context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    )
    .with_authorization_epoch(session.authorization_epoch())
}

fn managed_session(
    product: &mut NativeProduct,
    owner_secret: &str,
    key_path: &PathBuf,
    label: &str,
    grants: &[CustomRoleGrant],
    session_id: u128,
) -> Result<ProductSession, Box<dyn Error>> {
    let owner = product.authenticate_api_key(owner_secret, 0)?;
    let principal = product.create_security_principal(&owner, label, 10)?;
    let owner = product.authenticate_api_key(owner_secret, 0)?;
    let role = product.create_custom_security_role(&owner, label, grants.iter().copied(), 11)?;
    let owner = product.authenticate_api_key(owner_secret, 0)?;
    product.assign_custom_security_role(&owner, principal.principal_id, role.role_id, 12)?;
    let owner = product.authenticate_api_key(owner_secret, 0)?;
    product.set_security_principal_enabled(&owner, principal.principal_id, true, 13)?;
    let owner = product.authenticate_api_key(owner_secret, 0)?;
    let _ = fs::remove_file(key_path);
    product.issue_scoped_api_key_to_file(
        &owner,
        principal.principal_id,
        label,
        [],
        [role.role_id],
        ProductAuthorization::from_permissions([
            ProductPermission::CatalogRead,
            ProductPermission::DataWrite,
        ]),
        [
            ProductScope::CatalogObject(ObjectId::new(13)?),
            ProductScope::CatalogObject(ObjectId::new(14)?),
        ],
        None,
        key_path,
        14,
    )?;
    let secret = fs::read_to_string(key_path)?;
    let authority = product.authenticate_api_key(&secret, 0)?;
    Ok(ProductSession::new_authenticated(
        ProductSessionId::new(session_id).ok_or("zero managed session")?,
        authority,
    ))
}

fn batch(text: &str) -> Result<ProductSearchIngestBatch, Box<dyn Error>> {
    Ok(ProductSearchIngestBatch {
        idempotency_id: 77,
        documents: vec![ProductDocument {
            object_id: ObjectId::new(101)?,
            text: text.to_owned(),
            doc_values: BTreeMap::new(),
            vectors: BTreeMap::new(),
        }],
    })
}

fn operation(batch: ProductSearchIngestBatch) -> Result<ProductOperation, Box<dyn Error>> {
    Ok(ProductOperation::EmbedAndIngestBatch {
        collection: ObjectId::new(14)?,
        target: "semantic".to_owned(),
        batch,
    })
}

#[derive(Debug)]
struct FakeExecutor {
    calls: AtomicUsize,
    failures_remaining: AtomicUsize,
    tokens: u32,
}

impl FakeExecutor {
    fn new(failures: usize, tokens: u32) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            failures_remaining: AtomicUsize::new(failures),
            tokens,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

impl ProductEmbeddingExecutor for FakeExecutor {
    fn embed_passages(
        &self,
        request: ProductEmbeddingExecutorRequest<'_>,
        checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
    ) -> Result<ProductEmbeddingBatchOutput, ProductError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        checkpoint()?;
        if self.failures_remaining.load(Ordering::Acquire) != 0 {
            self.failures_remaining.fetch_sub(1, Ordering::AcqRel);
            return Err(ProductError::from_code(ProductErrorCode::Unavailable));
        }
        let dimension = usize::from(request.limits.output_dimension);
        let mut values = vec![0.0_f32; dimension];
        values[0] = 1.0;
        let vector = ProductVector::new(values)
            .map_err(|_| ProductError::from_code(ProductErrorCode::InvalidRequest))?;
        Ok(ProductEmbeddingBatchOutput {
            vectors: request.documents.iter().map(|_| vector.clone()).collect(),
            input_tokens: vec![self.tokens; request.documents.len()],
        })
    }

    fn execution_profile(
        &self,
        profile: &EmbeddingProfileDefinition,
    ) -> Result<Option<ProductLocalEmbeddingExecutionProfile>, ProductError> {
        ProductLocalEmbeddingExecutionProfile::new(
            "candle-qwen3",
            "0.9.2",
            "cpu",
            "float32",
            "x86_64-unknown-linux-gnu",
            "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3",
            *profile.artifact_manifest_digest.as_bytes(),
            32,
        )
        .map(Some)
    }
}

fn receipt(response: ProductResponse) -> Result<ProductEmbedAndIngestBatchReceipt, Box<dyn Error>> {
    let ProductResponse::EmbeddedAndIngested(receipt) = response else {
        return Err("wrong embedding response".into());
    };
    Ok(receipt)
}

fn wire_receipt(response: ProductResponse) -> Result<ProductEmbedAndIngestReceipt, Box<dyn Error>> {
    let ProductResponse::EmbedAndIngested(receipt) = response else {
        return Err("wrong wire embedding response".into());
    };
    Ok(receipt)
}

#[test]
fn path_free_operation_reports_commit_profile_and_exact_replay() -> Result<(), Box<dyn Error>> {
    let path = temporary("wire-replay");
    let mut product = configured(&path)?;
    let executor = Arc::new(FakeExecutor::new(0, 3));
    product.set_embedding_executor(executor.clone());
    let mut owner = session(91)?;
    let operation = || ProductOperation::EmbedAndIngest {
        collection: ObjectId::new(14).expect("nonzero collection"),
        batch: ProductEmbedAndIngestBatch {
            idempotency_id: 87,
            documents: vec![ProductEmbedAndIngestDocument {
                object_id: ObjectId::new(101).expect("nonzero object"),
                text: "bounded passage".to_owned(),
                doc_values: BTreeMap::new(),
            }],
        },
    };
    let first_context = context(&owner, 1);
    let committed = wire_receipt(product.dispatch(&mut owner, &first_context, operation())?)?;
    assert!(!committed.idempotent_replay);
    assert_eq!(committed.documents, 1);
    assert_eq!(
        committed.execution_profile.embedding_profile,
        ObjectId::new(13)?
    );
    assert_eq!(executor.calls(), 1);

    let replay_context = context(&owner, 2);
    let replay = wire_receipt(product.dispatch(&mut owner, &replay_context, operation())?)?;
    assert!(replay.idempotent_replay);
    assert_eq!(replay.commit, committed.commit);
    assert_eq!(replay.execution_profile, committed.execution_profile);
    assert_eq!(executor.calls(), 1);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn executor_crash_publishes_nothing_and_durable_replay_skips_execution()
-> Result<(), Box<dyn Error>> {
    let path = temporary("crash-replay");
    let mut product = configured(&path)?;
    let executor = Arc::new(FakeExecutor::new(1, 3));
    product.set_embedding_executor(executor.clone());
    let mut owner = session(1)?;
    let input = batch("bounded passage")?;

    let first_context = context(&owner, 1);
    let first = product
        .dispatch(&mut owner, &first_context, operation(input.clone())?)
        .expect_err("fake executor crash was accepted");
    assert_eq!(first.code(), ProductErrorCode::Unavailable);
    assert_eq!(executor.calls(), 1);
    assert!(
        NativeProduct::search_documents_at_snapshot(
            &product.snapshot_bounded(0)?,
            ObjectId::new(14)?,
            None,
            8,
        )?
        .documents
        .is_empty()
    );

    let second_context = context(&owner, 2);
    let committed =
        receipt(product.dispatch(&mut owner, &second_context, operation(input.clone())?)?)?;
    assert!(!committed.idempotent_replay);
    assert_eq!(committed.documents, 1);
    assert_eq!(committed.input_tokens, 3);
    assert_eq!(executor.calls(), 2);
    drop(product);

    let mut reopened = NativeProduct::open(&path)?;
    let replay_executor = Arc::new(FakeExecutor::new(1, 3));
    reopened.set_embedding_executor(replay_executor.clone());
    let mut reconnected = session(2)?;
    let replay_context = context(&reconnected, 3);
    let replayed =
        receipt(reopened.dispatch(&mut reconnected, &replay_context, operation(input)?)?)?;
    assert!(replayed.idempotent_replay);
    assert_eq!(replayed.commit, committed.commit);
    assert_eq!(replayed.documents, committed.documents);
    assert_eq!(replayed.input_tokens, committed.input_tokens);
    assert_eq!(replay_executor.calls(), 0);
    assert_eq!(
        NativeProduct::search_documents_at_snapshot(
            &reopened.snapshot_bounded(0)?,
            ObjectId::new(14)?,
            None,
            8,
        )?
        .documents
        .len(),
        1
    );
    drop(reopened);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn executor_output_is_incrementally_bounded_before_atomic_completion() -> Result<(), Box<dyn Error>>
{
    let path = temporary("output-bounds");
    let mut product = configured(&path)?;
    let oversized = Arc::new(FakeExecutor::new(0, 2));
    product.set_embedding_executor(oversized.clone());
    let mut owner = session(3)?;
    let input = batch("token bound")?;
    let mut bounded = context(&owner, 4);
    bounded.limits.max_work_units = 1;
    let error = product
        .dispatch(&mut owner, &bounded, operation(input.clone())?)
        .expect_err("executor exceeded the aggregate token bound");
    assert_eq!(error.code(), ProductErrorCode::LimitExceeded);
    assert_eq!(oversized.calls(), 1);

    let exact = Arc::new(FakeExecutor::new(0, 1));
    product.set_embedding_executor(exact.clone());
    let mut retry = context(&owner, 5);
    retry.limits.max_work_units = 1;
    let committed = receipt(product.dispatch(&mut owner, &retry, operation(input)?)?)?;
    assert!(!committed.idempotent_replay);
    assert_eq!(committed.input_tokens, 1);
    assert_eq!(exact.calls(), 1);
    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn collection_authority_without_profile_scope_fails_before_execution() -> Result<(), Box<dyn Error>>
{
    let path = temporary("profile-scope");
    let owner_key = path.with_extension("owner-key");
    let denied_key = path.with_extension("denied-key");
    let allowed_key = path.with_extension("allowed-key");
    let mut product = configured(&path)?;
    product.bootstrap_access_control_to_file("Owner", "owner", &owner_key, 1)?;
    let owner_secret = fs::read_to_string(&owner_key)?;
    let executor = Arc::new(FakeExecutor::new(0, 1));
    product.set_embedding_executor(executor.clone());
    let collection = ObjectId::new(14)?;
    let profile = ObjectId::new(13)?;

    let collection_only = [
        CustomRoleGrant::new(
            ProductPermission::CatalogRead,
            ProductScope::CatalogObject(collection),
        )
        .ok_or("invalid collection read grant")?,
        CustomRoleGrant::new(
            ProductPermission::DataWrite,
            ProductScope::CatalogObject(collection),
        )
        .ok_or("invalid collection write grant")?,
    ];
    let mut denied = managed_session(
        &mut product,
        &owner_secret,
        &denied_key,
        "collection-only",
        &collection_only,
        10,
    )?;
    let denied_context = context(&denied, 10);
    let error = product
        .dispatch(&mut denied, &denied_context, operation(batch("denied")?)?)
        .expect_err("collection-only authority reached the executor");
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    assert_eq!(executor.calls(), 0);

    let fully_scoped = [
        collection_only[0],
        collection_only[1],
        CustomRoleGrant::new(
            ProductPermission::CatalogRead,
            ProductScope::CatalogObject(profile),
        )
        .ok_or("invalid profile read grant")?,
    ];
    let mut allowed = managed_session(
        &mut product,
        &owner_secret,
        &allowed_key,
        "collection-and-profile",
        &fully_scoped,
        11,
    )?;
    let allowed_context = context(&allowed, 11);
    let committed = receipt(product.dispatch(
        &mut allowed,
        &allowed_context,
        operation(batch("allowed")?)?,
    )?)?;
    assert_eq!(committed.profile, profile);
    assert_eq!(executor.calls(), 1);

    drop(product);
    fs::remove_file(owner_key)?;
    fs::remove_file(denied_key)?;
    fs::remove_file(allowed_key)?;
    fs::remove_dir_all(path)?;
    Ok(())
}
