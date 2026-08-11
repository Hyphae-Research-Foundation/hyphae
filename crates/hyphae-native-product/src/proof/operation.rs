// SPDX-License-Identifier: AGPL-3.0-only

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_catalog::{CatalogObjectKind, CatalogObjectV2, LogicalCatalogObject};
use hyphae_native_runtime::{
    BoundedSearchLimits, BoundedSearchQuery, CatalogPageStop, DocValueAggregation, DocValueFilter,
    DocValueOperator, DocValueSortDirection, DocValueSortSource, MissingPlacement,
};

use crate::{
    CatalogCursor, CatalogListRequest, CatalogObjectSummary, NativeProduct, ObjectId,
    ProductAggregationValue, ProductDocValue, ProductLexicalBranch, ProductNamedAggregationValue,
    ProductOperation, ProductRequestContext, ProductResponse, ProductSearchHit,
    ProductSearchRequest, ProductSearchResults, ProductSession, ProductSqlResult, ProductValue,
    ProductVector, ProductVectorBranch, ProductVectorBranchReceipt, ProductVectorExecution,
    ProductVectorStrategy, SnapshotIdentity,
};

use super::{
    codec::{Decoder, Encoder, encode_native_proof},
    crypto::blake3_parts,
    model::{
        AdmittedProofLimits, AnnFilterStrategy, AnnProofMetadata, ApproximationLabel,
        CanonicalBytes, CompletionStatus, ExternalTrustedAnchor, HybridBranchBinding,
        HybridDuplicatePolicy, HybridFailurePolicy, HybridFusionMethod, HybridProofMetadata,
        NativeDirectoryWitness, NativeOperationProofArtifact, NativeProof, NativeProofAnchor,
        NativeProofContent, NativeProofError, NativeProofGenerationLimits, NativeProofKind,
        NativeVerificationLimits, ProofObjectBinding, VectorMetric, limit,
    },
    witness::bundle_native_witness,
};

const REQUEST_MAGIC: &[u8; 8] = b"HYOPRQ02";
const RESULT_MAGIC: &[u8; 8] = b"HYOPRS02";
const EVIDENCE_MAGIC: &[u8; 8] = b"HYOPEV02";
const SEMANTICS_VERSION: u16 = 2;
const ORDERING_VERSION: u16 = 2;
const OP_POINT_CATALOG: u8 = 1;
const OP_SQL: u8 = 2;
const OP_LEXICAL: u8 = 3;
const OP_SEARCH_COLLECTION: u8 = 4;
const OP_CATALOG_LIST: u8 = 5;
const OP_CATALOG_DESCRIBE: u8 = 6;
const MAX_OPERATION_DEPTH: usize = 32;
static NEXT_EXTRACTION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
enum SemanticOperation {
    PointCatalog {
        id: ObjectId,
    },
    Sql {
        statement: String,
        parameters: Vec<ProductValue>,
    },
    Lexical {
        index: ObjectId,
        query: BoundedSearchQuery,
        limit: usize,
        limits: BoundedSearchLimits,
    },
    SearchCollection {
        collection: ObjectId,
        request: ProductSearchRequest,
        logical_time_micros: i64,
    },
    CatalogList(CatalogListRequest),
    CatalogDescribe {
        id: ObjectId,
    },
}

struct CapturedExecution {
    operation: SemanticOperation,
    kind: NativeProofKind,
    response: ProductResponse,
    snapshot: SnapshotIdentity,
}

/// Executes one eligible product read and builds a complete semantic proof from its actual output.
///
/// # Errors
///
/// Rejects mutations, session-local prepared handles, unsupported reads, failed execution,
/// checkpoint divergence, noncanonical output, and configured proof or witness limits.
pub fn generate_native_operation_proof(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    context: &ProductRequestContext,
    operation: &ProductOperation,
    limits: NativeProofGenerationLimits,
) -> Result<(ProductResponse, NativeOperationProofArtifact), NativeProofError> {
    if operation.is_mutating_for_proof() {
        return Err(NativeProofError::Invalid(
            "proof generation requires a read-only operation",
        ));
    }
    let response = product
        .dispatch(session, context, operation.clone())
        .map_err(|_| NativeProofError::Invalid("proven operation execution failed"))?;
    let captured = capture_execution(product, context, operation, response)?;
    let checkpoint = checkpoint_authority(product, captured.snapshot)?;
    let visible_csn = captured.snapshot.visible_csn.map_or(0, crate::Csn::get);
    if checkpoint.0 != visible_csn {
        return Err(NativeProofError::Invalid(
            "proof checkpoint moved beyond the result snapshot",
        ));
    }
    let anchor = NativeProofAnchor {
        directory_lineage: captured.snapshot.directory_lineage,
        history_epoch: product.database.directory_identity().history_epoch(),
        visible_csn,
        catalog_version: captured.snapshot.catalog_version.get(),
        root_digest: captured.snapshot.root_digest,
        checkpoint_sequence: checkpoint.0,
        checkpoint_digest: checkpoint.1,
    };
    let witness = bundle_native_witness(product.data_directory(), anchor, &limits.witness)?;
    let request = CanonicalBytes::new(encode_semantic_operation(&captured.operation)?);
    let (result, evidence) = encode_claim(&captured.response, captured.snapshot)?;
    enforce_generation_limits(
        &captured.response,
        result.len(),
        evidence.len(),
        limits.admitted,
    )?;
    let objects = collect_object_bindings(product, &captured.operation, &captured.response)?;
    let ann = ann_metadata(
        product,
        &captured.operation,
        &captured.response,
        captured.snapshot,
    )?;
    let hybrid = hybrid_metadata(&captured.operation, &captured.response)?;
    let proof_content = NativeProofContent {
        kind: captured.kind,
        anchor,
        semantics_version: SEMANTICS_VERSION,
        ordering_version: ORDERING_VERSION,
        objects,
        request,
        result: CanonicalBytes::new(result),
        evidence: CanonicalBytes::new(evidence),
        limits: limits.admitted,
        completion: CompletionStatus::Complete,
        witness: witness.reference()?,
        ann,
        hybrid,
    };
    let proof = super::codec::finalize_proof(proof_content, &limits.proof)?;
    let proof_bytes = encode_native_proof(&proof, &limits.proof)?;
    let artifact = NativeOperationProofArtifact {
        trusted_anchor: ExternalTrustedAnchor::new(anchor.digest()),
        proof,
        proof_bytes,
        witness_bytes: witness.bytes,
    };
    Ok((captured.response, artifact))
}

fn checkpoint_authority(
    product: &mut NativeProduct,
    snapshot: SnapshotIdentity,
) -> Result<(u64, [u8; 32]), NativeProofError> {
    let checkpoint = match product.database.last_checkpoint_authority() {
        Some((sequence, digest)) if sequence == snapshot.visible_csn.map_or(0, crate::Csn::get) => {
            (sequence, digest)
        }
        _ => {
            let receipt = product
                .database
                .checkpoint()
                .map_err(|_| NativeProofError::Invalid("native checkpoint failed"))?;
            (receipt.visible_csn.get(), receipt.manifest_digest)
        }
    };
    let current = product
        .snapshot_bounded(snapshot.logical_time_micros)
        .map_err(|_| NativeProofError::Invalid("proof snapshot failed"))?
        .identity();
    if current.directory_lineage != snapshot.directory_lineage
        || current.visible_csn != snapshot.visible_csn
        || current.catalog_version != snapshot.catalog_version
        || current.root_digest != snapshot.root_digest
    {
        return Err(NativeProofError::Invalid(
            "proof result is not the checkpointed current root",
        ));
    }
    Ok(checkpoint)
}

pub(crate) fn dispatch_proven_operation(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    context: &ProductRequestContext,
    operation: &ProductOperation,
    limits: NativeProofGenerationLimits,
) -> Result<ProductResponse, NativeProofError> {
    if matches!(operation, ProductOperation::Prove { .. }) {
        return Err(NativeProofError::Invalid("nested proof generation"));
    }
    let (response, artifact) =
        generate_native_operation_proof(product, session, context, operation, limits)?;
    Ok(ProductResponse::Proven {
        response: Box::new(response),
        artifact: Box::new(artifact),
    })
}

/// Reopens the retained native authority and reexecutes a recognized operation proof.
///
/// `Ok(false)` means the artifacts are integrity-only and carry no v2 operation contract.
/// `Ok(true)` is returned only after exact request, result, evidence, object-binding, root, and
/// checkpoint comparison succeeds.
///
/// # Errors
///
/// Rejects recognized semantic artifacts whose authority cannot be extracted/opened or whose
/// reexecuted ordered result/evidence differs from the proof.
pub fn reexecute_native_operation_proof(
    proof: &NativeProof,
    witness: &NativeDirectoryWitness,
    limits: &NativeVerificationLimits,
) -> Result<bool, NativeProofError> {
    if !proof.content.request.as_bytes().starts_with(REQUEST_MAGIC) {
        return Ok(false);
    }
    enforce_reexecution_limits(proof, limits)?;
    let operation = decode_semantic_operation(proof.content.request.as_bytes(), limits)?;
    if encode_semantic_operation(&operation)? != proof.content.request.bytes {
        return Err(NativeProofError::Invalid(
            "noncanonical semantic operation encoding",
        ));
    }
    let extraction = ExtractedWitness::create(witness)?;
    let product = NativeProduct::open(extraction.path())
        .map_err(|_| NativeProofError::Invalid("retained native authority failed to open"))?;
    let identity = product
        .snapshot_bounded(operation.logical_time_micros())
        .map_err(|_| NativeProofError::Invalid("retained native snapshot failed"))?
        .identity();
    require_anchor_identity(proof.content.anchor, identity)?;
    if product.database.last_checkpoint_authority()
        != Some((
            proof.content.anchor.checkpoint_sequence,
            proof.content.anchor.checkpoint_digest,
        ))
    {
        return Err(NativeProofError::Invalid(
            "retained checkpoint authority does not match proof",
        ));
    }
    let (response, snapshot) = reexecute(&product, &operation)?;
    let (result, evidence) = encode_claim(&response, snapshot)?;
    if result != proof.content.result.bytes {
        return Err(NativeProofError::DigestMismatch("semantic result"));
    }
    if evidence != proof.content.evidence.bytes {
        return Err(NativeProofError::DigestMismatch("semantic evidence"));
    }
    let objects = collect_object_bindings(&product, &operation, &response)?;
    if objects != proof.content.objects {
        return Err(NativeProofError::DigestMismatch("semantic object bindings"));
    }
    verify_declared_metadata(proof, &product, &operation, &response, snapshot, &evidence)?;
    drop(product);
    Ok(true)
}

impl SemanticOperation {
    const fn logical_time_micros(&self) -> i64 {
        match self {
            Self::SearchCollection {
                logical_time_micros,
                ..
            } => *logical_time_micros,
            _ => 0,
        }
    }
}

trait ProofOperationClass {
    fn is_mutating_for_proof(&self) -> bool;
}

impl ProofOperationClass for ProductOperation {
    fn is_mutating_for_proof(&self) -> bool {
        match self {
            ProductOperation::CatalogCreate { .. }
            | ProductOperation::StructureSet { .. }
            | ProductOperation::StructureMutate { .. }
            | ProductOperation::SearchIngest { .. }
            | ProductOperation::SearchDocumentUpdate { .. }
            | ProductOperation::SearchDocumentDelete { .. }
            | ProductOperation::AdminCheckpoint
            | ProductOperation::Backup(_)
            | ProductOperation::Restore(_)
            | ProductOperation::Prove { .. } => true,
            ProductOperation::ExecuteSql { statement, .. } => {
                let first = statement
                    .trim_start()
                    .split(|character: char| character.is_ascii_whitespace() || character == '(')
                    .next()
                    .unwrap_or_default();
                !first.eq_ignore_ascii_case("select")
                    && !first.eq_ignore_ascii_case("with")
                    && !first.eq_ignore_ascii_case("explain")
            }
            _ => false,
        }
    }
}

fn capture_execution(
    product: &NativeProduct,
    context: &ProductRequestContext,
    operation: &ProductOperation,
    response: ProductResponse,
) -> Result<CapturedExecution, NativeProofError> {
    let (operation, kind, snapshot) = match (operation, &response) {
        (ProductOperation::CatalogObject { id }, ProductResponse::CatalogObject(read)) => (
            SemanticOperation::PointCatalog { id: *id },
            NativeProofKind::Point,
            read.snapshot,
        ),
        (
            ProductOperation::ExecuteSql {
                statement,
                parameters,
            },
            ProductResponse::Sql {
                snapshot: Some(snapshot),
                commit: None,
                ..
            },
        ) => (
            SemanticOperation::Sql {
                statement: statement.clone(),
                parameters: parameters.clone(),
            },
            NativeProofKind::Sql,
            *snapshot,
        ),
        (
            ProductOperation::Search {
                index,
                query,
                limit,
            },
            ProductResponse::Search(_),
        ) => {
            let snapshot = latest_snapshot_identity(product, context.logical_time_micros)?;
            (
                SemanticOperation::Lexical {
                    index: *index,
                    query: query.clone(),
                    limit: *limit,
                    limits: search_limits(context, *limit),
                },
                NativeProofKind::Lexical,
                snapshot,
            )
        }
        (
            ProductOperation::SearchCollection {
                collection,
                request,
            },
            ProductResponse::IntegratedSearch(result),
        ) => (
            SemanticOperation::SearchCollection {
                collection: *collection,
                request: resolve_proof_search_request(product, *collection, request)?,
                logical_time_micros: context.logical_time_micros,
            },
            integrated_kind(request, result)?,
            result.snapshot,
        ),
        (ProductOperation::CatalogList(request), ProductResponse::CatalogPage(page)) => (
            SemanticOperation::CatalogList(*request),
            NativeProofKind::Catalog,
            page.snapshot,
        ),
        (ProductOperation::CatalogDescribe { id }, ProductResponse::CatalogDefinition(_)) => {
            let snapshot = latest_snapshot_identity(product, 0)?;
            (
                SemanticOperation::CatalogDescribe { id: *id },
                NativeProofKind::Catalog,
                snapshot,
            )
        }
        _ => {
            return Err(NativeProofError::Invalid(
                "operation is not eligible for semantic proof generation",
            ));
        }
    };
    Ok(CapturedExecution {
        operation,
        kind,
        response,
        snapshot,
    })
}

fn resolve_proof_search_request(
    product: &NativeProduct,
    collection: ObjectId,
    request: &ProductSearchRequest,
) -> Result<ProductSearchRequest, NativeProofError> {
    let definition = search_collection_definition(product, collection)?;
    let mut resolved = request.clone();
    for branch in &mut resolved.vectors {
        if branch.execution.is_some() {
            continue;
        }
        let vector = definition
            .vectors
            .iter()
            .find(|vector| vector.name.lookup() == branch.target)
            .ok_or(NativeProofError::Invalid(
                "search vector definition is missing",
            ))?;
        branch.execution = Some(match vector.policy {
            hyphae_native_catalog::VectorSearchPolicy::Exact => ProductVectorExecution::Exact,
            hyphae_native_catalog::VectorSearchPolicy::Ann(ann) => ProductVectorExecution::Ann {
                ef_search: usize::from(ann.ef_search_default()),
                exact_rerank: None,
            },
            hyphae_native_catalog::VectorSearchPolicy::Adaptive {
                exact_candidate_threshold,
                ann,
            } => ProductVectorExecution::Adaptive {
                exact_candidate_threshold: usize::try_from(exact_candidate_threshold)
                    .map_err(|_| NativeProofError::LengthOverflow)?,
                ef_search: usize::from(ann.ef_search_default()),
                exact_rerank: None,
            },
        });
    }
    Ok(resolved)
}

fn latest_snapshot_identity(
    product: &NativeProduct,
    logical_time_micros: i64,
) -> Result<SnapshotIdentity, NativeProofError> {
    let catalog = product
        .catalog_snapshot()
        .map_err(|_| NativeProofError::Invalid("proof snapshot identity failed"))?
        .identity();
    Ok(SnapshotIdentity {
        logical_time_micros,
        ..catalog
    })
}

fn reexecute(
    product: &NativeProduct,
    operation: &SemanticOperation,
) -> Result<(ProductResponse, SnapshotIdentity), NativeProofError> {
    match operation {
        SemanticOperation::PointCatalog { id } => {
            let read = product
                .catalog_object(*id)
                .map_err(|_| NativeProofError::Invalid("point proof reexecution failed"))?;
            let snapshot = read.snapshot;
            Ok((ProductResponse::CatalogObject(read), snapshot))
        }
        SemanticOperation::Sql {
            statement,
            parameters,
        } => {
            let prepared = product
                .prepare_sql(statement)
                .map_err(|_| NativeProofError::Invalid("SQL proof prepare failed"))?;
            let read = product
                .execute_prepared(&prepared, parameters)
                .map_err(|_| NativeProofError::Invalid("SQL proof reexecution failed"))?;
            Ok((
                ProductResponse::Sql {
                    result: read.value,
                    snapshot: Some(read.snapshot),
                    commit: None,
                },
                read.snapshot,
            ))
        }
        SemanticOperation::Lexical {
            index,
            query,
            limit,
            limits,
        } => {
            let snapshot = product
                .snapshot_bounded(0)
                .map_err(|_| NativeProofError::Invalid("lexical proof snapshot failed"))?;
            let result = snapshot
                .inner
                .search_bounded(*index, query, *limit, *limits)
                .map_err(|_| NativeProofError::Invalid("lexical proof reexecution failed"))?;
            let identity = snapshot.identity();
            Ok((
                ProductResponse::Search(ProductSearchResults {
                    hits: result
                        .hits
                        .into_iter()
                        .map(|hit| ProductSearchHit {
                            document_id: hit.document_id,
                            score: crate::CanonicalF64::new(hit.score),
                        })
                        .collect(),
                    documents_examined: result.documents_examined,
                    source_bytes: result.source_bytes,
                    token_visits: result.token_visits,
                    token_comparisons: result.token_comparisons,
                    fuzzy_steps: result.fuzzy_steps,
                }),
                identity,
            ))
        }
        SemanticOperation::SearchCollection {
            collection,
            request,
            logical_time_micros,
        } => {
            let result = product
                .search_collection(*collection, request, *logical_time_micros)
                .map_err(|_| NativeProofError::Invalid("search proof reexecution failed"))?;
            let snapshot = result.snapshot;
            Ok((ProductResponse::IntegratedSearch(result), snapshot))
        }
        SemanticOperation::CatalogList(request) => {
            let snapshot = product
                .catalog_snapshot()
                .map_err(|_| NativeProofError::Invalid("catalog proof snapshot failed"))?;
            let page = product
                .catalog_list(&snapshot, *request)
                .map_err(|_| NativeProofError::Invalid("catalog list reexecution failed"))?;
            let identity = page.snapshot;
            Ok((ProductResponse::CatalogPage(page), identity))
        }
        SemanticOperation::CatalogDescribe { id } => {
            let snapshot = product
                .catalog_snapshot()
                .map_err(|_| NativeProofError::Invalid("catalog proof snapshot failed"))?;
            let value = product
                .catalog_describe(&snapshot, *id)
                .map_err(|_| NativeProofError::Invalid("catalog describe reexecution failed"))?;
            Ok((
                ProductResponse::CatalogDefinition(value),
                snapshot.identity(),
            ))
        }
    }
}

fn integrated_kind(
    request: &ProductSearchRequest,
    result: &crate::ProductSearchResult,
) -> Result<NativeProofKind, NativeProofError> {
    let branch_count = usize::from(request.lexical.is_some()).saturating_add(request.vectors.len());
    if branch_count > 1 {
        return Ok(NativeProofKind::Hybrid);
    }
    if request.lexical.is_some() {
        return Ok(NativeProofKind::Lexical);
    }
    let Some(receipt) = result.vector_branches.first() else {
        return Err(NativeProofError::Invalid(
            "search proof requires a lexical or vector branch",
        ));
    };
    Ok(match receipt.strategy {
        ProductVectorStrategy::ExactFiltered | ProductVectorStrategy::AdaptiveExactFiltered => {
            NativeProofKind::ExactVector
        }
        ProductVectorStrategy::FilterAwareAnn | ProductVectorStrategy::AdaptiveFilterAwareAnn => {
            NativeProofKind::Ann
        }
    })
}

fn search_limits(context: &ProductRequestContext, requested_hits: usize) -> BoundedSearchLimits {
    BoundedSearchLimits {
        max_hits: context.limits.max_count,
        max_documents: context.limits.max_work_units,
        max_matches: context.limits.max_count,
        max_source_bytes: context.limits.max_memory_bytes,
        max_token_visits: context.limits.max_work_units,
        max_token_comparisons: context.limits.max_work_units,
        max_fuzzy_steps: context.limits.max_work_units,
        max_clauses: context.limits.max_count,
        max_query_bytes: context.limits.max_request_bytes,
    }
    .with_requested_hits(requested_hits)
}

trait RequestedHits {
    fn with_requested_hits(self, requested: usize) -> Self;
}

impl RequestedHits for BoundedSearchLimits {
    fn with_requested_hits(mut self, requested: usize) -> Self {
        self.max_hits = self.max_hits.max(requested);
        self
    }
}

fn require_anchor_identity(
    anchor: NativeProofAnchor,
    identity: SnapshotIdentity,
) -> Result<(), NativeProofError> {
    if identity.directory_lineage != anchor.directory_lineage
        || identity.visible_csn.map_or(0, crate::Csn::get) != anchor.visible_csn
        || identity.catalog_version.get() != anchor.catalog_version
        || identity.root_digest != anchor.root_digest
    {
        return Err(NativeProofError::WitnessAnchorMismatch);
    }
    Ok(())
}

fn enforce_generation_limits(
    response: &ProductResponse,
    result_bytes: usize,
    evidence_bytes: usize,
    limits: AdmittedProofLimits,
) -> Result<(), NativeProofError> {
    let items = response_items(response)?;
    if items > limits.result_items {
        return Err(limit("proof result items", items, limits.result_items));
    }
    let evidence = u64::try_from(evidence_bytes).map_err(|_| NativeProofError::LengthOverflow)?;
    if evidence > limits.evidence_bytes {
        return Err(limit(
            "proof evidence bytes",
            evidence,
            limits.evidence_bytes,
        ));
    }
    let _ = u64::try_from(result_bytes).map_err(|_| NativeProofError::LengthOverflow)?;
    Ok(())
}

fn enforce_reexecution_limits(
    proof: &NativeProof,
    limits: &NativeVerificationLimits,
) -> Result<(), NativeProofError> {
    if proof.content.limits.result_items
        > u64::try_from(limits.max_reexecution_result_items).unwrap_or(u64::MAX)
    {
        return Err(limit(
            "semantic result items",
            proof.content.limits.result_items,
            limits.max_reexecution_result_items,
        ));
    }
    if proof.content.limits.candidate_items
        > u64::try_from(limits.max_reexecution_candidate_items).unwrap_or(u64::MAX)
    {
        return Err(limit(
            "semantic candidate items",
            proof.content.limits.candidate_items,
            limits.max_reexecution_candidate_items,
        ));
    }
    let mut bytes = 0_u64;
    for section in [
        &proof.content.request,
        &proof.content.result,
        &proof.content.evidence,
    ] {
        bytes = bytes
            .checked_add(
                u64::try_from(section.bytes.len()).map_err(|_| NativeProofError::LengthOverflow)?,
            )
            .ok_or(NativeProofError::LengthOverflow)?;
    }
    if bytes > limits.max_reexecution_bytes {
        return Err(limit(
            "semantic reexecution bytes",
            bytes,
            limits.max_reexecution_bytes,
        ));
    }
    Ok(())
}

fn response_items(response: &ProductResponse) -> Result<u64, NativeProofError> {
    let value = match response {
        ProductResponse::CatalogObject(_) | ProductResponse::CatalogDefinition(Some(_)) => 1,
        ProductResponse::CatalogDefinition(None) => 0,
        ProductResponse::CatalogPage(page) => page.items.len(),
        ProductResponse::Sql {
            result: ProductSqlResult::Rows { rows, .. },
            ..
        } => rows.len(),
        ProductResponse::Search(result) => result.hits.len(),
        ProductResponse::IntegratedSearch(result) => result.hits.len(),
        _ => {
            return Err(NativeProofError::Invalid(
                "response is not a semantic proof result",
            ));
        }
    };
    u64::try_from(value).map_err(|_| NativeProofError::LengthOverflow)
}

fn encode_semantic_operation(operation: &SemanticOperation) -> Result<Vec<u8>, NativeProofError> {
    let mut encoded = Encoder::default();
    encoded.extend(REQUEST_MAGIC);
    encoded.u16(SEMANTICS_VERSION);
    encoded.u16(ORDERING_VERSION);
    match operation {
        SemanticOperation::PointCatalog { id } => {
            encoded.byte(OP_POINT_CATALOG);
            encoded.u128(id.get());
        }
        SemanticOperation::Sql {
            statement,
            parameters,
        } => {
            encoded.byte(OP_SQL);
            put_text(&mut encoded, statement)?;
            put_count(&mut encoded, parameters.len())?;
            for value in parameters {
                encode_value(&mut encoded, value, 0)?;
            }
        }
        SemanticOperation::Lexical {
            index,
            query,
            limit,
            limits,
        } => {
            encoded.byte(OP_LEXICAL);
            encoded.u128(index.get());
            put_usize(&mut encoded, *limit)?;
            encode_search_limits(&mut encoded, *limits)?;
            encode_query(&mut encoded, query, 0)?;
        }
        SemanticOperation::SearchCollection {
            collection,
            request,
            logical_time_micros,
        } => {
            encoded.byte(OP_SEARCH_COLLECTION);
            encoded.extend(&logical_time_micros.to_le_bytes());
            encoded.u128(collection.get());
            encode_integrated_request(&mut encoded, request)?;
        }
        SemanticOperation::CatalogList(request) => {
            encoded.byte(OP_CATALOG_LIST);
            encode_catalog_list_request(&mut encoded, *request)?;
        }
        SemanticOperation::CatalogDescribe { id } => {
            encoded.byte(OP_CATALOG_DESCRIBE);
            encoded.u128(id.get());
        }
    }
    Ok(encoded.bytes)
}

fn decode_semantic_operation(
    encoded: &[u8],
    limits: &NativeVerificationLimits,
) -> Result<SemanticOperation, NativeProofError> {
    let mut decoder = Decoder::new(encoded);
    if decoder.take(8)? != REQUEST_MAGIC
        || decoder.u16()? != SEMANTICS_VERSION
        || decoder.u16()? != ORDERING_VERSION
    {
        return Err(NativeProofError::Invalid(
            "unsupported semantic operation contract",
        ));
    }
    let operation = match decoder.byte()? {
        OP_POINT_CATALOG => SemanticOperation::PointCatalog {
            id: object_id(decoder.u128()?)?,
        },
        OP_SQL => {
            let statement = text(&mut decoder, limits.max_reexecution_bytes)?;
            let count = bounded_count(
                &mut decoder,
                limits.max_reexecution_candidate_items,
                "SQL parameters",
            )?;
            let mut parameters = Vec::new();
            parameters
                .try_reserve_exact(count)
                .map_err(|_| NativeProofError::LengthOverflow)?;
            for _ in 0..count {
                parameters.push(decode_value(&mut decoder, 0, limits)?);
            }
            SemanticOperation::Sql {
                statement,
                parameters,
            }
        }
        OP_LEXICAL => SemanticOperation::Lexical {
            index: object_id(decoder.u128()?)?,
            limit: usize_value(&mut decoder)?,
            limits: decode_search_limits(&mut decoder)?,
            query: decode_query(&mut decoder, 0, limits)?,
        },
        OP_SEARCH_COLLECTION => SemanticOperation::SearchCollection {
            logical_time_micros: i64::from_le_bytes(decoder.array()?),
            collection: object_id(decoder.u128()?)?,
            request: decode_integrated_request(&mut decoder, limits)?,
        },
        OP_CATALOG_LIST => {
            SemanticOperation::CatalogList(decode_catalog_list_request(&mut decoder)?)
        }
        OP_CATALOG_DESCRIBE => SemanticOperation::CatalogDescribe {
            id: object_id(decoder.u128()?)?,
        },
        _ => return Err(NativeProofError::Invalid("invalid semantic operation tag")),
    };
    decoder.finish()?;
    Ok(operation)
}

fn encode_claim(
    response: &ProductResponse,
    snapshot: SnapshotIdentity,
) -> Result<(Vec<u8>, Vec<u8>), NativeProofError> {
    let mut result = Encoder::default();
    let mut evidence = Encoder::default();
    result.extend(RESULT_MAGIC);
    evidence.extend(EVIDENCE_MAGIC);
    match response {
        ProductResponse::CatalogObject(read) => {
            result.byte(OP_POINT_CATALOG);
            put_bytes(
                &mut result,
                &read
                    .value
                    .encode_definition()
                    .map_err(|_| NativeProofError::Invalid("catalog definition encoding failed"))?,
            )?;
            evidence.byte(OP_POINT_CATALOG);
            encode_snapshot(&mut evidence, read.snapshot);
        }
        ProductResponse::Sql {
            result: sql,
            snapshot: Some(actual),
            commit: None,
        } => {
            result.byte(OP_SQL);
            encode_sql_result(&mut result, sql)?;
            evidence.byte(OP_SQL);
            encode_snapshot(&mut evidence, *actual);
        }
        ProductResponse::Search(search) => {
            result.byte(OP_LEXICAL);
            put_count(&mut result, search.hits.len())?;
            for hit in &search.hits {
                put_bytes(&mut result, &hit.document_id)?;
                result.u64(hit.score.bits());
            }
            evidence.byte(OP_LEXICAL);
            encode_snapshot(&mut evidence, snapshot);
            for count in [
                search.documents_examined,
                search.source_bytes,
                search.token_visits,
                search.token_comparisons,
                search.fuzzy_steps,
            ] {
                put_usize(&mut evidence, count)?;
            }
        }
        ProductResponse::IntegratedSearch(search) => {
            result.byte(OP_SEARCH_COLLECTION);
            encode_integrated_result(&mut result, search)?;
            evidence.byte(OP_SEARCH_COLLECTION);
            encode_snapshot(&mut evidence, search.snapshot);
            put_count(&mut evidence, search.vector_branches.len())?;
            for branch in &search.vector_branches {
                encode_vector_receipt(&mut evidence, branch)?;
            }
            evidence.byte(u8::from(search.approximate));
            for count in [
                search.total_documents,
                search.eligible_documents,
                search.lexical_candidates,
                search.retrieval_candidates,
                search.matched_candidates,
            ] {
                put_usize(&mut evidence, count)?;
            }
        }
        ProductResponse::CatalogPage(page) => {
            result.byte(OP_CATALOG_LIST);
            put_count(&mut result, page.items.len())?;
            for item in &page.items {
                encode_catalog_summary(&mut result, item)?;
            }
            encode_optional_cursor(&mut result, page.cursor);
            evidence.byte(OP_CATALOG_LIST);
            encode_snapshot(&mut evidence, page.snapshot);
            evidence.byte(catalog_stop_tag(page.stop));
            put_usize(&mut evidence, page.visited)?;
            put_usize(&mut evidence, page.returned_bytes)?;
        }
        ProductResponse::CatalogDefinition(value) => {
            result.byte(OP_CATALOG_DESCRIBE);
            result.byte(u8::from(value.is_some()));
            if let Some(object) = value {
                put_bytes(
                    &mut result,
                    &object.encode_definition_v2().map_err(|_| {
                        NativeProofError::Invalid("logical catalog definition encoding failed")
                    })?,
                )?;
            }
            evidence.byte(OP_CATALOG_DESCRIBE);
            encode_snapshot(&mut evidence, snapshot);
        }
        _ => {
            return Err(NativeProofError::Invalid(
                "response is not encodable as a semantic proof claim",
            ));
        }
    }
    Ok((result.bytes, evidence.bytes))
}

fn collect_object_bindings(
    product: &NativeProduct,
    operation: &SemanticOperation,
    response: &ProductResponse,
) -> Result<Vec<ProofObjectBinding>, NativeProofError> {
    let mut definitions = BTreeMap::<ObjectId, Vec<u8>>::new();
    match (operation, response) {
        (SemanticOperation::PointCatalog { id }, ProductResponse::CatalogObject(read)) => {
            definitions.insert(
                *id,
                read.value.encode_definition().map_err(|_| {
                    NativeProofError::Invalid("catalog object binding encoding failed")
                })?,
            );
        }
        (
            SemanticOperation::SearchCollection { collection, .. },
            ProductResponse::IntegratedSearch(_),
        ) => {
            let snapshot = product
                .catalog_snapshot()
                .map_err(|_| NativeProofError::Invalid("catalog binding snapshot failed"))?;
            let binding = product
                .resolve_search_collection_binding(*collection, 0)
                .map_err(|_| NativeProofError::Invalid("physical search binding read failed"))?;
            for id in std::iter::once(*collection)
                .chain(std::iter::once(binding.lexical_index))
                .chain(binding.vectors.iter().map(|vector| vector.index))
            {
                if let Some(logical) = product
                    .catalog_describe(&snapshot, id)
                    .map_err(|_| NativeProofError::Invalid("catalog binding read failed"))?
                {
                    definitions.insert(id, encode_logical_binding(&logical)?);
                } else if let Ok(read) = product.catalog_object(id) {
                    definitions.insert(
                        id,
                        read.value.encode_definition().map_err(|_| {
                            NativeProofError::Invalid("catalog binding encoding failed")
                        })?,
                    );
                }
            }
        }
        (SemanticOperation::Lexical { index, .. }, ProductResponse::Search(_)) => {
            if let Ok(read) = product.catalog_object(*index) {
                definitions.insert(
                    *index,
                    read.value.encode_definition().map_err(|_| {
                        NativeProofError::Invalid("catalog binding encoding failed")
                    })?,
                );
            }
        }
        (SemanticOperation::CatalogList(_), ProductResponse::CatalogPage(page)) => {
            let snapshot = product
                .catalog_snapshot()
                .map_err(|_| NativeProofError::Invalid("catalog binding snapshot failed"))?;
            for item in &page.items {
                if let Some(object) = product
                    .catalog_describe(&snapshot, item.id)
                    .map_err(|_| NativeProofError::Invalid("catalog binding read failed"))?
                {
                    definitions.insert(item.id, encode_logical_binding(&object)?);
                } else if let Ok(read) = product.catalog_object(item.id) {
                    definitions.insert(
                        item.id,
                        read.value.encode_definition().map_err(|_| {
                            NativeProofError::Invalid("catalog binding encoding failed")
                        })?,
                    );
                }
            }
        }
        (
            SemanticOperation::CatalogDescribe { id },
            ProductResponse::CatalogDefinition(Some(object)),
        ) => {
            definitions.insert(*id, encode_logical_binding(object)?);
        }
        _ => {}
    }
    Ok(definitions
        .into_iter()
        .map(|(id, definition)| ProofObjectBinding {
            object_id: id.get(),
            definition_digest: blake3_parts(&[
                b"hyphae-native-proof-object-definition-v2",
                &definition,
            ]),
        })
        .collect())
}

fn encode_logical_binding(object: &LogicalCatalogObject) -> Result<Vec<u8>, NativeProofError> {
    match object {
        LogicalCatalogObject::Compatible(definition)
            if definition.parent == definition.object.header().id =>
        {
            definition
                .object
                .encode_definition()
                .map_err(|_| NativeProofError::Invalid("catalog binding encoding failed"))
        }
        _ => object
            .encode_definition_v2()
            .map_err(|_| NativeProofError::Invalid("catalog binding encoding failed")),
    }
}

#[allow(clippy::too_many_lines)]
fn ann_metadata(
    product: &NativeProduct,
    operation: &SemanticOperation,
    response: &ProductResponse,
    snapshot: SnapshotIdentity,
) -> Result<Option<AnnProofMetadata>, NativeProofError> {
    let (
        SemanticOperation::SearchCollection {
            collection,
            request,
            ..
        },
        ProductResponse::IntegratedSearch(result),
    ) = (operation, response)
    else {
        return Ok(None);
    };
    if integrated_kind(request, result)? != NativeProofKind::Ann {
        return Ok(None);
    }
    let branch = request.vectors.first().ok_or(NativeProofError::Invalid(
        "ANN request has no vector branch",
    ))?;
    let receipt = result
        .vector_branches
        .first()
        .ok_or(NativeProofError::Invalid(
            "ANN result has no vector receipt",
        ))?;
    let definition = search_collection_definition(product, *collection)?;
    let execution = branch.execution.ok_or(NativeProofError::Invalid(
        "search proof requires resolved vector execution",
    ))?;
    let (search_breadth, requested_rerank) = match execution {
        ProductVectorExecution::Ann {
            ef_search,
            exact_rerank,
        }
        | ProductVectorExecution::Adaptive {
            ef_search,
            exact_rerank,
            ..
        } => (ef_search, exact_rerank.unwrap_or(0)),
        ProductVectorExecution::Exact => {
            return Err(NativeProofError::Invalid(
                "ANN receipt contradicts exact request",
            ));
        }
    };
    let (metric, definition_bytes, physical_index) =
        ann_binding(product, *collection, &branch.target, definition)?;
    let exact_rerank = match execution {
        ProductVectorExecution::Ann { exact_rerank, .. }
        | ProductVectorExecution::Adaptive { exact_rerank, .. } => exact_rerank,
        ProductVectorExecution::Exact => None,
    };
    let native = execute_ann_evidence(
        product,
        operation.logical_time_micros(),
        physical_index,
        branch,
        search_breadth,
        exact_rerank,
    )?;
    let candidate_count = u64_value(native.candidate_count)?;
    let visited_count = u64_value(native.visited_nodes)?;
    let rerank_count = u64_value(requested_rerank.min(native.candidate_count))?;
    let mut eligibility = Encoder::default();
    encode_filter(&mut eligibility, &request.filter, 0)?;
    put_usize(&mut eligibility, receipt.eligible_documents)?;
    Ok(Some(AnnProofMetadata {
        metric: match metric {
            hyphae_native_catalog::VectorMetric::Cosine => VectorMetric::Cosine,
            hyphae_native_catalog::VectorMetric::NegativeDot => VectorMetric::NegativeDot,
            hyphae_native_catalog::VectorMetric::SquaredL2 => VectorMetric::SquaredL2,
        },
        index_definition_digest: blake3_parts(&[
            b"hyphae-native-ann-index-definition-v2",
            &definition_bytes,
            &physical_index.get().to_le_bytes(),
        ]),
        graph_generation_digest: native.build_identity,
        search_breadth: u32::try_from(search_breadth)
            .map_err(|_| NativeProofError::LengthOverflow)?,
        filter_strategy: AnnFilterStrategy::ExactSeededPostFilter,
        eligible_set_digest: blake3_parts(&[
            b"hyphae-native-ann-eligibility-evidence-v2",
            &snapshot.root_digest,
            &eligibility.bytes,
        ]),
        visited_count,
        candidate_count,
        rerank_count,
        approximation: ApproximationLabel::Approximate,
        exact_oracle_digest: None,
    }))
}

fn execute_ann_evidence(
    product: &NativeProduct,
    logical_time_micros: i64,
    index: ObjectId,
    branch: &crate::ProductVectorBranch,
    search_breadth: usize,
    exact_rerank: Option<usize>,
) -> Result<hyphae_native_runtime::AnnSearchReceipt, NativeProofError> {
    let native_snapshot = product
        .snapshot_bounded(logical_time_micros)
        .map_err(|_| NativeProofError::Invalid("ANN metadata snapshot failed"))?;
    native_snapshot
        .inner
        .search_ann(
            index,
            &branch.query,
            hyphae_native_runtime::AnnSearchOptions::new(
                branch.candidate_limit,
                search_breadth,
                exact_rerank,
            )
            .map_err(|_| NativeProofError::Invalid("invalid ANN proof options"))?,
        )
        .map_err(|_| NativeProofError::Invalid("ANN metadata execution failed"))
}

fn ann_binding(
    product: &NativeProduct,
    collection: ObjectId,
    target: &str,
    definition: hyphae_native_catalog::SearchCollectionDefinitionV2,
) -> Result<(hyphae_native_catalog::VectorMetric, Vec<u8>, ObjectId), NativeProofError> {
    let binding = product
        .resolve_search_collection_binding(collection, 0)
        .map_err(|_| NativeProofError::Invalid("ANN physical binding is missing"))?;
    let metric = definition
        .vectors
        .iter()
        .find(|vector| vector.name.lookup() == target)
        .ok_or(NativeProofError::Invalid(
            "ANN target definition is missing",
        ))?
        .metric;
    let definition_bytes = LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(definition))
        .encode_definition_v2()
        .map_err(|_| NativeProofError::Invalid("ANN definition encoding failed"))?;
    let index = binding
        .vectors
        .iter()
        .find(|candidate| candidate.name == target)
        .ok_or(NativeProofError::Invalid("ANN physical binding is missing"))?
        .index;
    Ok((metric, definition_bytes, index))
}

fn hybrid_metadata(
    operation: &SemanticOperation,
    response: &ProductResponse,
) -> Result<Option<HybridProofMetadata>, NativeProofError> {
    let (
        SemanticOperation::SearchCollection { request, .. },
        ProductResponse::IntegratedSearch(result),
    ) = (operation, response)
    else {
        return Ok(None);
    };
    if integrated_kind(request, result)? != NativeProofKind::Hybrid {
        return Ok(None);
    }
    let mut branches = Vec::new();
    if let Some(lexical) = &request.lexical {
        branches.push(hybrid_branch(
            0,
            lexical.query.as_bytes(),
            lexical.weight,
            lexical.candidate_limit,
        )?);
    }
    for (index, vector) in request.vectors.iter().enumerate() {
        let mut branch = Encoder::default();
        put_text(&mut branch, &vector.target)?;
        encode_vector(&mut branch, &vector.query)?;
        encode_vector_execution(
            &mut branch,
            vector.execution.ok_or(NativeProofError::Invalid(
                "search proof requires resolved vector execution",
            ))?,
        )?;
        branches.push(hybrid_branch(
            index.saturating_add(1),
            &branch.bytes,
            vector.weight,
            vector.candidate_limit,
        )?);
    }
    Ok(Some(HybridProofMetadata {
        branches,
        failure_policy: HybridFailurePolicy::FailClosed,
        fusion_method: HybridFusionMethod::WeightedReciprocalRank,
        duplicate_policy: HybridDuplicatePolicy::MergeByObjectId,
    }))
}

fn hybrid_branch(
    ordinal: usize,
    bytes: &[u8],
    weight: u32,
    candidate_limit: usize,
) -> Result<HybridBranchBinding, NativeProofError> {
    Ok(HybridBranchBinding {
        proof_digest: blake3_parts(&[
            b"hyphae-native-hybrid-branch-v2",
            &u64_value(ordinal)?.to_le_bytes(),
            bytes,
        ]),
        weight_millionths: weight,
        candidate_limit: u32::try_from(candidate_limit)
            .map_err(|_| NativeProofError::LengthOverflow)?,
    })
}

fn verify_declared_metadata(
    proof: &NativeProof,
    product: &NativeProduct,
    operation: &SemanticOperation,
    response: &ProductResponse,
    snapshot: SnapshotIdentity,
    _evidence: &[u8],
) -> Result<(), NativeProofError> {
    let ann = ann_metadata(product, operation, response, snapshot)?;
    let hybrid = hybrid_metadata(operation, response)?;
    if ann != proof.content.ann {
        return Err(NativeProofError::DigestMismatch(
            "declared ANN algorithm evidence",
        ));
    }
    if hybrid != proof.content.hybrid {
        return Err(NativeProofError::DigestMismatch("hybrid branch evidence"));
    }
    Ok(())
}

fn search_collection_definition(
    product: &NativeProduct,
    id: ObjectId,
) -> Result<hyphae_native_catalog::SearchCollectionDefinitionV2, NativeProofError> {
    let snapshot = product
        .catalog_snapshot()
        .map_err(|_| NativeProofError::Invalid("ANN catalog snapshot failed"))?;
    match product
        .catalog_describe(&snapshot, id)
        .map_err(|_| NativeProofError::Invalid("ANN catalog read failed"))?
    {
        Some(LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(definition))) => {
            Ok(definition)
        }
        _ => Err(NativeProofError::Invalid(
            "ANN collection definition is missing",
        )),
    }
}

struct ExtractedWitness {
    path: PathBuf,
}

impl ExtractedWitness {
    fn create(witness: &NativeDirectoryWitness) -> Result<Self, NativeProofError> {
        let nonce = NEXT_EXTRACTION.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-proof-reexecute-{}-{nanos}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(|source| super::model::io_error(&path, source))?;
        let extraction = Self { path };
        for entry in witness.entries() {
            match entry {
                super::model::NativeWitnessEntry::Directory { path } => {
                    let destination = extraction.path.join(path);
                    fs::create_dir(&destination)
                        .map_err(|source| super::model::io_error(&destination, source))?;
                }
                super::model::NativeWitnessEntry::File { path, bytes, .. } => {
                    let destination = extraction.path.join(path);
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&destination)
                        .map_err(|source| super::model::io_error(&destination, source))?;
                    file.write_all(bytes)
                        .map_err(|source| super::model::io_error(&destination, source))?;
                }
            }
        }
        let lock = extraction.path.join("LOCK");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .map_err(|source| super::model::io_error(&lock, source))?;
        Ok(extraction)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ExtractedWitness {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}

fn encode_search_limits(
    encoded: &mut Encoder,
    limits: BoundedSearchLimits,
) -> Result<(), NativeProofError> {
    for value in [
        limits.max_hits,
        limits.max_documents,
        limits.max_matches,
        limits.max_source_bytes,
        limits.max_token_visits,
        limits.max_token_comparisons,
        limits.max_fuzzy_steps,
        limits.max_clauses,
        limits.max_query_bytes,
    ] {
        put_usize(encoded, value)?;
    }
    Ok(())
}

fn decode_search_limits(
    decoder: &mut Decoder<'_>,
) -> Result<BoundedSearchLimits, NativeProofError> {
    Ok(BoundedSearchLimits {
        max_hits: usize_value(decoder)?,
        max_documents: usize_value(decoder)?,
        max_matches: usize_value(decoder)?,
        max_source_bytes: usize_value(decoder)?,
        max_token_visits: usize_value(decoder)?,
        max_token_comparisons: usize_value(decoder)?,
        max_fuzzy_steps: usize_value(decoder)?,
        max_clauses: usize_value(decoder)?,
        max_query_bytes: usize_value(decoder)?,
    })
}

fn encode_query(
    encoded: &mut Encoder,
    query: &BoundedSearchQuery,
    depth: usize,
) -> Result<(), NativeProofError> {
    if depth > MAX_OPERATION_DEPTH {
        return Err(NativeProofError::Invalid(
            "search query nesting is too deep",
        ));
    }
    match query {
        BoundedSearchQuery::Term(value) => {
            encoded.byte(1);
            put_text(encoded, value)?;
        }
        BoundedSearchQuery::Phrase(value) => {
            encoded.byte(2);
            put_text(encoded, value)?;
        }
        BoundedSearchQuery::Prefix(value) => {
            encoded.byte(3);
            put_text(encoded, value)?;
        }
        BoundedSearchQuery::Fuzzy { term, max_distance } => {
            encoded.byte(4);
            put_text(encoded, term)?;
            encoded.byte(*max_distance);
        }
        BoundedSearchQuery::Boolean {
            must,
            should,
            must_not,
        } => {
            encoded.byte(5);
            for values in [must, should, must_not] {
                put_count(encoded, values.len())?;
                for value in values {
                    encode_query(encoded, value, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

fn decode_query(
    decoder: &mut Decoder<'_>,
    depth: usize,
    limits: &NativeVerificationLimits,
) -> Result<BoundedSearchQuery, NativeProofError> {
    if depth > MAX_OPERATION_DEPTH {
        return Err(NativeProofError::Invalid(
            "search query nesting is too deep",
        ));
    }
    Ok(match decoder.byte()? {
        1 => BoundedSearchQuery::Term(text(decoder, limits.max_reexecution_bytes)?),
        2 => BoundedSearchQuery::Phrase(text(decoder, limits.max_reexecution_bytes)?),
        3 => BoundedSearchQuery::Prefix(text(decoder, limits.max_reexecution_bytes)?),
        4 => BoundedSearchQuery::Fuzzy {
            term: text(decoder, limits.max_reexecution_bytes)?,
            max_distance: decoder.byte()?,
        },
        5 => {
            let mut groups = Vec::new();
            for _ in 0..3 {
                let count = bounded_count(
                    decoder,
                    limits.max_reexecution_candidate_items,
                    "search clauses",
                )?;
                let mut values = Vec::new();
                values
                    .try_reserve_exact(count)
                    .map_err(|_| NativeProofError::LengthOverflow)?;
                for _ in 0..count {
                    values.push(decode_query(decoder, depth + 1, limits)?);
                }
                groups.push(values);
            }
            BoundedSearchQuery::Boolean {
                must: groups.remove(0),
                should: groups.remove(0),
                must_not: groups.remove(0),
            }
        }
        _ => return Err(NativeProofError::Invalid("invalid search query tag")),
    })
}

fn encode_integrated_request(
    encoded: &mut Encoder,
    request: &ProductSearchRequest,
) -> Result<(), NativeProofError> {
    encoded.byte(u8::from(request.lexical.is_some()));
    if let Some(lexical) = &request.lexical {
        put_text(encoded, &lexical.query)?;
        put_usize(encoded, lexical.candidate_limit)?;
        encoded.u32(lexical.weight);
    }
    put_count(encoded, request.vectors.len())?;
    for vector in &request.vectors {
        put_text(encoded, &vector.target)?;
        encode_vector(encoded, &vector.query)?;
        put_usize(encoded, vector.candidate_limit)?;
        encoded.u32(vector.weight);
        encode_vector_execution(
            encoded,
            vector.execution.ok_or(NativeProofError::Invalid(
                "search proof requires resolved vector execution",
            ))?,
        )?;
    }
    encode_filter(encoded, &request.filter, 0)?;
    put_count(encoded, request.sort.len())?;
    for sort in &request.sort {
        match &sort.source {
            DocValueSortSource::Score => encoded.byte(1),
            DocValueSortSource::Field(field) => {
                encoded.byte(2);
                put_text(encoded, field)?;
            }
        }
        encoded.byte(match sort.direction {
            DocValueSortDirection::Ascending => 1,
            DocValueSortDirection::Descending => 2,
        });
        encoded.byte(match sort.missing {
            MissingPlacement::First => 1,
            MissingPlacement::Last => 2,
        });
    }
    put_count(encoded, request.facets.len())?;
    for facet in &request.facets {
        put_text(encoded, &facet.field)?;
        put_usize(encoded, facet.limit)?;
    }
    put_count(encoded, request.aggregations.len())?;
    for aggregation in &request.aggregations {
        put_text(encoded, &aggregation.name)?;
        encode_aggregation(encoded, &aggregation.aggregation)?;
    }
    put_usize(encoded, request.limit)
}

fn decode_integrated_request(
    decoder: &mut Decoder<'_>,
    limits: &NativeVerificationLimits,
) -> Result<ProductSearchRequest, NativeProofError> {
    let lexical = match decoder.byte()? {
        0 => None,
        1 => Some(ProductLexicalBranch {
            query: text(decoder, limits.max_reexecution_bytes)?,
            candidate_limit: usize_value(decoder)?,
            weight: decoder.u32()?,
        }),
        _ => return Err(NativeProofError::Invalid("invalid lexical presence tag")),
    };
    let vector_count = bounded_count(
        decoder,
        limits.max_reexecution_candidate_items,
        "vector branches",
    )?;
    let mut vectors = Vec::new();
    vectors
        .try_reserve_exact(vector_count)
        .map_err(|_| NativeProofError::LengthOverflow)?;
    for _ in 0..vector_count {
        vectors.push(ProductVectorBranch {
            target: text(decoder, limits.max_reexecution_bytes)?,
            query: decode_vector(decoder, limits)?,
            candidate_limit: usize_value(decoder)?,
            weight: decoder.u32()?,
            execution: Some(decode_vector_execution(decoder)?),
        });
    }
    let filter = decode_filter(decoder, 0, limits)?;
    let sort_count = bounded_count(
        decoder,
        limits.max_reexecution_candidate_items,
        "sort fields",
    )?;
    let mut sort = Vec::new();
    for _ in 0..sort_count {
        let source = match decoder.byte()? {
            1 => DocValueSortSource::Score,
            2 => DocValueSortSource::Field(text(decoder, limits.max_reexecution_bytes)?),
            _ => return Err(NativeProofError::Invalid("invalid sort source")),
        };
        let direction = match decoder.byte()? {
            1 => DocValueSortDirection::Ascending,
            2 => DocValueSortDirection::Descending,
            _ => return Err(NativeProofError::Invalid("invalid sort direction")),
        };
        let missing = match decoder.byte()? {
            1 => MissingPlacement::First,
            2 => MissingPlacement::Last,
            _ => return Err(NativeProofError::Invalid("invalid missing placement")),
        };
        sort.push(crate::ProductSearchSort {
            source,
            direction,
            missing,
        });
    }
    let facet_count = bounded_count(decoder, limits.max_reexecution_candidate_items, "facets")?;
    let mut facets = Vec::new();
    for _ in 0..facet_count {
        facets.push(crate::ProductFacetRequest {
            field: text(decoder, limits.max_reexecution_bytes)?,
            limit: usize_value(decoder)?,
        });
    }
    let aggregation_count = bounded_count(
        decoder,
        limits.max_reexecution_candidate_items,
        "aggregations",
    )?;
    let mut aggregations = Vec::new();
    for _ in 0..aggregation_count {
        aggregations.push(crate::ProductNamedAggregation {
            name: text(decoder, limits.max_reexecution_bytes)?,
            aggregation: decode_aggregation(decoder, limits)?,
        });
    }
    Ok(ProductSearchRequest {
        lexical,
        vectors,
        filter,
        sort,
        facets,
        aggregations,
        limit: usize_value(decoder)?,
    })
}

fn encode_vector_execution(
    encoded: &mut Encoder,
    execution: ProductVectorExecution,
) -> Result<(), NativeProofError> {
    match execution {
        ProductVectorExecution::Exact => encoded.byte(1),
        ProductVectorExecution::Ann {
            ef_search,
            exact_rerank,
        } => {
            encoded.byte(2);
            put_usize(encoded, ef_search)?;
            encode_optional_usize(encoded, exact_rerank)?;
        }
        ProductVectorExecution::Adaptive {
            exact_candidate_threshold,
            ef_search,
            exact_rerank,
        } => {
            encoded.byte(3);
            put_usize(encoded, exact_candidate_threshold)?;
            put_usize(encoded, ef_search)?;
            encode_optional_usize(encoded, exact_rerank)?;
        }
    }
    Ok(())
}

fn decode_vector_execution(
    decoder: &mut Decoder<'_>,
) -> Result<ProductVectorExecution, NativeProofError> {
    Ok(match decoder.byte()? {
        1 => ProductVectorExecution::Exact,
        2 => ProductVectorExecution::Ann {
            ef_search: usize_value(decoder)?,
            exact_rerank: decode_optional_usize(decoder)?,
        },
        3 => ProductVectorExecution::Adaptive {
            exact_candidate_threshold: usize_value(decoder)?,
            ef_search: usize_value(decoder)?,
            exact_rerank: decode_optional_usize(decoder)?,
        },
        _ => return Err(NativeProofError::Invalid("invalid vector execution")),
    })
}

fn encode_filter(
    encoded: &mut Encoder,
    filter: &DocValueFilter,
    depth: usize,
) -> Result<(), NativeProofError> {
    if depth > MAX_OPERATION_DEPTH {
        return Err(NativeProofError::Invalid("doc-value filter is too deep"));
    }
    match filter {
        DocValueFilter::MatchAll => encoded.byte(1),
        DocValueFilter::Exists(field) => {
            encoded.byte(2);
            put_text(encoded, field)?;
        }
        DocValueFilter::Compare {
            field,
            operator,
            value,
        } => {
            encoded.byte(3);
            put_text(encoded, field)?;
            encoded.byte(match operator {
                DocValueOperator::Equal => 1,
                DocValueOperator::NotEqual => 2,
                DocValueOperator::Less => 3,
                DocValueOperator::LessOrEqual => 4,
                DocValueOperator::Greater => 5,
                DocValueOperator::GreaterOrEqual => 6,
            });
            encode_doc_value(encoded, value)?;
        }
        DocValueFilter::All(children) | DocValueFilter::Any(children) => {
            encoded.byte(if matches!(filter, DocValueFilter::All(_)) {
                4
            } else {
                5
            });
            put_count(encoded, children.len())?;
            for child in children {
                encode_filter(encoded, child, depth + 1)?;
            }
        }
        DocValueFilter::Not(child) => {
            encoded.byte(6);
            encode_filter(encoded, child, depth + 1)?;
        }
    }
    Ok(())
}

fn decode_filter(
    decoder: &mut Decoder<'_>,
    depth: usize,
    limits: &NativeVerificationLimits,
) -> Result<DocValueFilter, NativeProofError> {
    if depth > MAX_OPERATION_DEPTH {
        return Err(NativeProofError::Invalid("doc-value filter is too deep"));
    }
    Ok(match decoder.byte()? {
        1 => DocValueFilter::MatchAll,
        2 => DocValueFilter::Exists(text(decoder, limits.max_reexecution_bytes)?),
        3 => DocValueFilter::Compare {
            field: text(decoder, limits.max_reexecution_bytes)?,
            operator: match decoder.byte()? {
                1 => DocValueOperator::Equal,
                2 => DocValueOperator::NotEqual,
                3 => DocValueOperator::Less,
                4 => DocValueOperator::LessOrEqual,
                5 => DocValueOperator::Greater,
                6 => DocValueOperator::GreaterOrEqual,
                _ => return Err(NativeProofError::Invalid("invalid doc-value operator")),
            },
            value: decode_doc_value(decoder, limits)?,
        },
        tag @ (4 | 5) => {
            let count = bounded_count(
                decoder,
                limits.max_reexecution_candidate_items,
                "filter children",
            )?;
            let mut children = Vec::new();
            for _ in 0..count {
                children.push(decode_filter(decoder, depth + 1, limits)?);
            }
            if tag == 4 {
                DocValueFilter::All(children)
            } else {
                DocValueFilter::Any(children)
            }
        }
        6 => DocValueFilter::Not(Box::new(decode_filter(decoder, depth + 1, limits)?)),
        _ => return Err(NativeProofError::Invalid("invalid doc-value filter tag")),
    })
}

fn encode_aggregation(
    encoded: &mut Encoder,
    aggregation: &DocValueAggregation,
) -> Result<(), NativeProofError> {
    match aggregation {
        DocValueAggregation::Count => encoded.byte(1),
        DocValueAggregation::Sum(field) => {
            encoded.byte(2);
            put_text(encoded, field)?;
        }
        DocValueAggregation::Min(field) => {
            encoded.byte(3);
            put_text(encoded, field)?;
        }
        DocValueAggregation::Max(field) => {
            encoded.byte(4);
            put_text(encoded, field)?;
        }
    }
    Ok(())
}

fn decode_aggregation(
    decoder: &mut Decoder<'_>,
    limits: &NativeVerificationLimits,
) -> Result<DocValueAggregation, NativeProofError> {
    Ok(match decoder.byte()? {
        1 => DocValueAggregation::Count,
        2 => DocValueAggregation::Sum(text(decoder, limits.max_reexecution_bytes)?),
        3 => DocValueAggregation::Min(text(decoder, limits.max_reexecution_bytes)?),
        4 => DocValueAggregation::Max(text(decoder, limits.max_reexecution_bytes)?),
        _ => return Err(NativeProofError::Invalid("invalid aggregation tag")),
    })
}

fn encode_integrated_result(
    encoded: &mut Encoder,
    result: &crate::ProductSearchResult,
) -> Result<(), NativeProofError> {
    put_count(encoded, result.hits.len())?;
    for hit in &result.hits {
        encoded.u128(hit.object_id.get());
        encoded.u64(hit.score.to_bits());
        encode_doc_values(encoded, &hit.doc_values)?;
    }
    put_count(encoded, result.facets.len())?;
    for facet in &result.facets {
        put_text(encoded, &facet.field)?;
        put_count(encoded, facet.buckets.len())?;
        for bucket in &facet.buckets {
            encode_doc_value(encoded, &bucket.value)?;
            encoded.u64(bucket.count);
        }
    }
    put_count(encoded, result.aggregations.len())?;
    for aggregation in &result.aggregations {
        encode_named_aggregation_value(encoded, aggregation)?;
    }
    Ok(())
}

fn encode_vector_receipt(
    encoded: &mut Encoder,
    receipt: &ProductVectorBranchReceipt,
) -> Result<(), NativeProofError> {
    put_text(encoded, &receipt.target)?;
    encoded.byte(match receipt.strategy {
        ProductVectorStrategy::ExactFiltered => 1,
        ProductVectorStrategy::AdaptiveExactFiltered => 2,
        ProductVectorStrategy::FilterAwareAnn => 3,
        ProductVectorStrategy::AdaptiveFilterAwareAnn => 4,
    });
    encoded.byte(u8::from(receipt.approximate));
    put_usize(encoded, receipt.eligible_documents)?;
    put_usize(encoded, receipt.candidate_count)?;
    put_usize(encoded, receipt.visited_nodes)?;
    encoded.byte(u8::from(receipt.exact_reranked));
    Ok(())
}

fn encode_named_aggregation_value(
    encoded: &mut Encoder,
    aggregation: &ProductNamedAggregationValue,
) -> Result<(), NativeProofError> {
    put_text(encoded, &aggregation.name)?;
    match &aggregation.value {
        ProductAggregationValue::Count(value) => {
            encoded.byte(1);
            encoded.u64(*value);
        }
        ProductAggregationValue::Integer(value) => {
            encoded.byte(2);
            encoded.byte(u8::from(value.is_some()));
            if let Some(value) = value {
                encoded.extend(&value.to_le_bytes());
            }
        }
        ProductAggregationValue::Value(value) => {
            encoded.byte(3);
            encoded.byte(u8::from(value.is_some()));
            if let Some(value) = value {
                encode_doc_value(encoded, value)?;
            }
        }
    }
    Ok(())
}

fn encode_doc_values(
    encoded: &mut Encoder,
    values: &BTreeMap<String, ProductDocValue>,
) -> Result<(), NativeProofError> {
    put_count(encoded, values.len())?;
    for (name, value) in values {
        put_text(encoded, name)?;
        encode_doc_value(encoded, value)?;
    }
    Ok(())
}

fn encode_doc_value(
    encoded: &mut Encoder,
    value: &ProductDocValue,
) -> Result<(), NativeProofError> {
    match value {
        ProductDocValue::Boolean(value) => {
            encoded.byte(1);
            encoded.byte(u8::from(*value));
        }
        ProductDocValue::Integer(value) => {
            encoded.byte(2);
            encoded.extend(&value.to_le_bytes());
        }
        ProductDocValue::String(value) => {
            encoded.byte(3);
            put_text(encoded, value)?;
        }
        ProductDocValue::Bytes(value) => {
            encoded.byte(4);
            put_bytes(encoded, value)?;
        }
    }
    Ok(())
}

fn decode_doc_value(
    decoder: &mut Decoder<'_>,
    limits: &NativeVerificationLimits,
) -> Result<ProductDocValue, NativeProofError> {
    Ok(match decoder.byte()? {
        1 => ProductDocValue::Boolean(boolean(decoder)?),
        2 => ProductDocValue::Integer(i64::from_le_bytes(decoder.array()?)),
        3 => ProductDocValue::String(text(decoder, limits.max_reexecution_bytes)?),
        4 => ProductDocValue::Bytes(bytes(decoder, limits.max_reexecution_bytes)?),
        _ => return Err(NativeProofError::Invalid("invalid doc-value tag")),
    })
}

fn encode_vector(encoded: &mut Encoder, vector: &ProductVector) -> Result<(), NativeProofError> {
    put_count(encoded, vector.dimension())?;
    for value in vector.values() {
        encoded.u32(value.to_bits());
    }
    Ok(())
}

fn decode_vector(
    decoder: &mut Decoder<'_>,
    limits: &NativeVerificationLimits,
) -> Result<ProductVector, NativeProofError> {
    let count = bounded_count(
        decoder,
        limits.max_reexecution_candidate_items,
        "vector dimensions",
    )?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| NativeProofError::LengthOverflow)?;
    for _ in 0..count {
        values.push(f32::from_bits(decoder.u32()?));
    }
    ProductVector::new(values).map_err(|_| NativeProofError::Invalid("invalid proof vector"))
}

fn encode_sql_result(
    encoded: &mut Encoder,
    result: &ProductSqlResult,
) -> Result<(), NativeProofError> {
    match result {
        ProductSqlResult::Command {
            rows_affected,
            object_id,
        } => {
            encoded.byte(1);
            encoded.u64(*rows_affected);
            encoded.byte(u8::from(object_id.is_some()));
            if let Some(id) = object_id {
                encoded.u128(id.get());
            }
        }
        ProductSqlResult::Rows { columns, rows } => {
            encoded.byte(2);
            put_count(encoded, columns.len())?;
            for column in columns {
                put_text(encoded, column)?;
            }
            put_count(encoded, rows.len())?;
            for row in rows {
                put_count(encoded, row.len())?;
                for value in row {
                    encode_value(encoded, value, 0)?;
                }
            }
        }
    }
    Ok(())
}

fn encode_value(
    encoded: &mut Encoder,
    value: &ProductValue,
    depth: usize,
) -> Result<(), NativeProofError> {
    if depth > MAX_OPERATION_DEPTH {
        return Err(NativeProofError::Invalid("product value is too deep"));
    }
    match value {
        ProductValue::Null => encoded.byte(0),
        ProductValue::Boolean(value) => {
            encoded.byte(1);
            encoded.byte(u8::from(*value));
        }
        ProductValue::Signed(value) => {
            encoded.byte(2);
            encoded.extend(&value.to_le_bytes());
        }
        ProductValue::Unsigned(value) => {
            encoded.byte(3);
            encoded.u64(*value);
        }
        ProductValue::Decimal(value) => {
            encoded.byte(4);
            encoded.extend(&value.to_le_bytes());
        }
        ProductValue::Float32(value) => {
            encoded.byte(5);
            encoded.u32(value.bits());
        }
        ProductValue::Float64(value) => {
            encoded.byte(6);
            encoded.u64(value.bits());
        }
        ProductValue::Text(value) => {
            encoded.byte(7);
            put_text(encoded, value)?;
        }
        ProductValue::Binary(value) => {
            encoded.byte(8);
            put_bytes(encoded, value)?;
        }
        ProductValue::Date(value) => {
            encoded.byte(9);
            encoded.extend(&value.to_le_bytes());
        }
        ProductValue::Time(value) => {
            encoded.byte(10);
            encoded.u64(*value);
        }
        ProductValue::Timestamp(value) => {
            encoded.byte(11);
            encoded.extend(&value.to_le_bytes());
        }
        ProductValue::Interval {
            months,
            days,
            nanoseconds,
        } => {
            encoded.byte(12);
            encoded.extend(&months.to_le_bytes());
            encoded.extend(&days.to_le_bytes());
            encoded.extend(&nanoseconds.to_le_bytes());
        }
        ProductValue::Uuid(value) => {
            encoded.byte(13);
            encoded.extend(value);
        }
        ProductValue::Array(values) => {
            encoded.byte(14);
            put_count(encoded, values.len())?;
            for value in values {
                encode_value(encoded, value, depth + 1)?;
            }
        }
        ProductValue::Map(values) => {
            encoded.byte(15);
            put_count(encoded, values.len())?;
            for (key, value) in values {
                encode_value(encoded, key, depth + 1)?;
                encode_value(encoded, value, depth + 1)?;
            }
        }
        ProductValue::Vector(values) => {
            encoded.byte(16);
            put_count(encoded, values.len())?;
            for value in values {
                encoded.u32(value.bits());
            }
        }
        ProductValue::Json(value) => {
            encoded.byte(17);
            put_text(encoded, value)?;
        }
        _ => return Err(NativeProofError::Invalid("unsupported product value")),
    }
    Ok(())
}

fn decode_value(
    decoder: &mut Decoder<'_>,
    depth: usize,
    limits: &NativeVerificationLimits,
) -> Result<ProductValue, NativeProofError> {
    if depth > MAX_OPERATION_DEPTH {
        return Err(NativeProofError::Invalid("product value is too deep"));
    }
    Ok(match decoder.byte()? {
        0 => ProductValue::Null,
        1 => ProductValue::Boolean(boolean(decoder)?),
        2 => ProductValue::Signed(i64::from_le_bytes(decoder.array()?)),
        3 => ProductValue::Unsigned(decoder.u64()?),
        4 => ProductValue::Decimal(i128::from_le_bytes(decoder.array()?)),
        5 => ProductValue::Float32(crate::CanonicalF32::new(f32::from_bits(decoder.u32()?))),
        6 => ProductValue::Float64(crate::CanonicalF64::new(f64::from_bits(decoder.u64()?))),
        7 => ProductValue::Text(text(decoder, limits.max_reexecution_bytes)?),
        8 => ProductValue::Binary(bytes(decoder, limits.max_reexecution_bytes)?),
        9 => ProductValue::Date(i32::from_le_bytes(decoder.array()?)),
        10 => ProductValue::Time(decoder.u64()?),
        11 => ProductValue::Timestamp(i64::from_le_bytes(decoder.array()?)),
        12 => ProductValue::Interval {
            months: i32::from_le_bytes(decoder.array()?),
            days: i32::from_le_bytes(decoder.array()?),
            nanoseconds: i64::from_le_bytes(decoder.array()?),
        },
        13 => ProductValue::Uuid(decoder.array()?),
        tag @ (14 | 16) => {
            let count = bounded_count(
                decoder,
                limits.max_reexecution_candidate_items,
                "nested values",
            )?;
            if tag == 14 {
                let mut values = Vec::new();
                for _ in 0..count {
                    values.push(decode_value(decoder, depth + 1, limits)?);
                }
                ProductValue::Array(values)
            } else {
                let mut values = Vec::new();
                for _ in 0..count {
                    values.push(crate::CanonicalF32::new(f32::from_bits(decoder.u32()?)));
                }
                ProductValue::Vector(values)
            }
        }
        15 => {
            let count = bounded_count(
                decoder,
                limits.max_reexecution_candidate_items,
                "map entries",
            )?;
            let mut values = Vec::new();
            for _ in 0..count {
                values.push((
                    decode_value(decoder, depth + 1, limits)?,
                    decode_value(decoder, depth + 1, limits)?,
                ));
            }
            ProductValue::Map(values)
        }
        17 => ProductValue::Json(text(decoder, limits.max_reexecution_bytes)?),
        _ => return Err(NativeProofError::Invalid("invalid product value tag")),
    })
}

fn encode_catalog_list_request(
    encoded: &mut Encoder,
    request: CatalogListRequest,
) -> Result<(), NativeProofError> {
    encode_optional_object_id(encoded, request.parent);
    encoded.byte(request.kind.map_or(0, |kind| kind as u8));
    encode_optional_cursor(encoded, request.cursor);
    put_usize(encoded, request.item_limit)?;
    put_usize(encoded, request.visit_limit)?;
    put_usize(encoded, request.byte_limit)
}

fn decode_catalog_list_request(
    decoder: &mut Decoder<'_>,
) -> Result<CatalogListRequest, NativeProofError> {
    let parent = decode_optional_object_id(decoder)?;
    let kind = match decoder.byte()? {
        0 => None,
        value => Some(catalog_kind(value)?),
    };
    Ok(CatalogListRequest {
        parent,
        kind,
        cursor: decode_optional_cursor(decoder)?,
        item_limit: usize_value(decoder)?,
        visit_limit: usize_value(decoder)?,
        byte_limit: usize_value(decoder)?,
    })
}

fn encode_catalog_summary(
    encoded: &mut Encoder,
    summary: &CatalogObjectSummary,
) -> Result<(), NativeProofError> {
    encoded.u128(summary.id.get());
    encoded.byte(summary.kind as u8);
    encode_name(encoded, &summary.name)?;
    encode_optional_object_id(encoded, summary.parent);
    Ok(())
}

fn encode_name(encoded: &mut Encoder, name: &crate::QualifiedName) -> Result<(), NativeProofError> {
    for component in [&name.database, &name.schema, &name.object] {
        put_text(encoded, component.display())?;
        put_text(encoded, component.lookup())?;
    }
    Ok(())
}

fn encode_snapshot(encoded: &mut Encoder, snapshot: SnapshotIdentity) {
    encoded.extend(&snapshot.directory_lineage);
    encoded.u64(snapshot.visible_csn.map_or(0, crate::Csn::get));
    encoded.u64(snapshot.catalog_version.get());
    encoded.extend(&snapshot.root_digest);
    encoded.extend(&snapshot.logical_time_micros.to_le_bytes());
}

fn decode_snapshot(decoder: &mut Decoder<'_>) -> Result<SnapshotIdentity, NativeProofError> {
    let directory_lineage = decoder.array()?;
    let visible = decoder.u64()?;
    Ok(SnapshotIdentity {
        directory_lineage,
        visible_csn: if visible == 0 {
            None
        } else {
            Some(
                crate::Csn::new(visible)
                    .map_err(|_| NativeProofError::Invalid("invalid snapshot CSN"))?,
            )
        },
        catalog_version: crate::CatalogVersion::new(decoder.u64()?)
            .map_err(|_| NativeProofError::Invalid("invalid snapshot catalog version"))?,
        root_digest: decoder.array()?,
        logical_time_micros: i64::from_le_bytes(decoder.array()?),
    })
}

fn encode_optional_cursor(encoded: &mut Encoder, cursor: Option<CatalogCursor>) {
    encoded.byte(u8::from(cursor.is_some()));
    if let Some(cursor) = cursor {
        encode_snapshot(encoded, cursor.snapshot());
        encoded.u128(cursor.after().get());
    }
}

fn decode_optional_cursor(
    decoder: &mut Decoder<'_>,
) -> Result<Option<CatalogCursor>, NativeProofError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => Ok(Some(CatalogCursor::new(
            decode_snapshot(decoder)?,
            object_id(decoder.u128()?)?,
        ))),
        _ => Err(NativeProofError::Invalid("invalid catalog cursor tag")),
    }
}

fn encode_optional_object_id(encoded: &mut Encoder, id: Option<ObjectId>) {
    encoded.byte(u8::from(id.is_some()));
    if let Some(id) = id {
        encoded.u128(id.get());
    }
}

fn decode_optional_object_id(
    decoder: &mut Decoder<'_>,
) -> Result<Option<ObjectId>, NativeProofError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => Ok(Some(object_id(decoder.u128()?)?)),
        _ => Err(NativeProofError::Invalid("invalid optional object ID")),
    }
}

fn catalog_kind(value: u8) -> Result<CatalogObjectKind, NativeProofError> {
    Ok(match value {
        1 => CatalogObjectKind::Database,
        2 => CatalogObjectKind::Schema,
        3 => CatalogObjectKind::Relation,
        4 => CatalogObjectKind::SecondaryIndex,
        5 => CatalogObjectKind::Keyspace,
        6 => CatalogObjectKind::Structure,
        7 => CatalogObjectKind::SearchCollection,
        8 => CatalogObjectKind::Analyzer,
        9 => CatalogObjectKind::CrossEngineLink,
        _ => return Err(NativeProofError::Invalid("invalid catalog object kind")),
    })
}

const fn catalog_stop_tag(stop: CatalogPageStop) -> u8 {
    match stop {
        CatalogPageStop::Exhausted => 1,
        CatalogPageStop::ItemLimit => 2,
        CatalogPageStop::VisitLimit => 3,
        CatalogPageStop::ByteLimit => 4,
    }
}

fn encode_optional_usize(
    encoded: &mut Encoder,
    value: Option<usize>,
) -> Result<(), NativeProofError> {
    encoded.byte(u8::from(value.is_some()));
    if let Some(value) = value {
        put_usize(encoded, value)?;
    }
    Ok(())
}

fn decode_optional_usize(decoder: &mut Decoder<'_>) -> Result<Option<usize>, NativeProofError> {
    match decoder.byte()? {
        0 => Ok(None),
        1 => Ok(Some(usize_value(decoder)?)),
        _ => Err(NativeProofError::Invalid("invalid optional integer")),
    }
}

fn put_count(encoded: &mut Encoder, value: usize) -> Result<(), NativeProofError> {
    encoded.u32(u32::try_from(value).map_err(|_| NativeProofError::LengthOverflow)?);
    Ok(())
}

fn put_usize(encoded: &mut Encoder, value: usize) -> Result<(), NativeProofError> {
    encoded.u64(u64::try_from(value).map_err(|_| NativeProofError::LengthOverflow)?);
    Ok(())
}

fn put_text(encoded: &mut Encoder, value: &str) -> Result<(), NativeProofError> {
    put_bytes(encoded, value.as_bytes())
}

fn put_bytes(encoded: &mut Encoder, value: &[u8]) -> Result<(), NativeProofError> {
    encoded.u64(u64::try_from(value.len()).map_err(|_| NativeProofError::LengthOverflow)?);
    encoded.extend(value);
    Ok(())
}

fn bounded_count(
    decoder: &mut Decoder<'_>,
    maximum: usize,
    resource: &'static str,
) -> Result<usize, NativeProofError> {
    let value = usize::try_from(decoder.u32()?).map_err(|_| NativeProofError::LengthOverflow)?;
    if value > maximum {
        return Err(limit(resource, value, maximum));
    }
    Ok(value)
}

fn usize_value(decoder: &mut Decoder<'_>) -> Result<usize, NativeProofError> {
    usize::try_from(decoder.u64()?).map_err(|_| NativeProofError::LengthOverflow)
}

fn bytes(decoder: &mut Decoder<'_>, maximum: u64) -> Result<Vec<u8>, NativeProofError> {
    let length = decoder.u64()?;
    if length > maximum {
        return Err(limit("semantic field bytes", length, maximum));
    }
    decoder.owned(usize::try_from(length).map_err(|_| NativeProofError::LengthOverflow)?)
}

fn text(decoder: &mut Decoder<'_>, maximum: u64) -> Result<String, NativeProofError> {
    String::from_utf8(bytes(decoder, maximum)?)
        .map_err(|_| NativeProofError::Invalid("semantic text is not UTF-8"))
}

fn boolean(decoder: &mut Decoder<'_>) -> Result<bool, NativeProofError> {
    match decoder.byte()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(NativeProofError::Invalid("invalid boolean")),
    }
}

fn object_id(value: u128) -> Result<ObjectId, NativeProofError> {
    ObjectId::new(value).map_err(|_| NativeProofError::Invalid("zero object ID"))
}

fn u64_value(value: usize) -> Result<u64, NativeProofError> {
    u64::try_from(value).map_err(|_| NativeProofError::LengthOverflow)
}
