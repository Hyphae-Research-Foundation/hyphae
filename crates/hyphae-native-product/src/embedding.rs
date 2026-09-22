// SPDX-License-Identifier: Apache-2.0

//! Catalog-bound embedding execution and atomic integrated ingestion.

use std::{fmt::Debug, sync::Arc};

use hyphae_native_catalog::{
    CatalogObjectV2, EmbeddingProfileDefinition, LogicalCatalogObject, MAX_CATALOG_NAME_BYTES,
};

use crate::{
    NativeProduct, ObjectId, ProductCommitReceipt, ProductDocValue, ProductDocument,
    ProductDurability, ProductError, ProductErrorCode, ProductLimits, ProductSearchIngestBatch,
    ProductVector, SnapshotIdentity,
};

const COMPLETION_MAGIC: &[u8; 8] = b"HYPEMB01";
const COMPLETION_PREFIX_BYTES: usize = 68;
const COMPLETION_BYTES: usize = COMPLETION_PREFIX_BYTES + 16;
const EMBEDDING_STORAGE_PREFIX: &[u8] = b"\0hyphae.product.embedding.v1\0";

/// Largest generated-vector allocation admitted by one embedding operation.
pub const MAX_PRODUCT_EMBEDDING_OUTPUT_BYTES: usize =
    crate::MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS * 1_024 * size_of::<f32>();

/// Executor-visible ceilings for one admitted embedding batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductEmbeddingExecutionLimits {
    /// Maximum inputs in this call.
    pub max_inputs: usize,
    /// Maximum aggregate logical input bytes.
    pub max_input_bytes: usize,
    /// Maximum token positions for one formatted input.
    pub max_input_tokens_per_input: u32,
    /// Maximum aggregate token positions across the call.
    pub max_total_input_tokens: usize,
    /// Exact output dimension required by the catalog target.
    pub output_dimension: u16,
    /// Maximum aggregate returned vector bytes.
    pub max_output_bytes: usize,
}

/// Borrowed, catalog-bound request presented to an embedding executor.
#[derive(Clone, Copy, Debug)]
pub struct ProductEmbeddingExecutorRequest<'a> {
    /// Immutable catalog profile controlling artifact and pipeline identity.
    pub profile: &'a EmbeddingProfileDefinition,
    /// Exact normalized named-vector target.
    pub target: &'a str,
    /// Passage documents in caller order. Their vector maps are empty.
    pub documents: &'a [ProductDocument],
    /// Hard ceilings the executor must enforce while tokenizing and evaluating.
    pub limits: ProductEmbeddingExecutionLimits,
}

/// Bounded executor result in the same order as the input documents.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductEmbeddingBatchOutput {
    /// One finite canonical vector per input.
    pub vectors: Vec<ProductVector>,
    /// Actual formatted token positions consumed per input.
    pub input_tokens: Vec<u32>,
}

/// Pluggable local embedding execution contract.
///
/// Implementations must apply the supplied catalog profile exactly, enforce
/// every supplied limit incrementally, and call `checkpoint` between inputs
/// and during long-running model execution. This crate intentionally provides
/// no concrete model, provider, device, or wire implementation.
pub trait ProductEmbeddingExecutor: Debug + Send + Sync {
    /// Embeds one bounded ordered passage batch.
    ///
    /// # Errors
    ///
    /// Returns a stable product error without publishing product state.
    fn embed_passages(
        &self,
        request: ProductEmbeddingExecutorRequest<'_>,
        checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
    ) -> Result<ProductEmbeddingBatchOutput, ProductError>;
}

/// Definite result of one embedded-and-ingested batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductEmbedAndIngestReceipt {
    /// Snapshot containing the atomically accepted documents and vectors.
    pub snapshot: SnapshotIdentity,
    /// Original native commit evidence, including on durable replay.
    pub commit: ProductCommitReceipt,
    /// Catalog embedding profile that produced the target vectors.
    pub profile: ObjectId,
    /// Documents represented by the atomic completion marker.
    pub documents: usize,
    /// Actual aggregate formatted token positions consumed by the executor.
    pub input_tokens: usize,
    /// Whether durable completion suppressed executor invocation and publication.
    pub idempotent_replay: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompletionMarker {
    digest: [u8; 32],
    profile: ObjectId,
    documents: usize,
    input_tokens: usize,
    transaction_id: u128,
}

impl NativeProduct {
    /// Installs the process-local executor used by `EmbedAndIngestBatch`.
    pub fn set_embedding_executor(&mut self, executor: Arc<dyn ProductEmbeddingExecutor>) {
        self.embedding_executor = Some(executor);
    }

    /// Returns whether this product handle has an embedding executor installed.
    pub fn has_embedding_executor(&self) -> bool {
        self.embedding_executor.is_some()
    }

    pub(crate) fn embedding_profile_for_target(
        &self,
        collection: ObjectId,
        target: &str,
    ) -> Result<EmbeddingProfileDefinition, ProductError> {
        if target.is_empty() || target.len() > MAX_CATALOG_NAME_BYTES {
            return Err(invalid_request());
        }
        let snapshot = self.catalog_snapshot()?;
        let object = self
            .catalog_describe(&snapshot, collection)?
            .ok_or_else(|| {
                ProductError::from_code(ProductErrorCode::ObjectNotFound).with_object_id(collection)
            })?;
        let LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(definition)) = object else {
            return Err(invalid_request());
        };
        let vector = definition
            .vectors
            .iter()
            .find(|vector| vector.name.lookup() == target)
            .ok_or_else(invalid_request)?;
        let profile_id = vector.embedding_profile.ok_or_else(invalid_request)?;
        let profile = self
            .catalog_describe(&snapshot, profile_id)?
            .ok_or_else(corruption)?;
        let LogicalCatalogObject::V2(CatalogObjectV2::EmbeddingProfile(profile)) = profile else {
            return Err(corruption());
        };
        vector
            .validate_embedding_profile(&profile)
            .map_err(|_| corruption())?;
        Ok(profile)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the atomic operation keeps execution, authorization, bounds, and commit controls explicit"
    )]
    pub(crate) fn embed_and_ingest_batch(
        &mut self,
        collection: ObjectId,
        target: &str,
        batch: &ProductSearchIngestBatch,
        limits: ProductLimits,
        logical_time_micros: i64,
        durability: ProductDurability,
        mut reauthorize: impl FnMut(&NativeProduct) -> Result<(), ProductError>,
        mut checkpoint: impl FnMut() -> Result<(), ProductError>,
    ) -> Result<ProductEmbedAndIngestReceipt, ProductError> {
        validate_embedding_batch_shape(target, batch, limits)?;
        let profile = self.embedding_profile_for_target(collection, target)?;
        let digest = embedding_request_digest(collection, target, batch, &profile)?;
        let completion_key = completion_key(collection, batch.idempotency_id);
        if let Some(encoded) = self
            .database
            .get_latest_structure(&completion_key, logical_time_micros)?
        {
            let marker = decode_completion(&encoded)?;
            if marker.digest != digest || marker.profile != profile.header.id {
                return Err(idempotency_conflict());
            }
            return Ok(ProductEmbedAndIngestReceipt {
                snapshot: self.snapshot_bounded(logical_time_micros)?.identity(),
                commit: self.original_search_receipt(marker.transaction_id)?,
                profile: marker.profile,
                documents: marker.documents,
                input_tokens: marker.input_tokens,
                idempotent_replay: true,
            });
        }

        reauthorize(self)?;
        checkpoint()?;
        let executor = self
            .embedding_executor
            .clone()
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Unavailable))?;
        let input_bytes = embedding_input_bytes(target, batch)?;
        let output_bytes = batch
            .documents
            .len()
            .checked_mul(usize::from(profile.vector_type.dimension()))
            .and_then(|count| count.checked_mul(size_of::<f32>()))
            .ok_or_else(limit_exceeded)?;
        let execution_limits = ProductEmbeddingExecutionLimits {
            max_inputs: batch.documents.len(),
            max_input_bytes: input_bytes,
            max_input_tokens_per_input: profile.max_input_tokens,
            max_total_input_tokens: limits.max_work_units,
            output_dimension: profile.vector_type.dimension(),
            max_output_bytes: output_bytes,
        };
        let output = executor.embed_passages(
            ProductEmbeddingExecutorRequest {
                profile: &profile,
                target,
                documents: &batch.documents,
                limits: execution_limits,
            },
            &mut checkpoint,
        )?;
        let input_tokens = validate_embedding_output(
            &output,
            batch.documents.len(),
            execution_limits,
            &mut checkpoint,
        )?;

        let mut ingest = batch.clone();
        for (document, vector) in ingest.documents.iter_mut().zip(output.vectors) {
            if document.vectors.insert(target.to_owned(), vector).is_some() {
                return Err(invalid_request());
            }
        }
        let completion_prefix = encode_completion_prefix(CompletionMarker {
            digest,
            profile: profile.header.id,
            documents: ingest.documents.len(),
            input_tokens,
            transaction_id: 0,
        })?;
        let (snapshot, commit) = self.ingest_search_batch_with_completion(
            collection,
            &ingest,
            logical_time_micros,
            durability,
            completion_key,
            completion_prefix,
            |product| {
                checkpoint()?;
                reauthorize(product)
            },
        )?;
        Ok(ProductEmbedAndIngestReceipt {
            snapshot,
            commit,
            profile: profile.header.id,
            documents: ingest.documents.len(),
            input_tokens,
            idempotent_replay: false,
        })
    }
}

pub(crate) fn validate_embedding_batch_shape(
    target: &str,
    batch: &ProductSearchIngestBatch,
    limits: ProductLimits,
) -> Result<(), ProductError> {
    if target.is_empty()
        || target.len() > MAX_CATALOG_NAME_BYTES
        || batch.idempotency_id == 0
        || batch.documents.is_empty()
        || batch
            .documents
            .iter()
            .any(|document| !document.vectors.is_empty())
    {
        return Err(invalid_request());
    }
    if batch.documents.len() > crate::MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS {
        return Err(limit_exceeded());
    }
    let (count, bytes, work, memory) = embedding_batch_request_cost(target, batch);
    if bytes > crate::MAX_PRODUCT_SEARCH_BATCH_BYTES {
        return Err(limit_exceeded());
    }
    limits.admit_request(count, bytes, work, memory)
}

pub(crate) fn embedding_batch_request_cost(
    target: &str,
    batch: &ProductSearchIngestBatch,
) -> (usize, usize, usize, usize) {
    let bytes = embedding_input_bytes(target, batch).unwrap_or(usize::MAX);
    let output = batch
        .documents
        .len()
        .saturating_mul(1_024)
        .saturating_mul(size_of::<f32>());
    (
        batch.documents.len(),
        bytes,
        batch.documents.len().max(1),
        bytes.saturating_add(output),
    )
}

fn embedding_input_bytes(
    target: &str,
    batch: &ProductSearchIngestBatch,
) -> Result<usize, ProductError> {
    batch
        .documents
        .iter()
        .try_fold(32_usize.saturating_add(target.len()), |total, document| {
            let document_bytes = document.doc_values.iter().try_fold(
                24_usize.saturating_add(document.text.len()),
                |sum, (name, value)| {
                    sum.checked_add(name.len())
                        .and_then(|sum| sum.checked_add(doc_value_bytes(value)))
                        .ok_or_else(limit_exceeded)
                },
            )?;
            total.checked_add(document_bytes).ok_or_else(limit_exceeded)
        })
}

fn validate_embedding_output(
    output: &ProductEmbeddingBatchOutput,
    documents: usize,
    limits: ProductEmbeddingExecutionLimits,
    checkpoint: &mut impl FnMut() -> Result<(), ProductError>,
) -> Result<usize, ProductError> {
    if output.vectors.len() != documents || output.input_tokens.len() != documents {
        return Err(invalid_request());
    }
    let mut total_tokens = 0_usize;
    let mut total_output_bytes = 0_usize;
    for (vector, tokens) in output.vectors.iter().zip(&output.input_tokens) {
        checkpoint()?;
        if vector.dimension() != usize::from(limits.output_dimension) || *tokens == 0 {
            return Err(invalid_request());
        }
        if *tokens > limits.max_input_tokens_per_input {
            return Err(limit_exceeded());
        }
        total_tokens = total_tokens
            .checked_add(usize::try_from(*tokens).map_err(|_| limit_exceeded())?)
            .ok_or_else(limit_exceeded)?;
        total_output_bytes = total_output_bytes
            .checked_add(vector.dimension().saturating_mul(size_of::<f32>()))
            .ok_or_else(limit_exceeded)?;
        if total_tokens > limits.max_total_input_tokens
            || total_output_bytes > limits.max_output_bytes
        {
            return Err(limit_exceeded());
        }
    }
    Ok(total_tokens)
}

fn embedding_request_digest(
    collection: ObjectId,
    target: &str,
    batch: &ProductSearchIngestBatch,
    profile: &EmbeddingProfileDefinition,
) -> Result<[u8; 32], ProductError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae.product.embed-and-ingest.v1\0");
    hasher.update(&collection.get().to_le_bytes());
    hasher.update(&batch.idempotency_id.to_le_bytes());
    hash_bytes(&mut hasher, target.as_bytes())?;
    let encoded_profile =
        LogicalCatalogObject::V2(CatalogObjectV2::EmbeddingProfile(profile.clone()))
            .encode_definition_v2()
            .map_err(|_| corruption())?;
    hash_bytes(&mut hasher, &encoded_profile)?;
    hasher.update(
        &u32::try_from(batch.documents.len())
            .map_err(|_| limit_exceeded())?
            .to_le_bytes(),
    );
    for document in &batch.documents {
        hasher.update(&document.object_id.get().to_le_bytes());
        hash_bytes(&mut hasher, document.text.as_bytes())?;
        hasher.update(
            &u32::try_from(document.doc_values.len())
                .map_err(|_| limit_exceeded())?
                .to_le_bytes(),
        );
        for (name, value) in &document.doc_values {
            hash_bytes(&mut hasher, name.as_bytes())?;
            match value {
                ProductDocValue::Boolean(value) => {
                    hasher.update(&[1, u8::from(*value)]);
                }
                ProductDocValue::Integer(value) => {
                    hasher.update(&[2]);
                    hasher.update(&value.to_le_bytes());
                }
                ProductDocValue::String(value) => {
                    hasher.update(&[3]);
                    hash_bytes(&mut hasher, value.as_bytes())?;
                }
                ProductDocValue::Bytes(value) => {
                    hasher.update(&[4]);
                    hash_bytes(&mut hasher, value)?;
                }
                ProductDocValue::Float(value) => {
                    hasher.update(&[5]);
                    hasher.update(&value.bits().to_le_bytes());
                }
            }
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) -> Result<(), ProductError> {
    hasher.update(
        &u32::try_from(value.len())
            .map_err(|_| limit_exceeded())?
            .to_le_bytes(),
    );
    hasher.update(value);
    Ok(())
}

fn completion_key(collection: ObjectId, idempotency_id: u128) -> Vec<u8> {
    let mut key = Vec::with_capacity(EMBEDDING_STORAGE_PREFIX.len() + 32);
    key.extend_from_slice(EMBEDDING_STORAGE_PREFIX);
    key.extend_from_slice(&collection.get().to_be_bytes());
    key.extend_from_slice(&idempotency_id.to_be_bytes());
    key
}

fn encode_completion_prefix(marker: CompletionMarker) -> Result<Vec<u8>, ProductError> {
    let mut encoded = Vec::with_capacity(COMPLETION_PREFIX_BYTES);
    encoded.extend_from_slice(COMPLETION_MAGIC);
    encoded.extend_from_slice(&marker.digest);
    encoded.extend_from_slice(&marker.profile.get().to_be_bytes());
    encoded.extend_from_slice(
        &u32::try_from(marker.documents)
            .map_err(|_| limit_exceeded())?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(
        &u64::try_from(marker.input_tokens)
            .map_err(|_| limit_exceeded())?
            .to_le_bytes(),
    );
    debug_assert_eq!(encoded.len(), COMPLETION_PREFIX_BYTES);
    Ok(encoded)
}

fn decode_completion(encoded: &[u8]) -> Result<CompletionMarker, ProductError> {
    if encoded.len() != COMPLETION_BYTES || encoded.get(..8) != Some(COMPLETION_MAGIC.as_slice()) {
        return Err(corruption());
    }
    let profile = ObjectId::new(u128::from_be_bytes(
        encoded[40..56].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    let documents = usize::try_from(u32::from_le_bytes(
        encoded[56..60].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    let input_tokens = usize::try_from(u64::from_le_bytes(
        encoded[60..68].try_into().map_err(|_| corruption())?,
    ))
    .map_err(|_| corruption())?;
    let transaction_id = u128::from_le_bytes(
        encoded[COMPLETION_PREFIX_BYTES..]
            .try_into()
            .map_err(|_| corruption())?,
    );
    if documents == 0
        || documents > crate::MAX_PRODUCT_SEARCH_BATCH_DOCUMENTS
        || input_tokens == 0
        || transaction_id == 0
    {
        return Err(corruption());
    }
    Ok(CompletionMarker {
        digest: encoded[8..40].try_into().map_err(|_| corruption())?,
        profile,
        documents,
        input_tokens,
        transaction_id,
    })
}

const fn doc_value_bytes(value: &ProductDocValue) -> usize {
    match value {
        ProductDocValue::Boolean(_) => 1,
        ProductDocValue::Integer(_) | ProductDocValue::Float(_) => 8,
        ProductDocValue::String(value) => value.len(),
        ProductDocValue::Bytes(value) => value.len(),
    }
}

fn invalid_request() -> ProductError {
    ProductError::from_code(ProductErrorCode::InvalidRequest)
}

fn limit_exceeded() -> ProductError {
    ProductError::from_code(ProductErrorCode::LimitExceeded)
}

fn idempotency_conflict() -> ProductError {
    ProductError::from_code(ProductErrorCode::IdempotencyConflict)
}

fn corruption() -> ProductError {
    ProductError::from_code(ProductErrorCode::Corruption)
}
