// SPDX-License-Identifier: Apache-2.0

//! Curated embedded administration and typed explain models.

#![allow(clippy::missing_errors_doc)]
#![allow(clippy::result_large_err)]

use std::time::Instant;

use hyphae_native_runtime::{
    AnnRecallRisk, AnnSearchReceipt, AnnSearchStrategy, BlobCollectionReceipt, CheckpointReceipt,
    CommitReceipt, ConvergenceError, ConvergencePlan, ConvergenceStrategy, ExpirySweepReceipt,
    NativeHybridRequest, NativePhysicalObservation, NativeRuntimeError, NativeVectorBranch,
    PageGenerationCollectionReceipt, PageVacuumReceipt, SearchCompactionReceipt,
    StructureCompactionReceipt, WalRetentionReceipt,
};
use hyphae_native_types::DurabilityClass;

use crate::{
    BackupInfo, BackupPhase, BackupProductError, BackupRequest, MetricId, NativeProduct, ObjectId,
    ProductError, ProductErrorCode, ProductSnapshot, ProgressControl, SnapshotIdentity,
    TelemetryEvent, TelemetryEventKind, TimingClass,
};

/// Version of the bounded SQL plan-text strategy.
pub const SQL_PLAN_TEXT_VERSION: u16 = 1;
/// Maximum UTF-8 bytes returned by one SQL explain.
pub const MAX_SQL_PLAN_TEXT_BYTES: usize = 16 * 1024;
/// Product expiry sweep hard bound.
pub const MAX_ADMIN_EXPIRY_KEYS: usize = 4_096;

/// Product-owned durability selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductDurability {
    /// Acknowledge after physical synchronization.
    Strict,
    /// Use the runtime group durability class.
    Group,
    /// Publish without crash-durability acknowledgement.
    Memory,
}

impl From<ProductDurability> for DurabilityClass {
    fn from(value: ProductDurability) -> Self {
        match value {
            ProductDurability::Strict => Self::Strict,
            ProductDurability::Group => Self::Group,
            ProductDurability::Memory => Self::Memory,
        }
    }
}

impl From<DurabilityClass> for ProductDurability {
    fn from(value: DurabilityClass) -> Self {
        match value {
            DurabilityClass::Strict => Self::Strict,
            DurabilityClass::Group => Self::Group,
            DurabilityClass::Memory => Self::Memory,
        }
    }
}

/// Product-owned cross-engine commit evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductCommitReceipt {
    /// Stable non-guessable product resolution identity.
    pub transaction_id: crate::ProductTransactionId,
    /// Shared all-engine commit sequence.
    pub commit_csn: u64,
    /// Published catalog version.
    pub catalog_version: u64,
    /// Terminal WAL record byte position.
    pub commit_lsn: u64,
    /// Complete WAL block digest.
    pub wal_block_digest: [u8; 32],
    /// Acknowledgement durability.
    pub durability: ProductDurability,
    /// Transactions sharing the durability flush.
    pub durability_cohort_size: usize,
    /// This transaction's zero-based cohort position.
    pub durability_cohort_position: usize,
}

impl ProductCommitReceipt {
    pub(crate) fn from_runtime(
        value: CommitReceipt,
        transaction_id: crate::ProductTransactionId,
    ) -> Self {
        Self {
            transaction_id,
            commit_csn: value.commit_csn.get(),
            catalog_version: value.catalog_version.get(),
            commit_lsn: value.commit_lsn.get(),
            wal_block_digest: value.wal_block_digest,
            durability: value.durability.into(),
            durability_cohort_size: value.durability_cohort_size,
            durability_cohort_position: value.durability_cohort_position,
        }
    }
}

impl From<CommitReceipt> for ProductCommitReceipt {
    fn from(value: CommitReceipt) -> Self {
        Self::from_runtime(value, value.transaction_id.into())
    }
}

impl NativeProduct {
    pub(crate) fn observe_commit(&self, receipt: &CommitReceipt) {
        self.telemetry
            .record_timing(TimingClass::WalAppend, receipt.wal_append_time);
        self.telemetry.record_timing(
            TimingClass::PageSynchronization,
            receipt.page_synchronization_time,
        );
        self.telemetry.record_timing(
            TimingClass::WalSynchronization,
            receipt.wal_synchronization_time,
        );
        self.telemetry
            .record_timing(TimingClass::Durability, receipt.execution_time);
    }
}

/// Bounded embedded administration handle.
pub struct EmbeddedAdmin<'product> {
    product: &'product mut NativeProduct,
}

impl NativeProduct {
    /// Borrows the embedded administration surface.
    pub fn administration(&mut self) -> EmbeddedAdmin<'_> {
        EmbeddedAdmin { product: self }
    }
}

/// Bounded status request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusRequest {
    /// Logical time used to validate a current all-engine snapshot.
    pub logical_time_micros: i64,
}

/// Product-owned physical status counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductPhysicalObservation {
    /// Complete pages in the active generation.
    pub page_count: u64,
    /// Verified physical reads by this handle.
    pub physical_page_reads: u64,
    /// Current active WAL bytes.
    pub wal_bytes: u64,
    /// Process-wide complete state loads.
    pub process_full_state_loads: u64,
    /// Process-wide complete catalog loads.
    pub process_full_catalog_loads: u64,
}

impl From<NativePhysicalObservation> for ProductPhysicalObservation {
    fn from(value: NativePhysicalObservation) -> Self {
        Self {
            page_count: value.page_count,
            physical_page_reads: value.physical_page_reads,
            wal_bytes: value.wal_bytes,
            process_full_state_loads: value.process_full_state_loads,
            process_full_catalog_loads: value.process_full_catalog_loads,
        }
    }
}

/// Current embedded administration status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminStatus {
    /// Stable directory lineage and current root identity.
    pub snapshot: SnapshotIdentity,
    /// Verified durable snapshot pins.
    pub snapshot_pin_count: usize,
    /// Physical counters from the same open handle.
    pub physical: ProductPhysicalObservation,
    /// Retained WAL bytes verified during open or retention.
    pub retained_wal_bytes: u64,
    /// Replayed committed suffix transactions during open.
    pub replayed_transactions: usize,
    /// Verified immutable manifest count.
    pub manifest_count: usize,
    /// Verified immutable blob count.
    pub blob_count: usize,
}

/// Product checkpoint receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductCheckpointReceipt {
    /// Checkpoint transaction identity.
    pub transaction_id: u128,
    /// Captured all-engine CSN.
    pub visible_csn: u64,
    /// Immutable manifest generation.
    pub manifest_generation: u64,
    /// Complete immutable manifest digest.
    pub manifest_digest: [u8; 32],
    /// Checkpoint WAL record position.
    pub checkpoint_lsn: u64,
    /// Whether strict parent synchronization is supported.
    pub parent_directory_sync_supported: bool,
}

impl From<CheckpointReceipt> for ProductCheckpointReceipt {
    fn from(value: CheckpointReceipt) -> Self {
        Self {
            transaction_id: value.transaction_id.get(),
            visible_csn: value.visible_csn.get(),
            manifest_generation: value.manifest_generation.get(),
            manifest_digest: value.manifest_digest,
            checkpoint_lsn: value.checkpoint_lsn.get(),
            parent_directory_sync_supported: value.parent_directory_sync_supported,
        }
    }
}

/// Bounded active-expiry request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpiryRequest {
    /// Deterministic logical time.
    pub logical_time_micros: i64,
    /// Maximum due keys visited and tombstoned.
    pub max_keys: usize,
    /// Commit durability.
    pub durability: ProductDurability,
}

impl ExpiryRequest {
    /// Constructs a request within the product hard bound.
    pub const fn new(
        logical_time_micros: i64,
        max_keys: usize,
        durability: ProductDurability,
    ) -> Option<Self> {
        if max_keys > 0 && max_keys <= MAX_ADMIN_EXPIRY_KEYS {
            Some(Self {
                logical_time_micros,
                max_keys,
                durability,
            })
        } else {
            None
        }
    }
}

/// Product active-expiry receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductExpiryReceipt {
    /// Due identities tombstoned.
    pub expired_keys: usize,
    /// Whether another due identity was observed.
    pub more_due: bool,
    /// Commit evidence, absent for a no-op.
    pub commit: Option<ProductCommitReceipt>,
}

impl From<ExpirySweepReceipt> for ProductExpiryReceipt {
    fn from(value: ExpirySweepReceipt) -> Self {
        Self {
            expired_keys: value.expired_keys,
            more_due: value.more_due,
            commit: value.commit.map(Into::into),
        }
    }
}

/// Native compaction target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionTarget {
    /// Scalar and collection structure root.
    Structures,
    /// Lexical, document, and ANN search root.
    Search,
}

/// Bounded compaction request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionRequest {
    /// Root family to rebuild.
    pub target: CompactionTarget,
    /// Commit durability.
    pub durability: ProductDurability,
}

/// Product compaction evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductCompactionReceipt {
    /// Root family rebuilt.
    pub target: CompactionTarget,
    /// Physical entries inspected.
    pub scanned_entries: usize,
    /// Live entries retained.
    pub retained_entries: usize,
    /// Tombstones omitted.
    pub dropped_tombstones: usize,
    /// Reachable pages before compaction.
    pub reachable_pages_before: usize,
    /// Reachable pages after compaction.
    pub reachable_pages_after: usize,
    /// New pages appended.
    pub pages_appended: u64,
    /// Commit evidence, absent for a no-op.
    pub commit: Option<ProductCommitReceipt>,
}

/// Product page-vacuum evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductVacuumReceipt {
    /// Whether a smaller generation was selected.
    pub applied: bool,
    /// Prior page generation.
    pub previous_generation: u64,
    /// Resulting active page generation.
    pub active_generation: u64,
    /// Prior complete page count.
    pub previous_page_count: u64,
    /// Resulting complete page count.
    pub active_page_count: u64,
    /// Physically reclaimed pages.
    pub reclaimed_pages: u64,
    /// Commit evidence, absent for a no-op.
    pub commit: Option<ProductCommitReceipt>,
}

impl From<PageVacuumReceipt> for ProductVacuumReceipt {
    fn from(value: PageVacuumReceipt) -> Self {
        Self {
            applied: value.applied,
            previous_generation: value.previous_generation.get(),
            active_generation: value.active_generation.get(),
            previous_page_count: value.previous_page_count,
            active_page_count: value.active_page_count,
            reclaimed_pages: value.reclaimed_pages,
            commit: value.commit.map(Into::into),
        }
    }
}

/// Product retired-page generation collection evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductPageCollectionReceipt {
    /// Inactive unpinned files removed.
    pub removed_files: usize,
    /// Physical bytes removed.
    pub removed_bytes: u64,
    /// Active or pinned files retained.
    pub retained_files: usize,
    /// Physical bytes retained.
    pub retained_bytes: u64,
    /// Whether strict directory synchronization is supported.
    pub parent_directory_sync_supported: bool,
}

impl From<PageGenerationCollectionReceipt> for ProductPageCollectionReceipt {
    fn from(value: PageGenerationCollectionReceipt) -> Self {
        Self {
            removed_files: value.removed_files,
            removed_bytes: value.removed_bytes,
            retained_files: value.retained_files,
            retained_bytes: value.retained_bytes,
            parent_directory_sync_supported: value.parent_directory_sync_supported,
        }
    }
}

/// Product WAL-retention evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductWalRetentionReceipt {
    /// New stable retention-anchor epoch.
    pub anchor_epoch: u64,
    /// Base all-engine CSN.
    pub base_visible_csn: u64,
    /// Complete retired blocks.
    pub retired_wal_blocks: u64,
    /// Physical retired WAL bytes.
    pub retired_wal_bytes: u64,
    /// Complete new anchor digest.
    pub anchor_digest: [u8; 32],
    /// Complete operation duration.
    pub total_time_micros: u64,
}

impl From<WalRetentionReceipt> for ProductWalRetentionReceipt {
    fn from(value: WalRetentionReceipt) -> Self {
        Self {
            anchor_epoch: value.anchor_epoch,
            base_visible_csn: value.base_visible_csn.get(),
            retired_wal_blocks: value.retired_wal_blocks,
            retired_wal_bytes: value.retired_wal_bytes,
            anchor_digest: value.anchor_digest,
            total_time_micros: duration_micros(value.total_time),
        }
    }
}

/// Product immutable-blob collection evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductBlobCollectionReceipt {
    /// Sole retained root CSN.
    pub root_visible_csn: u64,
    /// Candidate files removed.
    pub removed_files: usize,
    /// Candidate bytes removed.
    pub removed_bytes: u64,
    /// Immutable files retained.
    pub retained_files: usize,
    /// Immutable bytes retained.
    pub retained_bytes: u64,
    /// Complete operation duration.
    pub total_time_micros: u64,
}

impl From<BlobCollectionReceipt> for ProductBlobCollectionReceipt {
    fn from(value: BlobCollectionReceipt) -> Self {
        Self {
            root_visible_csn: value.root_visible_csn.get(),
            removed_files: value.removed_files,
            removed_bytes: value.removed_bytes,
            retained_files: value.retained_files,
            retained_bytes: value.retained_bytes,
            total_time_micros: duration_micros(value.total_time),
        }
    }
}

/// Bounded ANN consolidation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnnConsolidationRequest {
    /// Vector index to consolidate.
    pub index: ObjectId,
    /// Maximum effective vectors captured.
    pub max_vectors: usize,
    /// Maximum delta records captured.
    pub max_delta_records: usize,
    /// Publication durability.
    pub durability: ProductDurability,
}

impl AnnConsolidationRequest {
    /// Constructs a request inside runtime hard bounds.
    pub const fn new(
        index: ObjectId,
        max_vectors: usize,
        max_delta_records: usize,
        durability: ProductDurability,
    ) -> Option<Self> {
        if max_vectors > 0
            && max_vectors <= hyphae_native_runtime::MAX_ANN_CONSOLIDATION_VECTORS
            && max_delta_records > 0
            && max_delta_records <= hyphae_native_runtime::MAX_ANN_DELTA_RECORDS
        {
            Some(Self {
                index,
                max_vectors,
                max_delta_records,
                durability,
            })
        } else {
            None
        }
    }
}

/// Product ANN consolidation evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductAnnConsolidationReceipt {
    /// Previously selected base identity.
    pub previous_base_identity: [u8; 32],
    /// Newly selected base identity.
    pub replacement_base_identity: [u8; 32],
    /// Captured delta records consumed.
    pub consumed_delta_records: usize,
    /// Effective vectors rebuilt.
    pub effective_vector_count: usize,
    /// Later delta records preserved.
    pub preserved_later_delta_records: usize,
    /// Atomic publication receipt.
    pub commit: ProductCommitReceipt,
}

/// Versioned bounded SQL text returned because runtime SQL plans are private.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlPlanText {
    /// Stable text-format version.
    pub version: u16,
    /// Opaque bounded plan text. Consumers must not parse it as product ABI.
    pub text: String,
    /// Immutable all-engine CSN visible while planning.
    pub visible_csn: Option<u64>,
    /// Catalog version used by the binder.
    pub catalog_version: u64,
    /// SQL explain performs planning only.
    pub executed: bool,
}

/// Product convergence strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductConvergenceStrategy {
    /// Scalar key lookup.
    ScalarLookup,
    /// Hash range.
    HashRange,
    /// Set range.
    SetRange,
    /// List range.
    ListRange,
    /// Sorted-set range.
    SortedSetRange,
    /// Stream range.
    StreamRange,
    /// Lexical top-k.
    LexicalTopK,
    /// Exact vector top-k.
    ExactVectorTopK,
    /// Approximate vector top-k.
    AnnTopK,
    /// Native reciprocal-rank hybrid fusion.
    HybridRrf,
}

impl From<ConvergenceStrategy> for ProductConvergenceStrategy {
    fn from(value: ConvergenceStrategy) -> Self {
        match value {
            ConvergenceStrategy::ScalarLookup => Self::ScalarLookup,
            ConvergenceStrategy::HashRange => Self::HashRange,
            ConvergenceStrategy::SetRange => Self::SetRange,
            ConvergenceStrategy::ListRange => Self::ListRange,
            ConvergenceStrategy::SortedSetRange => Self::SortedSetRange,
            ConvergenceStrategy::StreamRange => Self::StreamRange,
            ConvergenceStrategy::LexicalTopK => Self::LexicalTopK,
            ConvergenceStrategy::ExactVectorTopK => Self::ExactVectorTopK,
            ConvergenceStrategy::AnnTopK => Self::AnnTopK,
            ConvergenceStrategy::HybridRrf => Self::HybridRrf,
        }
    }
}

/// Product-owned convergence explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductConvergenceExplanation {
    /// Same-snapshot CSN used by all sources.
    pub snapshot_csn: Option<u64>,
    /// Physical source strategies in plan order.
    pub strategies: Vec<ProductConvergenceStrategy>,
    /// Whether execution joins by stable object identity.
    pub inner_join_by_object_id: bool,
    /// Whether output is stable object-ID order.
    pub stable_object_id_order: bool,
}

/// Product ANN strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductAnnStrategy {
    /// Bounded graph traversal.
    GraphTraversal,
    /// Bounded traversal followed by allowlist filtering.
    StableIdAllowlistPostFilter,
    /// Eligibility-aware traversal with connector nodes.
    StableIdEligibilityTraversal,
    /// Complete exact scoring over a restrictive admitted set.
    StableIdAdaptiveExact,
}

/// Product ANN recall qualification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductAnnRecallRisk {
    /// Ordinary approximate traversal risk.
    ApproximateTraversal,
    /// Post-filtering can omit allowed neighbors.
    PostFilterMayMissAllowedNeighbors,
    /// Filter-aware approximate traversal risk.
    FilteredApproximateTraversal,
    /// Complete exact scoring over every eligible vector.
    ExactFilteredCandidates,
}

/// Typed explanation copied directly from an ANN execution receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductAnnExplanation {
    /// Queried vector index.
    pub index: ObjectId,
    /// All-engine snapshot CSN.
    pub snapshot_csn: Option<u64>,
    /// Explicit approximation label.
    pub approximate: bool,
    /// Selected base-plus-delta identity.
    pub build_identity: [u8; 32],
    /// Effective graph breadth.
    pub ef_search: usize,
    /// Pre-truncation candidate count.
    pub candidate_count: usize,
    /// Candidates retained by filtering.
    pub eligible_candidate_count: usize,
    /// Physical strategy.
    pub strategy: ProductAnnStrategy,
    /// Honest recall qualification.
    pub recall_risk: ProductAnnRecallRisk,
    /// Whether candidates were exactly rescored.
    pub exact_reranked: bool,
    /// Distinct graph nodes visited.
    pub visited_nodes: usize,
}

/// Vector strategy selected by one hybrid request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductHybridVectorStrategy {
    /// Complete exact vector ranking.
    Exact,
    /// Bounded ANN traversal.
    Ann {
        /// Requested top-k.
        k: usize,
        /// Requested graph breadth.
        ef_search: usize,
        /// Optional exact-rerank count.
        exact_rerank: Option<usize>,
    },
}

/// Typed hybrid request explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductHybridExplanation {
    /// Lexical index.
    pub lexical_index: ObjectId,
    /// Maximum lexical branch candidates.
    pub lexical_limit: usize,
    /// Vector index.
    pub vector_index: ObjectId,
    /// Exact or ANN vector strategy.
    pub vector_strategy: ProductHybridVectorStrategy,
    /// Maximum vector branch candidates.
    pub vector_limit: usize,
    /// Lexical RRF weight.
    pub lexical_weight: u32,
    /// Vector RRF weight.
    pub vector_weight: u32,
    /// Maximum fused matches.
    pub fusion_limit: usize,
    /// Fixed RRF constant.
    pub rrf_constant: u64,
}

/// Closed typed explain result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductExplain {
    /// Versioned opaque SQL plan text.
    SqlPlanText(SqlPlanText),
    /// Typed relation-valued convergence plan.
    Convergence(ProductConvergenceExplanation),
    /// Typed ANN receipt explanation.
    Ann(ProductAnnExplanation),
    /// Typed hybrid request explanation.
    Hybrid(ProductHybridExplanation),
}

impl EmbeddedAdmin<'_> {
    /// Captures current logical and physical status.
    ///
    /// # Errors
    ///
    /// Returns a stable product error for snapshot or physical observation failure.
    pub fn status(&self, request: StatusRequest) -> Result<AdminStatus, ProductError> {
        let snapshot = self
            .product
            .database
            .catalog_snapshot()
            .map_err(ProductError::from)?;
        let snapshot = snapshot.identity();
        let physical = self
            .product
            .database
            .physical_observation()
            .map_err(ProductError::from)?;
        let recovery = self.product.database.recovery_report();
        Ok(AdminStatus {
            snapshot: SnapshotIdentity {
                directory_lineage: self
                    .product
                    .database
                    .directory_identity()
                    .lineage()
                    .encode(),
                visible_csn: snapshot.visible_csn,
                catalog_version: snapshot.catalog_version,
                root_digest: snapshot.root_digest,
                logical_time_micros: request.logical_time_micros,
            },
            snapshot_pin_count: self.product.database.snapshot_pin_count(),
            physical: physical.into(),
            retained_wal_bytes: recovery.retained_wal_bytes,
            replayed_transactions: recovery.replayed_transactions,
            manifest_count: recovery.manifest_count,
            blob_count: recovery.blob_count,
        })
    }

    /// Publishes one synchronized root checkpoint.
    ///
    /// # Errors
    ///
    /// Returns a stable error when no committed root exists or durability fails.
    pub fn checkpoint(&mut self) -> Result<ProductCheckpointReceipt, ProductError> {
        self.product.telemetry.increment(MetricId::Checkpoints, 1);
        let started = Instant::now();
        let result = self.product.database.checkpoint().map(Into::into);
        let elapsed = started.elapsed();
        self.product
            .telemetry
            .record_timing(TimingClass::EngineExecution, elapsed);
        self.product
            .telemetry
            .record_timing(TimingClass::WalAppend, elapsed);
        self.product
            .telemetry
            .record_timing(TimingClass::WalSynchronization, elapsed);
        self.product
            .telemetry
            .record_timing(TimingClass::Durability, elapsed);
        self.finish_timed(result)
    }

    /// Tombstones a bounded number of due structure identities.
    ///
    /// # Errors
    ///
    /// Returns a stable storage, corruption, or durability error.
    pub fn expire_due(
        &mut self,
        request: ExpiryRequest,
    ) -> Result<ProductExpiryReceipt, ProductError> {
        self.product.telemetry.increment(MetricId::ActiveExpiry, 1);
        self.timed(|database| {
            database
                .expire_due_structures(
                    request.logical_time_micros,
                    request.max_keys,
                    request.durability.into(),
                )
                .map(ProductExpiryReceipt::from)
        })
    }

    /// Compacts one current native root family.
    ///
    /// # Errors
    ///
    /// Returns a stable request, storage, corruption, or durability error.
    pub fn compact(
        &mut self,
        request: CompactionRequest,
    ) -> Result<ProductCompactionReceipt, ProductError> {
        self.product.telemetry.increment(MetricId::Compactions, 1);
        self.timed(|database| match request.target {
            CompactionTarget::Structures => database
                .compact_structure(request.durability.into())
                .map(structure_compaction),
            CompactionTarget::Search => database
                .compact_search(request.durability.into())
                .map(search_compaction),
        })
    }

    /// Rebuilds the current logical roots into a smaller page generation.
    ///
    /// # Errors
    ///
    /// Returns a stable storage, corruption, or durability error.
    pub fn vacuum_pages(&mut self) -> Result<ProductVacuumReceipt, ProductError> {
        self.product.telemetry.increment(MetricId::Vacuums, 1);
        self.timed(|database| database.vacuum_pages().map(ProductVacuumReceipt::from))
    }

    /// Removes inactive, unpinned page-generation files.
    ///
    /// # Errors
    ///
    /// Returns a stable filesystem or durable-state error.
    pub fn collect_retired_page_generations(
        &mut self,
    ) -> Result<ProductPageCollectionReceipt, ProductError> {
        self.timed(|database| database.collect_retired_page_generations().map(Into::into))
    }

    /// Retires the WAL prefix through an eligible retention checkpoint.
    ///
    /// # Errors
    ///
    /// Returns a stable request, filesystem, or durable-state error.
    pub fn retain_wal(&mut self) -> Result<ProductWalRetentionReceipt, ProductError> {
        self.product.telemetry.increment(MetricId::WalRetentions, 1);
        let started = Instant::now();
        let result = self.timed(|database| {
            database
                .truncate_wal_at_retention_checkpoint()
                .map(Into::into)
        });
        if result.is_ok() {
            self.product
                .telemetry
                .record_timing(TimingClass::WalSynchronization, started.elapsed());
        }
        result
    }

    /// Collects immutable blobs unreachable from the sole retained root.
    ///
    /// # Errors
    ///
    /// Returns a stable request, filesystem, or durable-state error.
    pub fn collect_blobs(&mut self) -> Result<ProductBlobCollectionReceipt, ProductError> {
        self.product
            .telemetry
            .increment(MetricId::BlobCollections, 1);
        let started = Instant::now();
        let result = self.timed(|database| database.collect_blobs().map(Into::into));
        if result.is_ok() {
            self.product
                .telemetry
                .record_timing(TimingClass::PageSynchronization, started.elapsed());
        }
        result
    }

    /// Captures, builds, and atomically publishes one bounded ANN consolidation.
    ///
    /// # Errors
    ///
    /// Returns a stable request, limit, storage, or corruption error.
    pub fn consolidate_ann(
        &mut self,
        request: AnnConsolidationRequest,
    ) -> Result<ProductAnnConsolidationReceipt, ProductError> {
        self.product
            .telemetry
            .increment(MetricId::AnnConsolidations, 1);
        self.timed(|database| {
            let plan = database.plan_ann_consolidation(
                request.index,
                request.max_vectors,
                request.max_delta_records,
            )?;
            let receipt = database.consolidate_ann(plan, request.durability.into())?;
            Ok(ProductAnnConsolidationReceipt {
                previous_base_identity: receipt.previous_base_identity,
                replacement_base_identity: receipt.replacement_base_identity,
                consumed_delta_records: receipt.consumed_delta_records,
                effective_vector_count: receipt.effective_vector_count,
                preserved_later_delta_records: receipt.preserved_later_delta_records,
                commit: receipt.commit.into(),
            })
        })
    }

    /// Creates and verifies a promoted native backup.
    ///
    /// # Errors
    ///
    /// Returns typed validation, cancellation, backup, or verification errors.
    pub fn backup(
        &mut self,
        request: &BackupRequest,
        progress: impl FnMut(BackupPhase) -> ProgressControl,
    ) -> Result<BackupInfo, BackupProductError> {
        self.product.telemetry.increment(MetricId::Backups, 1);
        self.product.telemetry.record_event(TelemetryEvent {
            captured_at_micros: 0,
            kind: TelemetryEventKind::Backup,
        });
        let started = Instant::now();
        let result = crate::backup::backup(&mut self.product.database, request, progress);
        self.product
            .telemetry
            .record_timing(TimingClass::EngineExecution, started.elapsed());
        result
    }

    /// Verifies and restores a native backup to a new path, then runs doctor.
    ///
    /// # Errors
    ///
    /// Returns typed validation, cancellation, restore, or doctor errors.
    pub fn restore(
        &mut self,
        request: &crate::RestoreRequest,
        progress: impl FnMut(crate::RestorePhase) -> ProgressControl,
    ) -> Result<crate::RestoreInfo, BackupProductError> {
        self.product.telemetry.increment(MetricId::Restores, 1);
        self.product.telemetry.record_event(TelemetryEvent {
            captured_at_micros: request.doctor_logical_time_micros,
            kind: TelemetryEventKind::Restore,
        });
        let started = Instant::now();
        let result = crate::backup::restore(request, progress);
        self.product
            .telemetry
            .record_timing(TimingClass::EngineExecution, started.elapsed());
        result
    }

    /// Executes runtime SQL `EXPLAIN` and wraps its private-plan output as
    /// bounded, explicitly versioned opaque text without parsing it.
    ///
    /// # Errors
    ///
    /// Returns a stable SQL, limit, or durable-state error.
    pub fn explain_sql(&mut self, statement: &str) -> Result<ProductExplain, ProductError> {
        if statement.len() > crate::MAX_PRODUCT_SQL_STATEMENT_BYTES {
            return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
        }
        let explained = format!("EXPLAIN {statement}");
        let identity = self.product.snapshot_bounded(0)?.identity();
        let started = Instant::now();
        let mut transaction = self
            .product
            .database
            .begin_sql(0, DurabilityClass::Memory)
            .map_err(ProductError::from)?;
        let result = transaction.execute_sql(&explained, &[]);
        transaction.rollback();
        self.product
            .telemetry
            .record_timing(TimingClass::Planning, started.elapsed());
        let result = result.map_err(ProductError::from)?;
        let hyphae_native_runtime::SqlResult::Rows { columns, rows } = result else {
            return Err(ProductError::from_code(ProductErrorCode::Internal));
        };
        let [column] = columns.as_slice() else {
            return Err(ProductError::from_code(ProductErrorCode::Internal));
        };
        let [row] = rows.as_slice() else {
            return Err(ProductError::from_code(ProductErrorCode::Internal));
        };
        let [crate::ProductValue::Text(text)] = row.as_slice() else {
            return Err(ProductError::from_code(ProductErrorCode::Internal));
        };
        if column != "plan" || text.len() > MAX_SQL_PLAN_TEXT_BYTES {
            return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
        }
        Ok(ProductExplain::SqlPlanText(SqlPlanText {
            version: SQL_PLAN_TEXT_VERSION,
            text: text.clone(),
            visible_csn: identity.visible_csn.map(hyphae_native_types::Csn::get),
            catalog_version: identity.catalog_version.get(),
            executed: false,
        }))
    }

    pub(crate) fn explain_bound_sql(
        &mut self,
        bound: &hyphae_native_runtime::BoundSqlStatement,
        statement: &str,
    ) -> Result<ProductExplain, ProductError> {
        if statement.len() > crate::MAX_PRODUCT_SQL_STATEMENT_BYTES {
            return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
        }
        let identity = self.product.snapshot_bounded(0)?.identity();
        if identity.catalog_version != bound.catalog_version() {
            return Err(hyphae_native_runtime::SqlError::CatalogChanged.into());
        }
        let started = Instant::now();
        let mut transaction = self
            .product
            .database
            .begin_sql(0, DurabilityClass::Memory)
            .map_err(ProductError::from)?;
        let result = transaction.execute_sql(&format!("EXPLAIN {statement}"), &[]);
        transaction.rollback();
        self.product
            .telemetry
            .record_timing(TimingClass::Planning, started.elapsed());
        if let Some(prepared) = bound.prepared_statement() {
            let current = self
                .product
                .database
                .prepare_sql_latest(statement)
                .map_err(ProductError::from)?;
            if current != *prepared {
                return Err(hyphae_native_runtime::SqlError::CatalogChanged.into());
            }
        }
        sql_plan_text(result.map_err(ProductError::from)?, identity)
    }

    fn timed<T>(
        &mut self,
        operation: impl FnOnce(
            &mut hyphae_native_runtime::NativeDatabase,
        ) -> Result<T, NativeRuntimeError>,
    ) -> Result<T, ProductError> {
        let started = Instant::now();
        let result = operation(&mut self.product.database).map_err(ProductError::from);
        self.product
            .telemetry
            .record_timing(TimingClass::EngineExecution, started.elapsed());
        if let Err(error) = &result {
            self.product.telemetry.increment(MetricId::Errors, 1);
            self.product.telemetry.record_event(TelemetryEvent {
                captured_at_micros: 0,
                kind: TelemetryEventKind::Error(error.category()),
            });
        }
        result
    }

    fn finish_timed<T>(
        &mut self,
        result: Result<T, NativeRuntimeError>,
    ) -> Result<T, ProductError> {
        let result = result.map_err(ProductError::from);
        if let Err(error) = &result {
            self.product.telemetry.increment(MetricId::Errors, 1);
            self.product.telemetry.record_event(TelemetryEvent {
                captured_at_micros: 0,
                kind: TelemetryEventKind::Error(error.category()),
            });
        }
        result
    }
}

fn sql_plan_text(
    result: hyphae_native_runtime::SqlResult,
    identity: SnapshotIdentity,
) -> Result<ProductExplain, ProductError> {
    let hyphae_native_runtime::SqlResult::Rows { columns, rows } = result else {
        return Err(ProductError::from_code(ProductErrorCode::Internal));
    };
    let [column] = columns.as_slice() else {
        return Err(ProductError::from_code(ProductErrorCode::Internal));
    };
    let [row] = rows.as_slice() else {
        return Err(ProductError::from_code(ProductErrorCode::Internal));
    };
    let [crate::ProductValue::Text(text)] = row.as_slice() else {
        return Err(ProductError::from_code(ProductErrorCode::Internal));
    };
    if column != "plan" || text.len() > MAX_SQL_PLAN_TEXT_BYTES {
        return Err(ProductError::from_code(ProductErrorCode::LimitExceeded));
    }
    Ok(ProductExplain::SqlPlanText(SqlPlanText {
        version: SQL_PLAN_TEXT_VERSION,
        text: text.clone(),
        visible_csn: identity.visible_csn.map(hyphae_native_types::Csn::get),
        catalog_version: identity.catalog_version.get(),
        executed: false,
    }))
}

impl NativeProduct {
    /// Captures this instance's telemetry with current catalog identity.
    ///
    /// # Errors
    ///
    /// Returns a stable product error if the current catalog snapshot cannot be read.
    pub fn telemetry_snapshot(
        &self,
        captured_at_micros: i64,
    ) -> Result<crate::TelemetrySnapshot, ProductError> {
        let catalog_version = self
            .database
            .snapshot(captured_at_micros)
            .map_err(ProductError::from)?
            .catalog_version();
        Ok(self
            .telemetry
            .snapshot(captured_at_micros, Some(catalog_version)))
    }
}

impl ProductSnapshot {
    /// Explains one typed convergence plan without reading source data.
    ///
    /// # Errors
    ///
    /// Returns a stable request, limit, or runtime error for an invalid plan.
    pub fn explain_convergence(
        &self,
        plan: &ConvergencePlan,
    ) -> Result<ProductExplain, ProductError> {
        let explanation = self
            .inner
            .explain_convergence(plan)
            .map_err(map_convergence_error)?;
        Ok(ProductExplain::Convergence(ProductConvergenceExplanation {
            snapshot_csn: explanation.snapshot_csn.map(hyphae_native_types::Csn::get),
            strategies: explanation.strategies.into_iter().map(Into::into).collect(),
            inner_join_by_object_id: explanation.inner_join_by_object_id,
            stable_object_id_order: explanation.stable_object_id_order,
        }))
    }
}

/// Wraps an executed ANN receipt as a typed product explanation.
pub fn explain_ann(receipt: &AnnSearchReceipt) -> ProductExplain {
    ProductExplain::Ann(ProductAnnExplanation {
        index: receipt.index_id,
        snapshot_csn: receipt.snapshot_csn.map(hyphae_native_types::Csn::get),
        approximate: receipt.approximate,
        build_identity: receipt.build_identity,
        ef_search: receipt.ef_search,
        candidate_count: receipt.candidate_count,
        eligible_candidate_count: receipt.eligible_candidate_count,
        strategy: match receipt.strategy {
            AnnSearchStrategy::GraphTraversal => ProductAnnStrategy::GraphTraversal,
            AnnSearchStrategy::StableIdEligibilityTraversal => {
                ProductAnnStrategy::StableIdEligibilityTraversal
            }
            AnnSearchStrategy::StableIdAdaptiveExact => ProductAnnStrategy::StableIdAdaptiveExact,
        },
        recall_risk: match receipt.recall_risk {
            AnnRecallRisk::ApproximateTraversal => ProductAnnRecallRisk::ApproximateTraversal,
            AnnRecallRisk::FilteredApproximateTraversal => {
                ProductAnnRecallRisk::FilteredApproximateTraversal
            }
            AnnRecallRisk::ExactFilteredCandidates => ProductAnnRecallRisk::ExactFilteredCandidates,
        },
        exact_reranked: receipt.exact_reranked,
        visited_nodes: receipt.visited_nodes,
    })
}

/// Wraps one existing typed hybrid request without executing or parsing text.
pub fn explain_hybrid(request: &NativeHybridRequest<'_>) -> ProductExplain {
    let vector_strategy = match request.vector_branch {
        NativeVectorBranch::Exact => ProductHybridVectorStrategy::Exact,
        NativeVectorBranch::Ann(options) => ProductHybridVectorStrategy::Ann {
            k: options.k(),
            ef_search: options.ef_search(),
            exact_rerank: options.exact_rerank(),
        },
    };
    ProductExplain::Hybrid(ProductHybridExplanation {
        lexical_index: request.lexical_index,
        lexical_limit: request.lexical_limit,
        vector_index: request.vector_index,
        vector_strategy,
        vector_limit: request.vector_limit,
        lexical_weight: request.fusion.lexical_weight,
        vector_weight: request.fusion.vector_weight,
        fusion_limit: request.fusion.limit,
        rrf_constant: hyphae_native_runtime::NATIVE_HYBRID_RRF_CONSTANT,
    })
}

fn structure_compaction(value: StructureCompactionReceipt) -> ProductCompactionReceipt {
    ProductCompactionReceipt {
        target: CompactionTarget::Structures,
        scanned_entries: value.scanned_entries,
        retained_entries: value.retained_entries,
        dropped_tombstones: value.dropped_tombstones,
        reachable_pages_before: value.reachable_pages_before,
        reachable_pages_after: value.reachable_pages_after,
        pages_appended: value.pages_appended,
        commit: value.commit.map(Into::into),
    }
}

fn search_compaction(value: SearchCompactionReceipt) -> ProductCompactionReceipt {
    ProductCompactionReceipt {
        target: CompactionTarget::Search,
        scanned_entries: value.scanned_entries,
        retained_entries: value.retained_entries,
        dropped_tombstones: value.dropped_tombstones,
        reachable_pages_before: value.reachable_pages_before,
        reachable_pages_after: value.reachable_pages_after,
        pages_appended: value.pages_appended,
        commit: value.commit.map(Into::into),
    }
}

fn map_convergence_error(error: ConvergenceError) -> ProductError {
    match error {
        ConvergenceError::InvalidLimits
        | ConvergenceError::EmptyPlan
        | ConvergenceError::InvalidAggregate
        | ConvergenceError::InvalidObjectId
        | ConvergenceError::DuplicateObjectId
        | ConvergenceError::InvalidNumber
        | ConvergenceError::NumericOverflow => {
            ProductError::from_code(ProductErrorCode::InvalidRequest)
        }
        ConvergenceError::PlanLimitExceeded
        | ConvergenceError::SourceRowLimitExceeded
        | ConvergenceError::OutputRowLimitExceeded => {
            ProductError::from_code(ProductErrorCode::LimitExceeded)
        }
        ConvergenceError::Runtime(error) => error.into(),
        ConvergenceError::Hybrid(hybrid) => match hybrid {
            hyphae_native_runtime::NativeHybridError::Runtime(error) => error.into(),
            _ => ProductError::from_code(ProductErrorCode::InvalidRequest),
        },
    }
}

fn duration_micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
