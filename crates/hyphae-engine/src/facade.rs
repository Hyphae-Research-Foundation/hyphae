// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};

use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
use hyphae_query::{
    BoundedQueryError, ExecutionLimits, Query, QueryError, QueryResult, Record, execute,
    execute_with_byte_limit, validate_query,
};
use hyphae_retrieval::{
    DurableVectorRecord, ExactRetrievalError, ExactRetrievalLimits, ExactRetrievalOutcome,
    ExactRetrievalRequest, HybridError, HybridOutcome, HybridRequest, LexicalError,
    LexicalIndexDefinition, LexicalLimits, LexicalOutcome, LexicalRequest, RetrievalError,
    RetrievalLimits, RetrievalOutcome, RetrievalRequest, VectorRecord, fuse_hybrid, retrieve,
    retrieve_exact, retrieve_lexical_materialized, tokenize_v1_checked,
};
use hyphae_storage::{
    AppendOutcome, BackupError, BackupInfo, CompactionOutcome, MAX_SCAN_PAGE_ENTRIES,
    MaintenanceLimits, Mutation, RestoreInfo, ScanPageError, SnapshotError, SnapshotInfo,
    StorageEngine, StorageError, StorageLimitError, StorageLimits, StorageRecoveryReport,
    VectorEntriesError, restore_backup, verify_backup,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    DocumentError, ExactRetrievalProof, ExactRetrievalProofArtifact, HybridRetrievalProof,
    HybridRetrievalProofArtifact, LexicalRetrievalProof, LexicalRetrievalProofArtifact, ProofError,
    ResultProof, ResultProofArtifact, RetrievalProofError, decode_document, encode_document,
};

/// Failure while operating the embeddable Hyphae facade.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Durable embedded storage failed.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// Portable backup creation, verification, or restore failed.
    #[error(transparent)]
    Backup(#[from] BackupError),

    /// Canonical document encoding or verification failed.
    #[error(transparent)]
    Document(#[from] DocumentError),

    /// Structured query validation or execution failed.
    #[error(transparent)]
    Query(#[from] QueryError),

    /// Exact semantic retrieval failed.
    #[error(transparent)]
    Retrieval(#[from] RetrievalError),

    /// Durable exact retrieval failed.
    #[error(transparent)]
    ExactRetrieval(#[from] ExactRetrievalError),

    /// Canonical result-proof creation failed.
    #[error(transparent)]
    Proof(#[from] ProofError),

    /// Canonical retrieval-proof creation failed.
    #[error(transparent)]
    RetrievalProof(#[from] RetrievalProofError),

    /// Provider-free lexical retrieval failed.
    #[error(transparent)]
    Lexical(#[from] LexicalError),

    /// Deterministic hybrid fusion failed.
    #[error(transparent)]
    Hybrid(#[from] HybridError),

    /// One atomic document batch repeats a key.
    #[error("atomic document batch contains a duplicate key")]
    DuplicateDocumentKey,

    /// An atomic batch must contain at least one item.
    #[error("atomic batch must contain at least one item")]
    EmptyBatch,
}

/// Failure from the additive engine query entry points that enforce an
/// aggregate durable byte budget.
#[derive(Debug, Error)]
pub enum BoundedEngineQueryError {
    /// Ordinary embedded-engine failure.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// Aggregate durable key and canonical-document bytes exceeded policy.
    #[error("global scanned-byte budget exceeded: {maximum}")]
    ScannedByteBudgetExceeded {
        /// Configured maximum.
        maximum: u64,
    },
}

impl From<BoundedQueryError> for BoundedEngineQueryError {
    fn from(source: BoundedQueryError) -> Self {
        match source {
            BoundedQueryError::Query(source) => Self::Engine(EngineError::Query(source)),
            BoundedQueryError::RecordDocument(source) => {
                Self::Engine(EngineError::Document(source))
            }
            BoundedQueryError::ScannedByteBudgetExceeded { maximum } => {
                Self::ScannedByteBudgetExceeded { maximum }
            }
        }
    }
}

/// Newly opened embeddable engine and durable recovery evidence.
#[derive(Debug)]
pub struct OpenedEngine {
    /// Ready engine facade.
    pub engine: HyphaeEngine,
    /// Log verification and index replay evidence.
    pub recovery: StorageRecoveryReport,
}

#[derive(Debug)]
struct HybridExecution {
    started: Instant,
    total_timeout: Duration,
    lexical: LexicalOutcome,
    vector: ExactRetrievalOutcome,
    outcome: HybridOutcome,
}

/// Embeddable autonomous Hyphae engine.
#[derive(Debug)]
pub struct HyphaeEngine {
    storage: StorageEngine,
}

impl HyphaeEngine {
    /// Opens one exclusively owned data directory and completes recovery.
    ///
    /// # Errors
    ///
    /// Returns an error for directory contention, corruption, unsupported
    /// formats, snapshot mismatch, or failed index replay.
    pub fn open(path: impl AsRef<Path>) -> Result<OpenedEngine, EngineError> {
        let opened = StorageEngine::open(path)?;
        Ok(OpenedEngine {
            engine: Self {
                storage: opened.storage,
            },
            recovery: opened.recovery,
        })
    }

    /// Opens one data directory under explicit finite recovery and
    /// maintenance limits.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, contention, corruption, exhausted
    /// recovery policy, timeout, unsupported formats, or failed replay.
    pub fn open_with_limits(
        path: impl AsRef<Path>,
        limits: StorageLimits,
    ) -> Result<OpenedEngine, EngineError> {
        let opened = StorageEngine::open_with_limits(path, limits)?;
        Ok(OpenedEngine {
            engine: Self {
                storage: opened.storage,
            },
            recovery: opened.recovery,
        })
    }

    /// Returns the owned data-directory path.
    pub fn data_path(&self) -> &Path {
        self.storage.data_path()
    }

    /// Atomically stores one canonical structured record.
    ///
    /// # Errors
    ///
    /// Returns a document codec or durable storage error.
    pub fn put_record(
        &mut self,
        transaction_id: Uuid,
        record: &Record,
    ) -> Result<AppendOutcome, EngineError> {
        self.put_records(transaction_id, std::slice::from_ref(record))
    }

    /// Atomically stores a batch of canonical structured records.
    ///
    /// Encoding every document and checking duplicate keys happens before the
    /// log append, so a codec failure cannot partially commit the batch.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate batch keys, document bounds, key bounds,
    /// idempotency conflicts, or durable storage failures.
    pub fn put_records(
        &mut self,
        transaction_id: Uuid,
        records: &[Record],
    ) -> Result<AppendOutcome, EngineError> {
        if records.is_empty() {
            return Err(EngineError::EmptyBatch);
        }
        let mut keys = BTreeSet::new();
        let mut mutations = Vec::with_capacity(records.len());
        for record in records {
            if !keys.insert(record.key.as_slice()) {
                return Err(EngineError::DuplicateDocumentKey);
            }
            mutations.push(Mutation::put(
                record.key.clone(),
                encode_document(&record.value)?,
            ));
        }
        Ok(self.storage.write(transaction_id, &mutations)?)
    }

    /// Atomically deletes one structured record.
    ///
    /// # Errors
    ///
    /// Returns a key-validation, idempotency, or durable storage error.
    pub fn delete_record(
        &mut self,
        transaction_id: Uuid,
        key: &[u8],
    ) -> Result<AppendOutcome, EngineError> {
        self.delete_records(transaction_id, &[key])
    }

    /// Atomically deletes a batch of structured records.
    ///
    /// Duplicate keys are rejected before the log append. Deleting a missing
    /// key remains a successful durable operation.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate keys, invalid key bounds, idempotency
    /// conflicts, or durable storage failures.
    pub fn delete_records(
        &mut self,
        transaction_id: Uuid,
        keys: &[&[u8]],
    ) -> Result<AppendOutcome, EngineError> {
        if keys.is_empty() {
            return Err(EngineError::EmptyBatch);
        }
        let mut unique = BTreeSet::new();
        let mut mutations = Vec::with_capacity(keys.len());
        for key in keys {
            if !unique.insert(*key) {
                return Err(EngineError::DuplicateDocumentKey);
            }
            mutations.push(Mutation::delete(*key));
        }
        Ok(self.storage.write(transaction_id, &mutations)?)
    }

    /// Gets and verifies one structured record by binary key.
    ///
    /// # Errors
    ///
    /// Returns a key, storage, or canonical document verification error.
    pub fn get_record(&self, key: &[u8]) -> Result<Option<Record>, EngineError> {
        self.storage
            .get(key)?
            .map(|encoded| {
                Ok(Record {
                    key: key.to_vec(),
                    value: decode_document(&encoded)?,
                })
            })
            .transpose()
    }

    /// Gets one structured record and binds the complete result, including
    /// absence, to a canonical snapshot witness.
    ///
    /// # Errors
    ///
    /// Returns a key, storage, document, snapshot, or result-proof error.
    pub fn get_record_with_proof(&self, key: &[u8]) -> Result<ResultProofArtifact, EngineError> {
        let result = self.get_record(key)?;
        let snapshot = self.snapshot()?;
        let proof = ResultProof::for_get(&snapshot, key.to_vec(), result)?;
        Ok(ResultProofArtifact { proof, snapshot })
    }

    /// Gets one record and creates its witness under explicit maintenance
    /// limits.
    ///
    /// # Errors
    ///
    /// Returns a key, storage, document, snapshot-limit, timeout, or
    /// result-proof error.
    pub fn get_record_with_proof_with_limits(
        &self,
        key: &[u8],
        maintenance: &MaintenanceLimits,
    ) -> Result<ResultProofArtifact, EngineError> {
        let started = Instant::now();
        let total_timeout = maintenance.timeout;
        let result = self.get_record(key)?;
        let maintenance = remaining_maintenance_limits(maintenance, started, total_timeout)?;
        let snapshot = self.snapshot_with_limits(&maintenance)?;
        let proof = ResultProof::for_get(&snapshot, key.to_vec(), result)?;
        ensure_total_timeout(started, total_timeout)?;
        Ok(ResultProofArtifact { proof, snapshot })
    }

    /// Executes deterministic structured query over all durable documents.
    ///
    /// Storage scan and document decoding consume the same wall-clock timeout;
    /// the reference executor receives only the remaining duration.
    ///
    /// # Errors
    ///
    /// Returns a storage, document, query validation, global budget, aggregate,
    /// or timeout error. No partial page is returned.
    pub fn query(
        &self,
        query: &Query,
        limits: &ExecutionLimits,
    ) -> Result<QueryResult, EngineError> {
        match self.query_internal(query, limits, None) {
            Ok(result) => Ok(result),
            Err(BoundedEngineQueryError::Engine(source)) => Err(source),
            // INVARIANT: `query_internal` receives no byte budget here, so
            // the budget-exceeded variant cannot be produced on this path.
            Err(BoundedEngineQueryError::ScannedByteBudgetExceeded { .. }) => {
                unreachable!("legacy query execution does not enforce an aggregate byte limit")
            }
        }
    }

    /// Executes deterministic structured query under an explicit aggregate
    /// durable key/document byte budget.
    ///
    /// The budget is enforced inside each storage page before its key/value
    /// bytes are cloned and again by the reference executor. Existing
    /// Legacy [`Self::query`] callers retain the 0.2.0 count-bounded behavior.
    ///
    /// # Errors
    ///
    /// Returns a storage, document, query validation, global count/byte
    /// budget, aggregate, or timeout error. No partial page is returned.
    pub fn query_with_byte_limit(
        &self,
        query: &Query,
        limits: &ExecutionLimits,
        max_scanned_bytes: u64,
    ) -> Result<QueryResult, BoundedEngineQueryError> {
        self.query_internal(query, limits, Some(max_scanned_bytes))
    }

    fn query_internal(
        &self,
        query: &Query,
        limits: &ExecutionLimits,
        max_scanned_bytes: Option<u64>,
    ) -> Result<QueryResult, BoundedEngineQueryError> {
        validate_query(query, limits).map_err(EngineError::from)?;
        let started = Instant::now();
        let mut records = Vec::new();
        let mut after = None;
        let mut scanned_bytes = 0_u64;
        loop {
            if started.elapsed() >= limits.timeout {
                return Err(EngineError::from(QueryError::TimedOut).into());
            }
            let loaded = u64::try_from(records.len()).unwrap_or(u64::MAX);
            let remaining = limits.max_scanned_records.saturating_sub(loaded);
            let remaining_entries = match usize::try_from(remaining) {
                Ok(value) => value,
                Err(_) => usize::MAX,
            };
            let page_limit = remaining_entries
                .saturating_add(1)
                .min(MAX_SCAN_PAGE_ENTRIES);
            let page = if let Some(maximum) = max_scanned_bytes {
                let remaining_bytes = maximum.saturating_sub(scanned_bytes);
                match self.storage.scan_page_with_byte_limit(
                    after.as_deref(),
                    page_limit,
                    remaining_bytes,
                ) {
                    Err(ScanPageError::ByteBudgetExceeded { .. }) => {
                        return Err(BoundedEngineQueryError::ScannedByteBudgetExceeded { maximum });
                    }
                    Err(ScanPageError::Storage(source)) => {
                        return Err(EngineError::from(source).into());
                    }
                    Ok(page) => page,
                }
            } else {
                self.storage
                    .scan_page(after.as_deref(), page_limit)
                    .map_err(EngineError::from)?
            };
            for entry in page.entries {
                if u64::try_from(records.len()).unwrap_or(u64::MAX) >= limits.max_scanned_records {
                    return Err(EngineError::from(QueryError::ScannedBudgetExceeded {
                        maximum: limits.max_scanned_records,
                    })
                    .into());
                }
                if let Some(maximum) = max_scanned_bytes {
                    let entry_bytes = u64::try_from(entry.key.len())
                        .ok()
                        .and_then(|key_bytes| {
                            u64::try_from(entry.value.len())
                                .ok()
                                .and_then(|value_bytes| key_bytes.checked_add(value_bytes))
                        })
                        .ok_or(BoundedEngineQueryError::ScannedByteBudgetExceeded { maximum })?;
                    scanned_bytes = scanned_bytes
                        .checked_add(entry_bytes)
                        .ok_or(BoundedEngineQueryError::ScannedByteBudgetExceeded { maximum })?;
                    if scanned_bytes > maximum {
                        return Err(BoundedEngineQueryError::ScannedByteBudgetExceeded { maximum });
                    }
                }
                records.push(Record {
                    key: entry.key,
                    value: decode_document(&entry.value).map_err(EngineError::from)?,
                });
            }
            let Some(next_after) = page.next_after else {
                break;
            };
            after = Some(next_after);
        }

        let elapsed = started.elapsed();
        let Some(timeout) = limits.timeout.checked_sub(elapsed) else {
            return Err(EngineError::from(QueryError::TimedOut).into());
        };
        if timeout.is_zero() {
            return Err(EngineError::from(QueryError::TimedOut).into());
        }
        let execution_limits = ExecutionLimits {
            timeout,
            ..limits.clone()
        };
        match max_scanned_bytes {
            Some(maximum) => {
                execute_with_byte_limit(&[records.as_slice()], query, &execution_limits, maximum)
                    .map_err(BoundedEngineQueryError::from)
            }
            None => execute(&[records.as_slice()], query, &execution_limits)
                .map_err(EngineError::from)
                .map_err(BoundedEngineQueryError::from),
        }
    }

    /// Executes one structured query and binds its complete logical result to
    /// a canonical snapshot witness at the same locked checkpoint.
    ///
    /// # Errors
    ///
    /// Returns any ordinary query error plus snapshot or proof creation
    /// failures. No proof is returned for a partial or failed query.
    pub fn query_with_proof(
        &self,
        query: &Query,
        limits: &ExecutionLimits,
    ) -> Result<ResultProofArtifact, EngineError> {
        let result = self.query(query, limits)?;
        let snapshot = self.snapshot()?;
        let proof = ResultProof::for_query(&snapshot, query.clone(), result)?;
        Ok(ResultProofArtifact { proof, snapshot })
    }

    /// Executes one query and creates its witness under the query's remaining
    /// end-to-end timeout and explicit maintenance bounds.
    ///
    /// # Errors
    ///
    /// Returns any ordinary query error plus snapshot-limit, timeout, or
    /// proof-creation failures.
    pub fn query_with_proof_with_limits(
        &self,
        query: &Query,
        limits: &ExecutionLimits,
        max_scanned_bytes: u64,
        maintenance: &MaintenanceLimits,
    ) -> Result<ResultProofArtifact, BoundedEngineQueryError> {
        let started = Instant::now();
        let result = self.query_with_byte_limit(query, limits, max_scanned_bytes)?;
        let maintenance = remaining_maintenance_limits(maintenance, started, limits.timeout)
            .map_err(BoundedEngineQueryError::from)?;
        let snapshot = self
            .snapshot_with_limits(&maintenance)
            .map_err(BoundedEngineQueryError::from)?;
        let proof = ResultProof::for_query(&snapshot, query.clone(), result)
            .map_err(EngineError::from)
            .map_err(BoundedEngineQueryError::from)?;
        ensure_total_timeout(started, limits.timeout).map_err(BoundedEngineQueryError::from)?;
        Ok(ResultProofArtifact { proof, snapshot })
    }

    /// Executes exact provider-neutral vector retrieval without persisting or
    /// producing embeddings.
    ///
    /// # Errors
    ///
    /// Returns vector, shape, duplicate-key, budget, or timeout errors.
    pub fn retrieve_vectors(
        shards: &[&[VectorRecord]],
        request: &RetrievalRequest,
        limits: &RetrievalLimits,
    ) -> Result<RetrievalOutcome, EngineError> {
        Ok(retrieve(shards, request, limits)?)
    }

    /// Defines one immutable durable named vector space.
    ///
    /// Repeating the identical definition is idempotent; changing dimension
    /// or metric for an existing name fails before the log append.
    ///
    /// # Errors
    ///
    /// Returns an idempotency, immutable-definition, or durable storage error.
    pub fn define_vector_space(
        &mut self,
        transaction_id: Uuid,
        definition: VectorSpaceDefinition,
    ) -> Result<AppendOutcome, EngineError> {
        Ok(self
            .storage
            .write(transaction_id, &[Mutation::define_vector_space(definition)])?)
    }

    /// Atomically stores vectors in one named space.
    ///
    /// # Errors
    ///
    /// Returns an error before append for duplicate keys, an unknown space,
    /// wrong dimensions, invalid keys, or invalid vectors.
    pub fn put_vectors(
        &mut self,
        transaction_id: Uuid,
        space: &VectorSpaceName,
        vectors: &[(Vec<u8>, Q15Vector)],
    ) -> Result<AppendOutcome, EngineError> {
        if vectors.is_empty() {
            return Err(EngineError::EmptyBatch);
        }
        let mut keys = BTreeSet::new();
        let mut mutations = Vec::with_capacity(vectors.len());
        for (key, vector) in vectors {
            if !keys.insert(key.as_slice()) {
                return Err(EngineError::DuplicateDocumentKey);
            }
            mutations.push(Mutation::upsert_vector(
                space.clone(),
                key.clone(),
                vector.clone(),
            ));
        }
        Ok(self.storage.write(transaction_id, &mutations)?)
    }

    /// Atomically deletes vectors from one named space.
    ///
    /// # Errors
    ///
    /// Returns an error before append for duplicate/invalid keys or an unknown
    /// vector space.
    pub fn delete_vectors(
        &mut self,
        transaction_id: Uuid,
        space: &VectorSpaceName,
        keys: &[&[u8]],
    ) -> Result<AppendOutcome, EngineError> {
        if keys.is_empty() {
            return Err(EngineError::EmptyBatch);
        }
        let mut unique = BTreeSet::new();
        let mut mutations = Vec::with_capacity(keys.len());
        for key in keys {
            if !unique.insert(*key) {
                return Err(EngineError::DuplicateDocumentKey);
            }
            mutations.push(Mutation::delete_vector(space.clone(), *key));
        }
        Ok(self.storage.write(transaction_id, &mutations)?)
    }

    /// Executes exact retrieval over the latest caught-up durable vector
    /// state. Storage budgets are enforced before returning candidates, then
    /// the canonical executor applies scoring and timeout policy.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown space, wrong query dimension, exhausted
    /// budget, timeout, stale storage, or malformed durable state. No partial
    /// ranking is returned.
    pub fn retrieve_exact(
        &self,
        request: &ExactRetrievalRequest,
        limits: &ExactRetrievalLimits,
    ) -> Result<ExactRetrievalOutcome, EngineError> {
        let started = Instant::now();
        let Some(definition) = self.storage.vector_space(&request.vector_space)? else {
            return Err(StorageError::from(
                hyphae_storage::MaterializedIndexError::UnknownVectorSpace {
                    name: request.vector_space.as_str().to_owned(),
                },
            )
            .into());
        };
        definition
            .validate_vector(&request.query)
            .map_err(|source| {
                StorageError::from(hyphae_storage::MaterializedIndexError::from(source))
            })?;
        let validation_limits = ExactRetrievalLimits {
            timeout: Duration::MAX,
            ..limits.clone()
        };
        retrieve_exact(&[], request, &validation_limits)?;
        let timeout = remaining_exact_timeout(started, limits.timeout)?;
        let candidates = match self.storage.vector_entries_with_timeout(
            &request.vector_space,
            limits.max_candidates,
            limits.max_candidate_bytes,
            timeout,
        ) {
            Ok(candidates) => candidates,
            Err(VectorEntriesError::ExactRetrieval(error)) => return Err(error.into()),
            Err(VectorEntriesError::Storage(StorageError::Index { source })) => match *source {
                hyphae_storage::MaterializedIndexError::VectorCandidateBudgetExceeded {
                    maximum,
                } => {
                    return Err(ExactRetrievalError::CandidateBudgetExceeded { maximum }.into());
                }
                hyphae_storage::MaterializedIndexError::VectorByteBudgetExceeded { maximum } => {
                    return Err(ExactRetrievalError::CandidateByteBudgetExceeded { maximum }.into());
                }
                source => {
                    return Err(StorageError::Index {
                        source: Box::new(source),
                    }
                    .into());
                }
            },
            Err(VectorEntriesError::Storage(error)) => return Err(error.into()),
        };
        let mut durable_candidates = Vec::with_capacity(candidates.len());
        for entry in candidates {
            remaining_exact_timeout(started, limits.timeout)?;
            durable_candidates.push(DurableVectorRecord {
                key: entry.key,
                vector: entry.vector,
            });
        }
        let execution_limits = ExactRetrievalLimits {
            timeout: remaining_exact_timeout(started, limits.timeout)?,
            ..limits.clone()
        };
        Ok(retrieve_exact(
            &durable_candidates,
            request,
            &execution_limits,
        )?)
    }

    /// Executes exact durable retrieval and binds its complete outcome to a
    /// canonical format-2 snapshot witness.
    ///
    /// # Errors
    ///
    /// Returns any exact-retrieval, snapshot, or retrieval-proof error. No
    /// proof is emitted for failed or partial execution.
    pub fn retrieve_exact_with_proof(
        &self,
        request: &ExactRetrievalRequest,
        limits: &ExactRetrievalLimits,
    ) -> Result<ExactRetrievalProofArtifact, EngineError> {
        let outcome = self.retrieve_exact(request, limits)?;
        let snapshot = self.snapshot()?;
        let proof = ExactRetrievalProof::new(&snapshot, request.clone(), outcome)?;
        Ok(ExactRetrievalProofArtifact { proof, snapshot })
    }

    /// Executes exact retrieval and creates its witness under the retrieval
    /// operation's remaining end-to-end timeout and explicit maintenance
    /// bounds.
    ///
    /// # Errors
    ///
    /// Returns any exact-retrieval error plus snapshot-limit, timeout, or
    /// proof-creation failures.
    pub fn retrieve_exact_with_proof_with_limits(
        &self,
        request: &ExactRetrievalRequest,
        limits: &ExactRetrievalLimits,
        maintenance: &MaintenanceLimits,
    ) -> Result<ExactRetrievalProofArtifact, EngineError> {
        let started = Instant::now();
        let outcome = self.retrieve_exact(request, limits)?;
        let maintenance = remaining_maintenance_limits(maintenance, started, limits.timeout)?;
        let snapshot = self.snapshot_with_limits(&maintenance)?;
        let proof = ExactRetrievalProof::new(&snapshot, request.clone(), outcome)?;
        ensure_total_timeout(started, limits.timeout)?;
        Ok(ExactRetrievalProofArtifact { proof, snapshot })
    }

    /// Defines one immutable provider-free lexical index.
    ///
    /// Repeating the identical definition is idempotent. Any change to an
    /// existing definition fails before the durable append.
    ///
    /// # Errors
    ///
    /// Returns an immutable-definition, idempotency, or storage error.
    pub fn define_lexical_index(
        &mut self,
        transaction_id: Uuid,
        definition: LexicalIndexDefinition,
    ) -> Result<AppendOutcome, EngineError> {
        Ok(self.storage.write(
            transaction_id,
            &[Mutation::define_lexical_index(definition)],
        )?)
    }

    /// Executes provider-free lexical retrieval from the rebuildable durable
    /// posting projection.
    ///
    /// Posting lookup, candidate materialization, and reference scoring share
    /// one lexical timeout and never return a partial ranking.
    ///
    /// # Errors
    ///
    /// Returns an unknown-index, document, budget, timeout, or storage error.
    pub fn retrieve_lexical(
        &self,
        request: &LexicalRequest,
        limits: &LexicalLimits,
    ) -> Result<LexicalOutcome, EngineError> {
        let started = Instant::now();
        let Some(definition) = self.storage.lexical_index(&request.index)? else {
            return Err(StorageError::from(
                hyphae_storage::MaterializedIndexError::UnknownLexicalIndex {
                    name: request.index.as_str().to_owned(),
                },
            )
            .into());
        };
        let query_tokens = tokenize_v1_checked(
            &request.query,
            || ensure_lexical_timeout(started, limits.timeout),
            || ensure_lexical_timeout(started, limits.timeout),
        )?
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
        if query_tokens.is_empty() {
            return Err(LexicalError::EmptyQuery.into());
        }
        if u64::try_from(query_tokens.len()).unwrap_or(u64::MAX) > limits.max_tokens {
            return Err(LexicalError::TokenBudgetExceeded {
                maximum: limits.max_tokens,
            }
            .into());
        }
        let timeout = remaining_lexical_timeout(started, limits.timeout)?;
        let corpus = match self.storage.lexical_corpus(
            &definition,
            &query_tokens,
            limits.max_candidates,
            timeout,
        ) {
            Ok(corpus) => corpus,
            Err(StorageError::Index { source }) => match *source {
                hyphae_storage::MaterializedIndexError::Lexical(error) => {
                    return Err(error.into());
                }
                source => {
                    return Err(StorageError::Index {
                        source: Box::new(source),
                    }
                    .into());
                }
            },
            Err(error) => return Err(error.into()),
        };
        let Some(timeout) = limits.timeout.checked_sub(started.elapsed()) else {
            return Err(LexicalError::TimedOut.into());
        };
        if timeout.is_zero() {
            return Err(LexicalError::TimedOut.into());
        }
        let execution_limits = LexicalLimits {
            timeout,
            ..limits.clone()
        };
        Ok(retrieve_lexical_materialized(
            &corpus,
            &definition,
            request,
            &execution_limits,
        )?)
    }

    /// Executes lexical retrieval and binds the complete outcome to a
    /// canonical format-2 snapshot witness.
    ///
    /// # Errors
    ///
    /// Returns any lexical, snapshot, or retrieval-proof error.
    pub fn retrieve_lexical_with_proof(
        &self,
        request: &LexicalRequest,
        limits: &LexicalLimits,
    ) -> Result<LexicalRetrievalProofArtifact, EngineError> {
        let outcome = self.retrieve_lexical(request, limits)?;
        let snapshot = self.snapshot()?;
        let proof = LexicalRetrievalProof::new(&snapshot, request.clone(), outcome)?;
        Ok(LexicalRetrievalProofArtifact { proof, snapshot })
    }

    /// Executes lexical retrieval and creates its witness under the
    /// retrieval operation's remaining timeout and explicit maintenance
    /// bounds.
    ///
    /// # Errors
    ///
    /// Returns any lexical error plus snapshot-limit, timeout, or
    /// proof-creation failures.
    pub fn retrieve_lexical_with_proof_with_limits(
        &self,
        request: &LexicalRequest,
        limits: &LexicalLimits,
        maintenance: &MaintenanceLimits,
    ) -> Result<LexicalRetrievalProofArtifact, EngineError> {
        let started = Instant::now();
        let outcome = self.retrieve_lexical(request, limits)?;
        let maintenance = remaining_maintenance_limits(maintenance, started, limits.timeout)?;
        let snapshot = self.snapshot_with_limits(&maintenance)?;
        let proof = LexicalRetrievalProof::new(&snapshot, request.clone(), outcome)?;
        ensure_total_timeout(started, limits.timeout)?;
        Ok(LexicalRetrievalProofArtifact { proof, snapshot })
    }

    /// Executes both durable branches and fuses their complete outcomes using
    /// deterministic RRF semantics.
    ///
    /// # Errors
    ///
    /// Returns any lexical, exact-vector, storage, budget, timeout, or fusion
    /// error. Branch failures never silently downgrade to single-modality
    /// success.
    pub fn retrieve_hybrid(
        &self,
        lexical_request: &LexicalRequest,
        lexical_limits: &LexicalLimits,
        vector_request: &ExactRetrievalRequest,
        vector_limits: &ExactRetrievalLimits,
        hybrid_request: &HybridRequest,
    ) -> Result<HybridOutcome, EngineError> {
        Ok(self
            .execute_hybrid(
                lexical_request,
                lexical_limits,
                vector_request,
                vector_limits,
                hybrid_request,
            )?
            .outcome)
    }

    /// Executes lexical and exact-vector branches, fuses their complete
    /// outcomes, and binds all three outcomes to one canonical snapshot.
    ///
    /// # Errors
    ///
    /// Returns any branch, fusion, snapshot, or retrieval-proof error.
    pub fn retrieve_hybrid_with_proof(
        &self,
        lexical_request: &LexicalRequest,
        lexical_limits: &LexicalLimits,
        vector_request: &ExactRetrievalRequest,
        vector_limits: &ExactRetrievalLimits,
        hybrid_request: &HybridRequest,
    ) -> Result<HybridRetrievalProofArtifact, EngineError> {
        let execution = self.execute_hybrid(
            lexical_request,
            lexical_limits,
            vector_request,
            vector_limits,
            hybrid_request,
        )?;
        let snapshot = self.snapshot()?;
        let proof = HybridRetrievalProof::new(
            &snapshot,
            lexical_request.clone(),
            execution.lexical,
            vector_request.clone(),
            execution.vector,
            hybrid_request.clone(),
            execution.outcome,
        )?;
        Ok(HybridRetrievalProofArtifact { proof, snapshot })
    }

    /// Executes both hybrid branches and creates their shared witness under
    /// one combined remaining branch timeout and explicit maintenance bounds.
    ///
    /// # Errors
    ///
    /// Returns any branch, fusion, snapshot-limit, timeout, or proof error.
    #[allow(clippy::too_many_arguments)]
    pub fn retrieve_hybrid_with_proof_with_limits(
        &self,
        lexical_request: &LexicalRequest,
        lexical_limits: &LexicalLimits,
        vector_request: &ExactRetrievalRequest,
        vector_limits: &ExactRetrievalLimits,
        hybrid_request: &HybridRequest,
        maintenance: &MaintenanceLimits,
    ) -> Result<HybridRetrievalProofArtifact, EngineError> {
        let execution = self.execute_hybrid(
            lexical_request,
            lexical_limits,
            vector_request,
            vector_limits,
            hybrid_request,
        )?;
        let maintenance =
            remaining_maintenance_limits(maintenance, execution.started, execution.total_timeout)?;
        let snapshot = self.snapshot_with_limits(&maintenance)?;
        let proof = HybridRetrievalProof::new(
            &snapshot,
            lexical_request.clone(),
            execution.lexical,
            vector_request.clone(),
            execution.vector,
            hybrid_request.clone(),
            execution.outcome,
        )?;
        ensure_total_timeout(execution.started, execution.total_timeout)?;
        Ok(HybridRetrievalProofArtifact { proof, snapshot })
    }

    fn execute_hybrid(
        &self,
        lexical_request: &LexicalRequest,
        lexical_limits: &LexicalLimits,
        vector_request: &ExactRetrievalRequest,
        vector_limits: &ExactRetrievalLimits,
        hybrid_request: &HybridRequest,
    ) -> Result<HybridExecution, EngineError> {
        let started = Instant::now();
        self.execute_hybrid_with_elapsed(
            lexical_request,
            lexical_limits,
            vector_request,
            vector_limits,
            hybrid_request,
            started,
            || started.elapsed(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_hybrid_with_elapsed(
        &self,
        lexical_request: &LexicalRequest,
        lexical_limits: &LexicalLimits,
        vector_request: &ExactRetrievalRequest,
        vector_limits: &ExactRetrievalLimits,
        hybrid_request: &HybridRequest,
        started: Instant,
        mut elapsed: impl FnMut() -> Duration,
    ) -> Result<HybridExecution, EngineError> {
        let total_timeout = lexical_limits
            .timeout
            .checked_add(vector_limits.timeout)
            .unwrap_or(Duration::MAX);

        let mut bounded_lexical = lexical_limits.clone();
        bounded_lexical.timeout = bounded_lexical
            .timeout
            .min(remaining_exact_timeout_after(total_timeout, elapsed())?);
        let lexical = self.retrieve_lexical(lexical_request, &bounded_lexical)?;

        let mut bounded_vector = vector_limits.clone();
        bounded_vector.timeout = bounded_vector
            .timeout
            .min(remaining_exact_timeout_after(total_timeout, elapsed())?);
        let vector = self.retrieve_exact(vector_request, &bounded_vector)?;

        let outcome = fuse_hybrid(&lexical, &vector, hybrid_request)?;
        remaining_exact_timeout_after(total_timeout, elapsed())?;
        Ok(HybridExecution {
            started,
            total_timeout,
            lexical,
            vector,
            outcome,
        })
    }

    /// Creates or reuses a verified logical snapshot.
    ///
    /// # Errors
    ///
    /// Returns a stale-handle, index, or snapshot error.
    pub fn snapshot(&self) -> Result<SnapshotInfo, EngineError> {
        Ok(self.storage.snapshot()?)
    }

    /// Creates or reuses a verified logical snapshot under explicit limits.
    ///
    /// # Errors
    ///
    /// Returns a limit, timeout, stale-handle, index, or snapshot error.
    pub fn snapshot_with_limits(
        &self,
        limits: &MaintenanceLimits,
    ) -> Result<SnapshotInfo, EngineError> {
        Ok(self.storage.snapshot_with_limits(limits)?)
    }

    /// Commits an anchored compaction generation.
    ///
    /// # Errors
    ///
    /// Returns a stale-handle, snapshot, segment, or manifest error.
    pub fn compact(&mut self) -> Result<CompactionOutcome, EngineError> {
        Ok(self.storage.compact()?)
    }

    /// Commits an anchored compaction generation under one finite deadline.
    ///
    /// # Errors
    ///
    /// Returns a limit, timeout, stale-handle, snapshot, segment, or manifest
    /// error before the manifest commit point.
    pub fn compact_with_limits(
        &mut self,
        limits: &MaintenanceLimits,
    ) -> Result<CompactionOutcome, EngineError> {
        Ok(self.storage.compact_with_limits(limits)?)
    }

    /// Creates an atomic portable backup at the locked logical checkpoint.
    ///
    /// # Errors
    ///
    /// Returns a snapshot, destination, synchronization, or promotion error.
    pub fn backup(&self, destination: impl AsRef<Path>) -> Result<BackupInfo, EngineError> {
        Ok(self.storage.backup(destination)?)
    }

    /// Verifies a portable backup without opening a live data directory.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed layout, metadata mismatch, or corrupt
    /// snapshot.
    pub fn verify_backup(path: impl AsRef<Path>) -> Result<BackupInfo, EngineError> {
        Ok(verify_backup(path)?)
    }

    /// Restores a backup to a new atomically activated data directory.
    ///
    /// # Errors
    ///
    /// Returns an error before destination activation if verification, index
    /// reconstruction, reopen, or filesystem synchronization fails.
    pub fn restore_backup(
        backup: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<RestoreInfo, EngineError> {
        Ok(restore_backup(backup, destination)?)
    }
}

fn remaining_exact_timeout(
    started: Instant,
    total_timeout: Duration,
) -> Result<Duration, ExactRetrievalError> {
    remaining_exact_timeout_after(total_timeout, started.elapsed())
}

fn remaining_exact_timeout_after(
    total_timeout: Duration,
    elapsed: Duration,
) -> Result<Duration, ExactRetrievalError> {
    total_timeout
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ExactRetrievalError::TimedOut)
}

fn ensure_lexical_timeout(started: Instant, total_timeout: Duration) -> Result<(), LexicalError> {
    remaining_lexical_timeout(started, total_timeout)?;
    Ok(())
}

fn remaining_lexical_timeout(
    started: Instant,
    total_timeout: Duration,
) -> Result<Duration, LexicalError> {
    total_timeout
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(LexicalError::TimedOut)
}

fn ensure_total_timeout(started: Instant, total_timeout: Duration) -> Result<(), EngineError> {
    if started.elapsed() >= total_timeout {
        Err(StorageError::from(SnapshotError::from(StorageLimitError::TimedOut)).into())
    } else {
        Ok(())
    }
}

fn remaining_maintenance_limits(
    template: &MaintenanceLimits,
    started: Instant,
    total_timeout: Duration,
) -> Result<MaintenanceLimits, EngineError> {
    let Some(remaining) = total_timeout.checked_sub(started.elapsed()) else {
        return Err(StorageError::from(SnapshotError::from(StorageLimitError::TimedOut)).into());
    };
    if remaining.is_zero() {
        return Err(StorageError::from(SnapshotError::from(StorageLimitError::TimedOut)).into());
    }
    Ok(MaintenanceLimits {
        timeout: template.timeout.min(remaining),
        snapshot: template.snapshot.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
    use hyphae_query::{
        AggregationPlan, CompareOperator, FieldPath, Filter, Metric, MetricValue, NamedMetric,
        NullPlacement, SortDirection, SortField, Value, encoded_document_len,
    };
    use uuid::Uuid;

    use hyphae_retrieval::{
        ExactRetrievalError, ExactRetrievalLimits, ExactRetrievalOutcome, ExactRetrievalRequest,
        HybridOutcome, HybridRequest, LexicalField, LexicalIndexDefinition, LexicalLimits,
        LexicalOutcome, LexicalRequest, retrieve_lexical,
    };

    use super::{
        BoundedEngineQueryError, EngineError, ExecutionLimits, HyphaeEngine, Query, Record,
    };

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> std::io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "hyphae-engine-{name}-{}-{}",
                std::process::id(),
                Uuid::now_v7()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.path);
        }
    }

    fn value(score: i64, group: &str) -> Value {
        Value::Object(BTreeMap::from([
            ("group".to_owned(), Value::String(group.to_owned())),
            ("score".to_owned(), Value::Integer(score)),
        ]))
    }

    #[test]
    fn durable_documents_query_identically_after_compaction_and_reopen()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("engine-query-reopen")?;
        let root = temporary.path().join("data");
        let mut opened = HyphaeEngine::open(&root)?;
        opened.engine.put_records(
            Uuid::now_v7(),
            &[
                Record::new(b"a", value(10, "x")),
                Record::new(b"b", value(8, "x")),
                Record::new(b"c", value(7, "y")),
                Record::new(b"d", value(2, "y")),
            ],
        )?;
        let request = Query {
            filter: Filter::Compare {
                path: FieldPath::field("score"),
                operator: CompareOperator::GreaterOrEqual,
                value: Value::Integer(7),
            },
            sort: vec![SortField {
                path: FieldPath::field("score"),
                direction: SortDirection::Descending,
                nulls: NullPlacement::Last,
            }],
            cursor: None,
            limit: 2,
            aggregation: Some(AggregationPlan {
                group_by: Vec::new(),
                metrics: vec![NamedMetric {
                    name: "count".to_owned(),
                    metric: Metric::Count,
                }],
            }),
        };
        let before = opened.engine.query(&request, &ExecutionLimits::default())?;
        assert_eq!(before.rows.len(), 2);
        assert_eq!(
            before
                .aggregation
                .as_ref()
                .map(|aggregation| { aggregation.groups[0].metrics[0].value.clone() }),
            Some(MetricValue::Count(3))
        );
        opened.engine.compact()?;
        drop(opened);

        let reopened = HyphaeEngine::open(&root)?;
        let after = reopened
            .engine
            .query(&request, &ExecutionLimits::default())?;
        assert_eq!(before, after);
        assert_eq!(
            reopened.engine.get_record(b"a")?.map(|record| record.value),
            Some(value(10, "x"))
        );
        Ok(())
    }

    #[test]
    fn facade_enforces_scan_budget_before_building_a_partial_page()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("engine-query-budget")?;
        let mut opened = HyphaeEngine::open(temporary.path().join("data"))?;
        opened.engine.put_records(
            Uuid::now_v7(),
            &[
                Record::new(b"a", Value::Null),
                Record::new(b"b", Value::Null),
            ],
        )?;
        let limits = ExecutionLimits {
            max_scanned_records: 1,
            ..ExecutionLimits::default()
        };
        let result = opened.engine.query(
            &Query {
                filter: Filter::MatchAll,
                sort: Vec::new(),
                cursor: None,
                limit: 1,
                aggregation: None,
            },
            &limits,
        );
        assert!(matches!(
            result,
            Err(EngineError::Query(
                hyphae_query::QueryError::ScannedBudgetExceeded { maximum: 1 }
            ))
        ));
        Ok(())
    }

    #[test]
    fn facade_enforces_query_scan_byte_budget_before_decode()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("engine-query-byte-budget")?;
        let mut opened = HyphaeEngine::open(temporary.path().join("data"))?;
        let records = [
            Record::new(b"a", Value::Null),
            Record::new(b"b", Value::String("bounded".to_owned())),
        ];
        let total_bytes = records.iter().try_fold(0_u64, |total, record| {
            let document = crate::encode_document(&record.value)?;
            let bytes = u64::try_from(record.key.len() + document.len())?;
            Ok::<_, Box<dyn std::error::Error>>(total.checked_add(bytes).ok_or("byte overflow")?)
        })?;
        opened.engine.put_records(Uuid::now_v7(), &records)?;
        let request = Query {
            filter: Filter::MatchAll,
            sort: Vec::new(),
            cursor: None,
            limit: 2,
            aggregation: None,
        };
        let result = opened.engine.query_with_byte_limit(
            &request,
            &ExecutionLimits::default(),
            total_bytes,
        )?;
        assert_eq!(result.rows, records);
        assert!(matches!(
            opened.engine.query_with_byte_limit(
                &request,
                &ExecutionLimits::default(),
                total_bytes - 1,
            ),
            Err(BoundedEngineQueryError::ScannedByteBudgetExceeded { maximum })
                if maximum == total_bytes - 1
        ));
        Ok(())
    }

    #[test]
    fn durable_scan_byte_budget_is_exact_across_storage_pages()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("engine-query-byte-pages")?;
        let mut opened = HyphaeEngine::open(temporary.path().join("data"))?;
        let records = (0_u64..4_097)
            .map(|value| Record::new(value.to_be_bytes(), Value::Null))
            .collect::<Vec<_>>();
        let total_bytes = records.iter().try_fold(0_u64, |total, record| {
            let key_bytes = u64::try_from(record.key.len())?;
            let document_bytes = u64::try_from(encoded_document_len(&record.value)?)?;
            Ok::<_, Box<dyn std::error::Error>>(
                total
                    .checked_add(key_bytes)
                    .and_then(|next| next.checked_add(document_bytes))
                    .ok_or_else(|| std::io::Error::other("scan byte total overflow"))?,
            )
        })?;
        opened.engine.put_records(Uuid::now_v7(), &records)?;
        let query = Query {
            filter: Filter::MatchAll,
            sort: Vec::new(),
            cursor: None,
            limit: 1,
            aggregation: None,
        };

        let exact = opened.engine.query_with_byte_limit(
            &query,
            &ExecutionLimits::default(),
            total_bytes,
        )?;
        assert_eq!(exact.rows.len(), 1);
        assert!(matches!(
            opened.engine.query_with_byte_limit(
                &query,
                &ExecutionLimits::default(),
                total_bytes - 1,
            ),
            Err(BoundedEngineQueryError::ScannedByteBudgetExceeded { maximum })
                if maximum == total_bytes - 1
        ));
        Ok(())
    }

    #[test]
    fn durable_vectors_survive_compaction_backup_restore_and_index_rebuild()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("durable-vectors-lifecycle")?;
        let root = temporary.path().join("data");
        let backup = temporary.path().join("backup");
        let restored = temporary.path().join("restored");
        let space = VectorSpaceName::new("semantic.v1")?;
        let definition = VectorSpaceDefinition::cosine(space.clone(), 3)?;
        let mut opened = HyphaeEngine::open(&root)?;
        opened
            .engine
            .define_vector_space(Uuid::now_v7(), definition.clone())?;
        opened.engine.put_vectors(
            Uuid::now_v7(),
            &space,
            &[
                (b"alpha".to_vec(), Q15Vector::new(vec![32_767, 0, 0])?),
                (b"beta".to_vec(), Q15Vector::new(vec![0, 32_767, 0])?),
            ],
        )?;
        let request = ExactRetrievalRequest {
            vector_space: space.clone(),
            query: Q15Vector::new(vec![32_767, 0, 0])?,
            limit: 2,
            minimum_score_nanos: -1_000_000_000,
            minimum_margin_nanos: 0,
        };
        let limits = ExactRetrievalLimits {
            max_candidates: 10,
            max_candidate_bytes: 64 * 1024,
            max_returned: 10,
            timeout: Duration::from_secs(1),
        };
        let expected = opened.engine.retrieve_exact(&request, &limits)?;
        assert!(matches!(
            &expected,
            ExactRetrievalOutcome::Matches { matches, .. }
                if matches.first().is_some_and(|matched| matched.key == b"alpha")
        ));
        let exact_candidate_bytes =
            u64::try_from(b"alpha".len() + b"beta".len() + (2 * 3 * std::mem::size_of::<i16>()))?;
        let exact_limits = ExactRetrievalLimits {
            max_candidate_bytes: exact_candidate_bytes,
            ..limits.clone()
        };
        assert_eq!(
            opened.engine.retrieve_exact(&request, &exact_limits)?,
            expected
        );
        let one_byte_short = ExactRetrievalLimits {
            max_candidate_bytes: exact_candidate_bytes - 1,
            ..limits.clone()
        };
        assert!(matches!(
            opened.engine.retrieve_exact(&request, &one_byte_short),
            Err(EngineError::ExactRetrieval(
                hyphae_retrieval::ExactRetrievalError::CandidateByteBudgetExceeded {
                    maximum
                }
            )) if maximum == exact_candidate_bytes - 1
        ));
        assert!(matches!(
            opened.engine.retrieve_exact(
                &request,
                &ExactRetrievalLimits {
                    timeout: Duration::ZERO,
                    ..limits.clone()
                }
            ),
            Err(EngineError::ExactRetrieval(
                hyphae_retrieval::ExactRetrievalError::TimedOut
            ))
        ));
        opened.engine.compact()?;
        assert_eq!(opened.engine.retrieve_exact(&request, &limits)?, expected);
        opened.engine.backup(&backup)?;
        drop(opened);

        let reopened = HyphaeEngine::open(&root)?;
        assert_eq!(reopened.engine.retrieve_exact(&request, &limits)?, expected);
        drop(reopened);
        fs::remove_file(root.join("indexes/primary.redb"))?;
        let rebuilt = HyphaeEngine::open(&root)?;
        assert_eq!(rebuilt.engine.retrieve_exact(&request, &limits)?, expected);
        drop(rebuilt);

        HyphaeEngine::restore_backup(&backup, &restored)?;
        let restored = HyphaeEngine::open(&restored)?;
        assert_eq!(restored.engine.retrieve_exact(&request, &limits)?, expected);
        Ok(())
    }

    #[test]
    fn mixed_validity_vector_batch_is_rejected_without_partial_visibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("vector-batch-rollback")?;
        let root = temporary.path().join("data");
        let space = VectorSpaceName::new("semantic")?;
        let mut opened = HyphaeEngine::open(&root)?;
        opened.engine.define_vector_space(
            Uuid::now_v7(),
            VectorSpaceDefinition::cosine(space.clone(), 2)?,
        )?;
        let result = opened.engine.put_vectors(
            Uuid::now_v7(),
            &space,
            &[
                (b"valid".to_vec(), Q15Vector::new(vec![32_767, 0])?),
                (b"wrong".to_vec(), Q15Vector::new(vec![32_767, 0, 0])?),
            ],
        );
        assert!(result.is_err());
        let request = ExactRetrievalRequest {
            vector_space: space,
            query: Q15Vector::new(vec![32_767, 0])?,
            limit: 10,
            minimum_score_nanos: -1_000_000_000,
            minimum_margin_nanos: 0,
        };
        assert!(matches!(
            opened
                .engine
                .retrieve_exact(&request, &ExactRetrievalLimits::default())?,
            ExactRetrievalOutcome::Abstained(_)
        ));
        Ok(())
    }

    fn lexical_value(title: &str, body: &str) -> Value {
        Value::Object(BTreeMap::from([
            ("body".to_owned(), Value::String(body.to_owned())),
            ("title".to_owned(), Value::String(title.to_owned())),
        ]))
    }

    #[test]
    fn hybrid_deadline_includes_fusion_after_both_branches()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("hybrid-total-deadline")?;
        let name = VectorSpaceName::new("documents.v1")?;
        let mut opened = HyphaeEngine::open(temporary.path().join("data"))?;
        opened.engine.define_lexical_index(
            Uuid::now_v7(),
            LexicalIndexDefinition::new(
                name.clone(),
                vec![LexicalField {
                    path: FieldPath::field("title"),
                    weight_micros: 1_000_000,
                }],
            )?,
        )?;
        opened.engine.define_vector_space(
            Uuid::now_v7(),
            VectorSpaceDefinition::cosine(name.clone(), 2)?,
        )?;
        let lexical_request = LexicalRequest {
            index: name.clone(),
            query: "durable".into(),
            limit: 1,
        };
        let lexical_limits = LexicalLimits {
            timeout: Duration::from_secs(5),
            ..LexicalLimits::default()
        };
        let vector_request = ExactRetrievalRequest {
            vector_space: name,
            query: Q15Vector::new(vec![32_767, 0])?,
            limit: 1,
            minimum_score_nanos: -1_000_000_000,
            minimum_margin_nanos: 0,
        };
        let vector_limits = ExactRetrievalLimits {
            timeout: Duration::from_secs(5),
            ..ExactRetrievalLimits::default()
        };
        let hybrid_request = HybridRequest {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 1,
        };
        let mut elapsed = [
            Duration::ZERO,
            Duration::from_secs(4),
            Duration::from_secs(10),
        ]
        .into_iter();
        let Err(error) = opened.engine.execute_hybrid_with_elapsed(
            &lexical_request,
            &lexical_limits,
            &vector_request,
            &vector_limits,
            &hybrid_request,
            Instant::now(),
            || elapsed.next().unwrap_or(Duration::MAX),
        ) else {
            return Err("fusion at the total deadline unexpectedly succeeded".into());
        };
        assert!(matches!(
            error,
            EngineError::ExactRetrieval(ExactRetrievalError::TimedOut)
        ));
        assert!(elapsed.next().is_none());
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lexical_and_hybrid_retrieval_survive_every_durable_lifecycle()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("lexical-hybrid-lifecycle")?;
        let root = temporary.path().join("data");
        let backup = temporary.path().join("backup");
        let restored = temporary.path().join("restored");
        let name = VectorSpaceName::new("documents.v1")?;
        let lexical_definition = LexicalIndexDefinition::new(
            name.clone(),
            vec![
                LexicalField {
                    path: FieldPath::field("body"),
                    weight_micros: 1_000_000,
                },
                LexicalField {
                    path: FieldPath::field("title"),
                    weight_micros: 2_000_000,
                },
            ],
        )?;
        let vector_definition = VectorSpaceDefinition::cosine(name.clone(), 2)?;
        let mut opened = HyphaeEngine::open(&root)?;
        opened.engine.put_records(
            Uuid::now_v7(),
            &[
                Record::new(b"alpha", lexical_value("Durable Rust", "offline engine")),
                Record::new(b"beta", lexical_value("Other", "durable storage")),
                Record::new(b"gamma", lexical_value("Unrelated", "nothing")),
            ],
        )?;
        opened
            .engine
            .define_lexical_index(Uuid::now_v7(), lexical_definition)?;
        opened
            .engine
            .define_vector_space(Uuid::now_v7(), vector_definition)?;
        opened.engine.put_vectors(
            Uuid::now_v7(),
            &name,
            &[
                (b"alpha".to_vec(), Q15Vector::new(vec![32_767, 0])?),
                (b"beta".to_vec(), Q15Vector::new(vec![30_000, 2_000])?),
                (b"gamma".to_vec(), Q15Vector::new(vec![0, 32_767])?),
            ],
        )?;
        let lexical_request = LexicalRequest {
            index: name.clone(),
            query: "durable".into(),
            limit: 3,
        };
        let lexical_limits = LexicalLimits {
            max_documents: 10,
            max_tokens: 100,
            max_candidates: 10,
            max_returned: 10,
            timeout: Duration::from_secs(2),
        };
        let vector_request = ExactRetrievalRequest {
            vector_space: name,
            query: Q15Vector::new(vec![32_767, 0])?,
            limit: 3,
            minimum_score_nanos: -1_000_000_000,
            minimum_margin_nanos: 0,
        };
        let vector_limits = ExactRetrievalLimits {
            max_candidates: 10,
            max_candidate_bytes: 64 * 1024,
            max_returned: 10,
            timeout: Duration::from_secs(2),
        };
        let hybrid_request = HybridRequest {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 3,
        };
        let expected_lexical = opened
            .engine
            .retrieve_lexical(&lexical_request, &lexical_limits)?;
        assert!(matches!(
            &expected_lexical,
            LexicalOutcome::Matches { matches, .. }
                if matches.first().is_some_and(|matched| matched.key == b"alpha")
        ));
        let expected_hybrid = opened.engine.retrieve_hybrid(
            &lexical_request,
            &lexical_limits,
            &vector_request,
            &vector_limits,
            &hybrid_request,
        )?;
        assert!(matches!(
            &expected_hybrid,
            HybridOutcome::Matches { matches, .. }
                if matches.first().is_some_and(|matched| matched.key == b"alpha")
        ));
        opened.engine.compact()?;
        assert_eq!(
            opened
                .engine
                .retrieve_lexical(&lexical_request, &lexical_limits)?,
            expected_lexical
        );
        opened.engine.backup(&backup)?;
        drop(opened);

        let reopened = HyphaeEngine::open(&root)?;
        assert_eq!(
            reopened.engine.retrieve_hybrid(
                &lexical_request,
                &lexical_limits,
                &vector_request,
                &vector_limits,
                &hybrid_request,
            )?,
            expected_hybrid
        );
        drop(reopened);
        fs::remove_file(root.join("indexes/primary.redb"))?;
        let rebuilt = HyphaeEngine::open(&root)?;
        assert_eq!(
            rebuilt
                .engine
                .retrieve_lexical(&lexical_request, &lexical_limits)?,
            expected_lexical
        );
        drop(rebuilt);

        HyphaeEngine::restore_backup(&backup, &restored)?;
        let restored = HyphaeEngine::open(&restored)?;
        assert_eq!(
            restored.engine.retrieve_hybrid(
                &lexical_request,
                &lexical_limits,
                &vector_request,
                &vector_limits,
                &hybrid_request,
            )?,
            expected_hybrid
        );
        assert_eq!(restored.engine.snapshot()?.lexical_index_count, 1);
        Ok(())
    }

    #[test]
    fn lexical_document_budget_returns_no_partial_ranking() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TestDirectory::new("lexical-budget")?;
        let name = VectorSpaceName::new("documents")?;
        let mut opened = HyphaeEngine::open(temporary.path().join("data"))?;
        opened.engine.put_records(
            Uuid::now_v7(),
            &[
                Record::new(b"a", lexical_value("one", "durable")),
                Record::new(b"b", lexical_value("two", "durable")),
            ],
        )?;
        opened.engine.define_lexical_index(
            Uuid::now_v7(),
            LexicalIndexDefinition::new(
                name.clone(),
                vec![LexicalField {
                    path: FieldPath::field("body"),
                    weight_micros: 1_000_000,
                }],
            )?,
        )?;
        let outcome = opened.engine.retrieve_lexical(
            &LexicalRequest {
                index: name,
                query: "durable".into(),
                limit: 2,
            },
            &LexicalLimits {
                max_documents: 1,
                ..LexicalLimits::default()
            },
        );
        assert!(matches!(
            outcome,
            Err(EngineError::Lexical(
                hyphae_retrieval::LexicalError::DocumentBudgetExceeded { maximum: 1 }
            ))
        ));
        Ok(())
    }

    #[test]
    fn lexical_materialization_timeout_returns_typed_timeout()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("lexical-timeout")?;
        let name = VectorSpaceName::new("documents.timeout")?;
        let mut opened = HyphaeEngine::open(temporary.path().join("data"))?;
        opened.engine.put_record(
            Uuid::now_v7(),
            &Record::new(b"a", lexical_value("one", "durable")),
        )?;
        opened.engine.define_lexical_index(
            Uuid::now_v7(),
            LexicalIndexDefinition::new(
                name.clone(),
                vec![LexicalField {
                    path: FieldPath::field("body"),
                    weight_micros: 1_000_000,
                }],
            )?,
        )?;

        let outcome = opened.engine.retrieve_lexical(
            &LexicalRequest {
                index: name,
                query: "durable".into(),
                limit: 1,
            },
            &LexicalLimits {
                timeout: Duration::ZERO,
                ..LexicalLimits::default()
            },
        );

        assert!(matches!(
            outcome,
            Err(EngineError::Lexical(
                hyphae_retrieval::LexicalError::TimedOut
            ))
        ));
        Ok(())
    }

    #[test]
    fn materialized_lexical_index_matches_reference_after_update_delete_and_rebuild()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::new("lexical-reference-equivalence")?;
        let root = temporary.path().join("data");
        let name = VectorSpaceName::new("documents.reference")?;
        let definition = LexicalIndexDefinition::new(
            name.clone(),
            vec![
                LexicalField {
                    path: FieldPath::field("body"),
                    weight_micros: 1_000_000,
                },
                LexicalField {
                    path: FieldPath::field("title"),
                    weight_micros: 2_000_000,
                },
            ],
        )?;
        let request = LexicalRequest {
            index: name,
            query: "durable rust engine".into(),
            limit: 10,
        };
        let limits = LexicalLimits {
            max_documents: 100,
            max_tokens: 10_000,
            max_candidates: 100,
            max_returned: 100,
            timeout: Duration::from_secs(2),
        };
        let mut records = vec![
            Record::new(
                b"alpha",
                lexical_value("Durable Rust", "offline engine durable durable"),
            ),
            Record::new(
                b"beta",
                lexical_value("Storage Engine", "rust transactions"),
            ),
            Record::new(b"gamma", lexical_value("Unrelated", "nothing relevant")),
            Record::new(
                b"delta",
                lexical_value("Rust Engine", "durable local search"),
            ),
        ];
        let mut opened = HyphaeEngine::open(&root)?;
        opened.engine.put_records(Uuid::now_v7(), &records)?;
        opened
            .engine
            .define_lexical_index(Uuid::now_v7(), definition.clone())?;

        let reference = retrieve_lexical(&records, &definition, &request, &limits)?;
        assert_eq!(
            opened.engine.retrieve_lexical(&request, &limits)?,
            reference
        );

        let updated = Record::new(
            b"gamma",
            lexical_value("Durable Engine", "rust rust offline"),
        );
        opened.engine.put_record(Uuid::now_v7(), &updated)?;
        records.retain(|record| record.key != b"gamma");
        records.push(updated);
        opened.engine.delete_record(Uuid::now_v7(), b"beta")?;
        records.retain(|record| record.key != b"beta");

        let updated_reference = retrieve_lexical(&records, &definition, &request, &limits)?;
        assert_eq!(
            opened.engine.retrieve_lexical(&request, &limits)?,
            updated_reference
        );
        drop(opened);

        fs::remove_file(root.join("indexes/primary.redb"))?;
        let rebuilt = HyphaeEngine::open(&root)?;
        assert_eq!(
            rebuilt.engine.retrieve_lexical(&request, &limits)?,
            updated_reference
        );
        Ok(())
    }
}
