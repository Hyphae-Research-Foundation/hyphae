// SPDX-License-Identifier: Apache-2.0

//! Embedded deterministic RRF over one immutable native snapshot.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use hyphae_native_btree::BTREE_MAX_KEY_SIZE;
use hyphae_native_types::{CatalogVersion, Csn, ObjectId};
use thiserror::Error;

use crate::{
    AnnSearchOptions, AnnSearchReceipt, NativeAnnReadView, NativeAnnReadViewOpenReceipt,
    NativeAnnReadViewQueryReceipt, NativeDatabase, NativeEngineWorkReceipt, NativeLexicalReadState,
    NativeRuntimeError, NativeSnapshot, NativeStructureFilterState, Vector, VectorHit,
    WorkloadClass, bm25_term_score, decode_live_search_posting, decode_search_document_header,
    rank_match_hits, retained_structure_filter_matches, search_count_f64,
};

/// Fixed reciprocal-rank fusion constant.
pub const NATIVE_HYBRID_RRF_CONSTANT: u64 = 60;
const CONTRIBUTION_SCALE: u64 = 1_000_000_000;
const MAX_WEIGHT: u32 = 1_000_000;
const LEXICAL_SCORE_ENTRY_BYTES: u64 = BTREE_MAX_KEY_SIZE as u64 + 256;
const LEXICAL_FIXED_QUERY_BYTES: u64 = 64 * 1_024;
const HYBRID_FUSION_ENTRY_BYTES: u64 = 512;
const HYBRID_RESULT_FIXED_BYTES: u64 = 64 * 1_024;
/// Maximum candidates admitted from either native branch.
pub const MAX_NATIVE_HYBRID_BRANCH_HITS: usize = 10_000;
/// Maximum fused matches returned by one embedded request.
pub const MAX_NATIVE_HYBRID_RETURNED: usize = 1_000;
/// Frozen scope of the retained lexical authority.
pub const NATIVE_LEXICAL_READ_VIEW_PLAN_SCOPE: &str = "query-bound-encoded-postings-v1";
/// Frozen execution work repeated for every lexical observation.
pub const NATIVE_LEXICAL_READ_VIEW_EXECUTION: &str = "decode-bm25-rank-per-observation-v1";
/// Frozen domain for identities over one physical lexical index root.
pub const NATIVE_LEXICAL_INDEX_IDENTITY_ALGORITHM: &str =
    "blake3-search-root-page-object-format-v1";
/// Frozen domain for one query-bound physical structure filter identity.
pub const NATIVE_STRUCTURE_FILTER_IDENTITY_ALGORITHM: &str =
    "blake3-structure-root-key-prefix-value-time-v1";
/// Frozen per-observation structure filter work.
pub const NATIVE_STRUCTURE_FILTER_EXECUTION: &str =
    "decode-expiry-inline-value-filter-before-rank-v1";
/// Frozen boundary: retained structure predicates accept inline scalars only.
pub const NATIVE_STRUCTURE_FILTER_VALUE_SCOPE: &str = "inline-scalar-only-v1";

/// Native vector branch selected for hybrid execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeVectorBranch {
    /// Complete exact ranking over the vector generation.
    Exact,
    /// HNSW traversal with its explicit native search options.
    Ann(AnnSearchOptions),
}

/// Complete native fusion settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHybridFusion {
    /// Positive lexical branch weight.
    pub lexical_weight: u32,
    /// Positive vector branch weight.
    pub vector_weight: u32,
    /// Maximum returned fused matches.
    pub limit: usize,
}

/// Complete embedded hybrid request.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridRequest<'request> {
    /// Native lexical collection.
    pub lexical_index: ObjectId,
    /// MATCH query evaluated by the native analyzer.
    pub lexical_query: &'request str,
    /// Maximum lexical candidates passed to RRF.
    pub lexical_limit: usize,
    /// Native vector index.
    pub vector_index: ObjectId,
    /// Validated native query vector.
    pub vector_query: &'request Vector,
    /// Exact or approximate vector execution.
    pub vector_branch: NativeVectorBranch,
    /// Maximum exact-vector candidates; ANN uses `AnnSearchOptions::k`.
    pub vector_limit: usize,
    /// Deterministic native RRF settings.
    pub fusion: NativeHybridFusion,
}

/// Full per-result native fusion explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHybridExplanation {
    /// One-based lexical rank.
    pub lexical_rank: Option<u64>,
    /// Canonical lexical score nanos.
    pub lexical_score_nanos: Option<i64>,
    /// One-based vector rank.
    pub vector_rank: Option<u64>,
    /// Canonical vector score nanos.
    pub vector_score_nanos: Option<i64>,
    /// Integer lexical RRF contribution.
    pub lexical_contribution: u64,
    /// Integer vector RRF contribution.
    pub vector_contribution: u64,
    /// Checked contribution sum.
    pub fusion_score: u64,
    /// One-based final rank.
    pub final_rank: u64,
}

/// One canonical native hybrid match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHybridMatch {
    /// Stable object identity.
    pub object_id: ObjectId,
    /// Explainable fusion components.
    pub explanation: NativeHybridExplanation,
}

/// Complete native hybrid outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeHybridOutcome {
    /// At least one branch produced candidates.
    Matches(Vec<NativeHybridMatch>),
    /// Both branches had no candidates.
    Abstained,
}

/// Evidence returned with one embedded hybrid execution.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridReceipt {
    /// Immutable all-engine CSN shared by both branches.
    pub snapshot_csn: Option<Csn>,
    /// Complete native RRF outcome.
    pub outcome: NativeHybridOutcome,
    /// Lexical candidates admitted to fusion.
    pub lexical_candidates: usize,
    /// Vector candidates admitted to fusion.
    pub vector_candidates: usize,
    /// ANN execution evidence, present only for an ANN branch.
    pub ann: Option<AnnSearchReceipt>,
}

/// Bounded prepared lexical query captured by one owned read view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeLexicalReadViewOpenRequest<'request> {
    /// Native lexical collection pinned by the view.
    pub index: ObjectId,
    /// Query analyzed and planned once during open.
    pub query: &'request str,
    /// Maximum canonically ranked results returned on every execution.
    pub limit: usize,
    /// Hard ceiling on retained encoded postings across all planned terms.
    pub maximum_retained_postings: usize,
    /// Hard ceiling on the complete retained lexical allocation.
    pub maximum_retained_bytes: u64,
}

/// Query-bound exact scalar filter over the same captured structure root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeStructureScalarFilter<'request> {
    /// Prefix prepended to every lexical document identity.
    pub key_prefix: &'request [u8],
    /// Canonical inline scalar value required for admission. Blob-backed
    /// scalar candidates are outside this frozen view contract and fail open.
    pub expected_inline_value: &'request [u8],
    /// Frozen logical time used to evaluate expiry on every observation.
    pub logical_time_micros: i64,
}

/// Open request for one same-root filtered lexical authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeFilteredLexicalReadViewOpenRequest<'request> {
    /// Lexical query, result limit, and retention bounds.
    pub lexical: NativeLexicalReadViewOpenRequest<'request>,
    /// Root-bound scalar predicate applied before final ranking.
    pub filter: NativeStructureScalarFilter<'request>,
}

/// One-time lexical hydration and retained-plan evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeLexicalReadViewOpenReceipt {
    /// Frozen statement that the view retains only query-bound encoded bytes.
    pub lexical_plan_scope: &'static str,
    /// Versioned algorithm used for `lexical_index_identity`.
    pub lexical_index_identity_algorithm: &'static str,
    /// Physical search-root, object, and format identity selected at open.
    pub lexical_index_identity: [u8; 32],
    /// Complete captured all-engine root identity.
    pub root_identity: [u8; 32],
    /// Exact visible commit shared with sibling views.
    pub snapshot_csn: Option<Csn>,
    /// Exact captured catalog version.
    pub catalog_version: CatalogVersion,
    /// Stable lexical collection identity.
    pub index_id: ObjectId,
    /// Canonical analyzed terms retained by the plan.
    pub planned_terms: usize,
    /// Encoded posting records retained by the plan.
    pub retained_postings: usize,
    /// Caller-authorized encoded-posting ceiling.
    pub maximum_retained_postings: usize,
    /// Caller-authorized retained allocation ceiling.
    pub maximum_retained_bytes: u64,
    /// Conservative physical entries covered before hydration.
    pub planned_physical_entries: usize,
    /// Conservative encoded physical bytes covered before hydration.
    pub planned_physical_bytes: u64,
    /// Exact root-bound metadata/posting/document entries observed at open.
    pub observed_physical_entries: usize,
    /// Exact encoded keys and complete record values visited while hydrating.
    pub observed_physical_bytes: u64,
    /// Engine-derived allocation admitted before any posting scan or copy.
    pub admitted_retained_memory_bytes: u64,
    /// Governed bytes retained until the last handle drops.
    pub retained_memory_bytes: u64,
    /// Physical pages read while opening the view.
    pub physical_page_reads: u64,
    /// Metadata-only preplan admission released before retained hydration.
    pub planning: NativeEngineWorkReceipt,
    /// Admission and execution evidence for opening the view.
    pub hydration: NativeEngineWorkReceipt,
}

/// Per-query evidence from an owned lexical read view.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeLexicalReadViewQueryReceipt {
    /// Frozen statement of the lexical work repeated by this observation.
    pub lexical_execution: &'static str,
    /// Physical lexical identity re-executed without storage access.
    pub lexical_index_identity: [u8; 32],
    /// Complete captured all-engine root identity.
    pub root_identity: [u8; 32],
    /// Exact visible commit used by this execution.
    pub snapshot_csn: Option<Csn>,
    /// Exact captured catalog version.
    pub catalog_version: CatalogVersion,
    /// Monotonic execution sequence proving that results were recomputed.
    pub execution_sequence: u64,
    /// Encoded postings decoded and scored by this execution.
    pub postings_evaluated: usize,
    /// Canonically ranked BM25 hits from this execution.
    pub hits: Vec<crate::MatchHit>,
    /// Query admission and execution evidence.
    pub execution: NativeEngineWorkReceipt,
    /// Always zero: a hydrated view cannot touch pages.
    pub physical_page_reads: u64,
}

/// Same-root lexical and structure-filter hydration evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeFilteredLexicalReadViewOpenReceipt {
    /// Complete shared root identity.
    pub root_identity: [u8; 32],
    /// Exact visible commit used by both engines.
    pub snapshot_csn: Option<Csn>,
    /// Captured catalog version.
    pub catalog_version: CatalogVersion,
    /// Lexical hydration evidence.
    pub lexical: NativeLexicalReadViewOpenReceipt,
    /// Versioned physical structure-filter identity.
    pub structure_filter_identity_algorithm: &'static str,
    /// Frozen value-storage boundary for the retained predicate.
    pub structure_filter_value_scope: &'static str,
    /// Physical structure root plus predicate identity.
    pub structure_filter_identity: [u8; 32],
    /// Candidate records retained before any final top-K ranking.
    pub retained_filter_records: usize,
    /// Conservative physical records admitted before filter hydration.
    pub planned_filter_physical_entries: usize,
    /// Conservative encoded key/value bytes admitted before hydration.
    pub planned_filter_physical_bytes: u64,
    /// Exact existing physical records visited at open.
    pub observed_filter_physical_entries: usize,
    /// Exact encoded key/value bytes visited at open.
    pub observed_filter_physical_bytes: u64,
    /// Retained filter allocation measured from actual vector capacities.
    pub retained_filter_memory_bytes: u64,
    /// Governed metadata-only filter preplan evidence.
    pub filter_planning: NativeEngineWorkReceipt,
    /// Governed filter hydration evidence before the memory-only shrink.
    pub filter_hydration: NativeEngineWorkReceipt,
    /// Additional physical page reads for filter planning/hydration only.
    pub physical_page_reads: u64,
}

/// Fresh filtered lexical result and per-observation predicate evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeFilteredLexicalReadViewQueryReceipt {
    /// Frozen repeated predicate execution contract.
    pub filter_execution: &'static str,
    /// Complete shared root identity.
    pub root_identity: [u8; 32],
    /// Exact shared visible commit.
    pub snapshot_csn: Option<Csn>,
    /// Captured catalog version.
    pub catalog_version: CatalogVersion,
    /// Physical lexical index identity shared with the open receipt.
    pub lexical_index_identity: [u8; 32],
    /// Versioned physical structure-filter identity.
    pub structure_filter_identity: [u8; 32],
    /// Monotonic sequence proving each filtered result was recomputed.
    pub execution_sequence: u64,
    /// Encoded postings decoded and scored after candidate filtering.
    pub postings_scored: usize,
    /// Unique candidate structure records decoded.
    pub filter_records_evaluated: usize,
    /// Unique candidates admitted by the exact predicate.
    pub filter_records_matched: usize,
    /// Canonically ranked hits after filtering.
    pub hits: Vec<crate::MatchHit>,
    /// Query admission and execution evidence.
    pub execution: NativeEngineWorkReceipt,
    /// Always zero after the owned view opens.
    pub physical_page_reads: u64,
}

#[derive(Debug)]
struct NativeLexicalReadViewInner {
    state: NativeLexicalReadState,
    snapshot: hyphae_native_mvcc::Snapshot,
    governor: Arc<crate::NativeResourceGovernor>,
    maximum_wait: std::time::Duration,
    _memory_permit: crate::OwnedGovernorPermit,
    database_live: Arc<std::sync::atomic::AtomicBool>,
    live_views: Arc<AtomicU64>,
    sequence: AtomicU64,
    open_receipt: NativeLexicalReadViewOpenReceipt,
}

impl Drop for NativeLexicalReadViewInner {
    fn drop(&mut self) {
        let previous = self.live_views.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }
}

/// Cloneable owned lexical view that re-decodes and scores retained bytes.
#[derive(Clone, Debug)]
pub struct NativeLexicalReadView {
    inner: Arc<NativeLexicalReadViewInner>,
}

impl NativeLexicalReadView {
    /// Returns one-time plan/hydration evidence.
    pub fn open_receipt(&self) -> &NativeLexicalReadViewOpenReceipt {
        &self.inner.open_receipt
    }

    /// Re-decodes, scores, merges, and ranks the retained encoded postings.
    ///
    /// # Errors
    ///
    /// Returns fail-closed for a dropped database, admission rejection, or
    /// malformed retained bytes.
    pub fn search(&self) -> Result<NativeLexicalReadViewQueryReceipt, NativeRuntimeError> {
        self.search_with_control(self.inner.maximum_wait, None)
    }

    fn search_with_control(
        &self,
        maximum_wait: std::time::Duration,
        cancellation: Option<&crate::GovernorCancellation>,
    ) -> Result<NativeLexicalReadViewQueryReceipt, NativeRuntimeError> {
        if !self.inner.database_live.load(Ordering::Acquire) {
            return Err(NativeRuntimeError::AnnReadViewDatabaseClosed);
        }
        if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
            return Err(NativeRuntimeError::ResourceQueue(
                crate::GovernorQueueError::Cancelled,
            ));
        }
        let planned_memory = u64::try_from(self.inner.state.retained_postings)
            .ok()
            .and_then(|postings| postings.checked_mul(LEXICAL_SCORE_ENTRY_BYTES))
            .and_then(|bytes| bytes.checked_add(LEXICAL_FIXED_QUERY_BYTES))
            .ok_or_else(|| {
                NativeRuntimeError::Model("lexical-read-view-query-memory-overflow".to_owned())
            })?;
        let permit = crate::admit_governor_work(
            &self.inner.governor,
            maximum_wait,
            WorkloadClass::ForegroundBounded,
            crate::GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: planned_memory,
            },
            cancellation,
        )?;
        let execution = permit.subdivision(crate::GovernorRequest {
            compute_threads: 1,
            io_slots: 0,
            memory_bytes: planned_memory,
        })?;
        self.search_with_parent_permit(execution, cancellation)
    }

    fn query_scratch_bytes(&self) -> Result<u64, NativeRuntimeError> {
        u64::try_from(self.inner.state.retained_postings)
            .ok()
            .and_then(|postings| postings.checked_mul(LEXICAL_SCORE_ENTRY_BYTES))
            .and_then(|bytes| bytes.checked_add(LEXICAL_FIXED_QUERY_BYTES))
            .ok_or_else(|| {
                NativeRuntimeError::Model("lexical-read-view-query-memory-overflow".to_owned())
            })
    }

    fn search_with_parent_permit(
        &self,
        permit: crate::DatabaseGovernorSubdivision<'_>,
        cancellation: Option<&crate::GovernorCancellation>,
    ) -> Result<NativeLexicalReadViewQueryReceipt, NativeRuntimeError> {
        let mut scores = BTreeMap::<Vec<u8>, f64>::new();
        let mut postings_evaluated = 0_usize;
        for term in &self.inner.state.terms {
            if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
                return Err(NativeRuntimeError::ResourceQueue(
                    crate::GovernorQueueError::Cancelled,
                ));
            }
            let idf = crate::bm25_idf(
                search_count_f64(self.inner.state.document_count)?,
                search_count_f64(term.document_frequency)?,
            );
            let average_length = search_count_f64(self.inner.state.total_document_terms)?
                / search_count_f64(self.inner.state.document_count)?;
            let mut live = 0_u64;
            for posting in &term.postings {
                if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
                    return Err(NativeRuntimeError::ResourceQueue(
                        crate::GovernorQueueError::Cancelled,
                    ));
                }
                let term_frequency = decode_live_search_posting(
                    &posting.encoded_frequency,
                    self.inner.state.format,
                )?
                .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                let (document_length, _, _) =
                    decode_search_document_header(&posting.encoded_document_header)?;
                *scores.entry(posting.document_id.clone()).or_default() += bm25_term_score(
                    idf,
                    f64::from(term_frequency.term_frequency),
                    search_count_f64(document_length)?,
                    average_length,
                    crate::model::Bm25ScoreParameters::default(),
                );
                live = live
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                postings_evaluated = postings_evaluated
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            }
            if live != term.document_frequency {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
        }
        let hits = rank_match_hits(scores, self.inner.state.limit);
        let execution_sequence = self
            .inner
            .sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                NativeRuntimeError::Model("lexical-read-view-sequence-overflow".to_owned())
            })?
            .checked_add(1)
            .ok_or_else(|| {
                NativeRuntimeError::Model("lexical-read-view-sequence-overflow".to_owned())
            })?;
        Ok(NativeLexicalReadViewQueryReceipt {
            lexical_execution: NATIVE_LEXICAL_READ_VIEW_EXECUTION,
            lexical_index_identity: self.inner.state.lexical_index_identity,
            root_identity: self.inner.state.root_identity,
            snapshot_csn: Some(self.inner.state.snapshot_csn),
            catalog_version: self.inner.state.catalog_version,
            execution_sequence,
            postings_evaluated,
            hits,
            execution: permit.finish(),
            physical_page_reads: 0,
        })
    }
}

#[derive(Debug)]
struct NativeFilteredLexicalReadViewInner {
    lexical: NativeLexicalReadView,
    filter: NativeStructureFilterState,
    _memory_permit: crate::OwnedGovernorPermit,
    sequence: AtomicU64,
    open_receipt: NativeFilteredLexicalReadViewOpenReceipt,
}

/// Cloneable same-root lexical view with a retained encoded scalar predicate.
#[derive(Clone, Debug)]
pub struct NativeFilteredLexicalReadView {
    inner: Arc<NativeFilteredLexicalReadViewInner>,
}

impl NativeFilteredLexicalReadView {
    /// Returns one-time lexical and structure-filter hydration evidence.
    pub fn open_receipt(&self) -> &NativeFilteredLexicalReadViewOpenReceipt {
        &self.inner.open_receipt
    }

    /// Re-decodes postings and retained structure records, applies the
    /// predicate, then ranks the complete admitted candidate set.
    ///
    /// # Errors
    ///
    /// Returns fail-closed for cancellation, admission failure, or malformed
    /// retained bytes. No partial result is returned.
    pub fn search(&self) -> Result<NativeFilteredLexicalReadViewQueryReceipt, NativeRuntimeError> {
        self.search_with_cancellation(None)
    }

    /// Executes one filtered observation with cooperative cancellation.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::search`] plus cancellation.
    #[allow(clippy::too_many_lines)]
    pub fn search_with_cancellation(
        &self,
        cancellation: Option<&crate::GovernorCancellation>,
    ) -> Result<NativeFilteredLexicalReadViewQueryReceipt, NativeRuntimeError> {
        if !self
            .inner
            .lexical
            .inner
            .database_live
            .load(Ordering::Acquire)
        {
            return Err(NativeRuntimeError::AnnReadViewDatabaseClosed);
        }
        if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
            return Err(NativeRuntimeError::ResourceQueue(
                crate::GovernorQueueError::Cancelled,
            ));
        }
        let candidates = u64::try_from(self.inner.filter.records.len()).unwrap_or(u64::MAX);
        let planned_memory = candidates
            .checked_mul(LEXICAL_SCORE_ENTRY_BYTES.saturating_mul(2))
            .and_then(|bytes| bytes.checked_add(LEXICAL_FIXED_QUERY_BYTES))
            .ok_or_else(|| {
                NativeRuntimeError::Model("filtered-lexical-query-memory-overflow".to_owned())
            })?;
        let permit = crate::admit_governor_work(
            &self.inner.lexical.inner.governor,
            self.inner.lexical.inner.maximum_wait,
            WorkloadClass::ForegroundBounded,
            crate::GovernorRequest {
                compute_threads: 1,
                io_slots: 0,
                memory_bytes: planned_memory,
            },
            cancellation,
        )?;
        let mut admitted = BTreeSet::new();
        for record in &self.inner.filter.records {
            if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
                return Err(NativeRuntimeError::ResourceQueue(
                    crate::GovernorQueueError::Cancelled,
                ));
            }
            if retained_structure_filter_matches(
                record.encoded.as_deref(),
                &self.inner.filter.expected_inline_value,
                self.inner.filter.logical_time_micros,
            )? {
                admitted.insert(record.document_id.clone());
            }
        }
        let mut scores = BTreeMap::<Vec<u8>, f64>::new();
        let mut postings_scored = 0_usize;
        for term in &self.inner.lexical.inner.state.terms {
            let idf = crate::bm25_idf(
                search_count_f64(self.inner.lexical.inner.state.document_count)?,
                search_count_f64(term.document_frequency)?,
            );
            let average_length =
                search_count_f64(self.inner.lexical.inner.state.total_document_terms)?
                    / search_count_f64(self.inner.lexical.inner.state.document_count)?;
            for posting in &term.postings {
                if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
                    return Err(NativeRuntimeError::ResourceQueue(
                        crate::GovernorQueueError::Cancelled,
                    ));
                }
                if !admitted.contains(&posting.document_id) {
                    continue;
                }
                postings_scored = postings_scored
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                let term_frequency = decode_live_search_posting(
                    &posting.encoded_frequency,
                    self.inner.lexical.inner.state.format,
                )?
                .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                let (document_length, _, _) =
                    decode_search_document_header(&posting.encoded_document_header)?;
                *scores.entry(posting.document_id.clone()).or_default() += bm25_term_score(
                    idf,
                    f64::from(term_frequency.term_frequency),
                    search_count_f64(document_length)?,
                    average_length,
                    crate::model::Bm25ScoreParameters::default(),
                );
            }
        }
        let hits = rank_match_hits(scores, self.inner.lexical.inner.state.limit);
        if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
            return Err(NativeRuntimeError::ResourceQueue(
                crate::GovernorQueueError::Cancelled,
            ));
        }
        let execution_sequence = self
            .inner
            .sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                NativeRuntimeError::Model("filtered-lexical-read-view-sequence-overflow".to_owned())
            })?
            .checked_add(1)
            .ok_or_else(|| {
                NativeRuntimeError::Model("filtered-lexical-read-view-sequence-overflow".to_owned())
            })?;
        Ok(NativeFilteredLexicalReadViewQueryReceipt {
            filter_execution: NATIVE_STRUCTURE_FILTER_EXECUTION,
            root_identity: self.inner.open_receipt.root_identity,
            snapshot_csn: self.inner.open_receipt.snapshot_csn,
            catalog_version: self.inner.open_receipt.catalog_version,
            lexical_index_identity: self.inner.open_receipt.lexical.lexical_index_identity,
            structure_filter_identity: self.inner.filter.structure_identity,
            execution_sequence,
            postings_scored,
            filter_records_evaluated: self.inner.filter.records.len(),
            filter_records_matched: admitted.len(),
            hits,
            execution: permit.finish(),
            physical_page_reads: 0,
        })
    }
}

/// One `RootSet` capture used to open lexical and ANN sibling views.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeHybridReadViewOpenRequest<'request> {
    /// Prepared lexical branch.
    pub lexical: NativeLexicalReadViewOpenRequest<'request>,
    /// Native vector index hydrated from the same root set.
    pub vector_index: ObjectId,
}

/// One-time evidence for a composed, single-root hybrid view.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridReadViewOpenReceipt {
    /// Complete shared all-engine root identity.
    pub root_identity: [u8; 32],
    /// Exact shared visible commit.
    pub snapshot_csn: Option<Csn>,
    /// Exact shared catalog version.
    pub catalog_version: CatalogVersion,
    /// Lexical view open evidence.
    pub lexical: NativeLexicalReadViewOpenReceipt,
    /// ANN view open evidence.
    pub ann: NativeAnnReadViewOpenReceipt,
}

/// One selected-route query against a composed hybrid view.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridReadViewQuery<'request> {
    /// Query vector evaluated by the retained ANN sibling.
    pub vector_query: &'request Vector,
    /// ANN breadth and result limit.
    pub ann_options: AnnSearchOptions,
    /// Maximum selected-route partition budget.
    pub maximum_partitions: usize,
    /// Deterministic native RRF settings.
    pub fusion: NativeHybridFusion,
}

/// Per-query same-root lexical, selected ANN, and fusion evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeHybridReadViewQueryReceipt {
    /// Complete shared all-engine root identity.
    pub root_identity: [u8; 32],
    /// Exact shared visible commit.
    pub snapshot_csn: Option<Csn>,
    /// Exact shared catalog version.
    pub catalog_version: CatalogVersion,
    /// Monotonic lexical recomputation sequence.
    pub execution_sequence: u64,
    /// Fresh lexical execution evidence.
    pub lexical: NativeLexicalReadViewQueryReceipt,
    /// Fresh selected-route ANN execution evidence.
    pub ann: NativeAnnReadViewQueryReceipt,
    /// Atomic peak admission covering result retention plus the largest
    /// sequential branch scratch allocation.
    pub peak_admission: NativeEngineWorkReceipt,
    /// Memory-only admission retained from before the first branch through
    /// complete result construction.
    pub result_retention: NativeEngineWorkReceipt,
    /// Compute-only admission and execution evidence for bounded in-process fusion.
    pub fusion: NativeEngineWorkReceipt,
    /// Complete native RRF outcome.
    pub outcome: NativeHybridOutcome,
}

/// Cloneable composed hybrid authority with shared lexical/ANN siblings.
#[derive(Clone, Debug)]
pub struct NativeHybridReadView {
    lexical: NativeLexicalReadView,
    ann: NativeAnnReadView,
    open_receipt: Arc<NativeHybridReadViewOpenReceipt>,
}

impl NativeHybridReadView {
    /// Returns a clone of the one shared lexical authority.
    pub fn lexical_view(&self) -> NativeLexicalReadView {
        self.lexical.clone()
    }

    /// Returns a clone of the one shared ANN authority.
    pub fn ann_view(&self) -> NativeAnnReadView {
        self.ann.clone()
    }

    /// Returns one-time shared-root evidence.
    pub fn open_receipt(&self) -> &NativeHybridReadViewOpenReceipt {
        &self.open_receipt
    }

    /// Executes two sequential governor-admitted branches and performs bounded
    /// in-process fusion over their fresh results.
    ///
    /// # Errors
    ///
    /// Returns fail-closed for invalid fusion, branch failure, or identity
    /// drift between the two sibling views.
    pub fn search_selected(
        &self,
        request: &NativeHybridReadViewQuery<'_>,
    ) -> Result<NativeHybridReadViewQueryReceipt, NativeHybridError> {
        self.search_selected_with_control(request, None, None, None)
    }

    /// Executes both branches with an explicit ANN worker and queue budget.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::search_selected`] plus invalid
    /// worker-limit or bounded-queue failures from the ANN sibling.
    pub fn search_selected_with_worker_budget(
        &self,
        request: &NativeHybridReadViewQuery<'_>,
        maximum_workers: u64,
        maximum_wait: std::time::Duration,
    ) -> Result<NativeHybridReadViewQueryReceipt, NativeHybridError> {
        self.search_selected_with_control(request, Some(maximum_workers), Some(maximum_wait), None)
    }

    /// Executes both branches with cooperative cancellation.
    ///
    /// Cancellation before the lexical branch, between branches, or inside
    /// ANN routing returns no fused partial result and leaves both views reusable.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::search_selected`] plus a cancelled
    /// resource-queue error.
    pub fn search_selected_with_cancellation(
        &self,
        request: &NativeHybridReadViewQuery<'_>,
        cancellation: &crate::GovernorCancellation,
    ) -> Result<NativeHybridReadViewQueryReceipt, NativeHybridError> {
        self.search_selected_with_control(request, None, None, Some(cancellation))
    }

    fn search_selected_with_control(
        &self,
        request: &NativeHybridReadViewQuery<'_>,
        maximum_workers: Option<u64>,
        maximum_wait: Option<std::time::Duration>,
        cancellation: Option<&crate::GovernorCancellation>,
    ) -> Result<NativeHybridReadViewQueryReceipt, NativeHybridError> {
        validate_read_view_query(request)?;
        if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
            return Err(
                NativeRuntimeError::ResourceQueue(crate::GovernorQueueError::Cancelled).into(),
            );
        }
        let effective_wait = maximum_wait.unwrap_or(self.lexical.inner.maximum_wait);
        let result_retention_memory = self.result_retention_memory(request)?;
        let ann_workers = self.ann.query_compute_threads(maximum_workers)?;
        let lexical_scratch = self.lexical.query_scratch_bytes()?;
        let ann_scratch = self
            .ann
            .query_scratch_bytes(request.ann_options, ann_workers)?;
        let peak_memory = result_retention_memory
            .checked_add(lexical_scratch.max(ann_scratch))
            .ok_or(NativeHybridError::ArithmeticOverflow)?;
        let peak_permit = crate::admit_governor_work(
            &self.lexical.inner.governor,
            effective_wait,
            WorkloadClass::ForegroundBounded,
            crate::GovernorRequest {
                compute_threads: ann_workers.max(1),
                io_slots: 0,
                memory_bytes: peak_memory,
            },
            cancellation,
        )?;
        let result_retention_permit = peak_permit.subdivision(crate::GovernorRequest {
            compute_threads: 0,
            io_slots: 0,
            memory_bytes: result_retention_memory,
        })?;
        let lexical_permit = peak_permit.subdivision(crate::GovernorRequest {
            compute_threads: 1,
            io_slots: 0,
            memory_bytes: lexical_scratch,
        })?;
        let lexical = self
            .lexical
            .search_with_parent_permit(lexical_permit, cancellation)?;
        if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
            return Err(
                NativeRuntimeError::ResourceQueue(crate::GovernorQueueError::Cancelled).into(),
            );
        }
        let ann = self.execute_ann_phase(
            request,
            ann_workers,
            ann_scratch,
            &peak_permit,
            cancellation,
        )?;
        if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
            return Err(
                NativeRuntimeError::ResourceQueue(crate::GovernorQueueError::Cancelled).into(),
            );
        }
        if lexical.root_identity != ann.root_identity
            || lexical.root_identity != self.open_receipt.root_identity
            || lexical.snapshot_csn != ann.search.search.snapshot_csn
            || lexical.snapshot_csn != self.open_receipt.snapshot_csn
            || lexical.catalog_version != self.open_receipt.catalog_version
        {
            return Err(NativeHybridError::Runtime(NativeRuntimeError::Model(
                "hybrid-read-view-identity-mismatch".to_owned(),
            )));
        }
        let fusion_permit = peak_permit.subdivision(crate::GovernorRequest {
            compute_threads: 1,
            io_slots: 0,
            memory_bytes: 0,
        })?;
        let outcome = fuse(&lexical.hits, &ann.search.search.hits, &request.fusion)?;
        if cancellation.is_some_and(crate::GovernorCancellation::is_cancelled) {
            return Err(
                NativeRuntimeError::ResourceQueue(crate::GovernorQueueError::Cancelled).into(),
            );
        }
        let fusion = fusion_permit.finish();
        let result_retention = result_retention_permit.finish();
        let peak_admission = peak_permit.finish();
        Ok(NativeHybridReadViewQueryReceipt {
            root_identity: lexical.root_identity,
            snapshot_csn: lexical.snapshot_csn,
            catalog_version: lexical.catalog_version,
            execution_sequence: lexical.execution_sequence,
            lexical,
            ann,
            peak_admission,
            result_retention,
            fusion,
            outcome,
        })
    }

    fn execute_ann_phase(
        &self,
        request: &NativeHybridReadViewQuery<'_>,
        compute_threads: u64,
        query_scratch_bytes: u64,
        peak_permit: &crate::DatabaseGovernorPermit,
        cancellation: Option<&crate::GovernorCancellation>,
    ) -> Result<NativeAnnReadViewQueryReceipt, NativeHybridError> {
        let phase_request = crate::GovernorRequest {
            compute_threads,
            io_slots: 0,
            memory_bytes: query_scratch_bytes,
        };
        let phase = peak_permit.sequential_phase_evidence(phase_request)?;
        let ann = self.ann.search_selected_with_parent_permit(
            &crate::NativeAnnReadViewParentQuery {
                query: request.vector_query,
                options: request.ann_options,
                maximum_partitions: request.maximum_partitions,
                compute_threads,
                query_scratch_bytes,
                cancellation,
            },
            peak_permit.permit(),
        )?;
        Ok(NativeAnnReadViewQueryReceipt {
            execution: phase.finish(),
            ..ann
        })
    }

    fn result_retention_memory(
        &self,
        request: &NativeHybridReadViewQuery<'_>,
    ) -> Result<u64, NativeHybridError> {
        let lexical_outer_slots = u64::try_from(self.lexical.inner.state.retained_postings)
            .map_err(|_| NativeHybridError::ArithmeticOverflow)?;
        let lexical_document_ids = u64::try_from(self.lexical.inner.state.limit)
            .map_err(|_| NativeHybridError::ArithmeticOverflow)?;
        let ann_partitions = u64::try_from(self.open_receipt.ann.logical_partitions)
            .map_err(|_| NativeHybridError::ArithmeticOverflow)?;
        let ann_hits_per_partition = u64::try_from(request.ann_options.k())
            .map_err(|_| NativeHybridError::ArithmeticOverflow)?;
        let fusion_candidates = lexical_document_ids
            .checked_add(ann_hits_per_partition)
            .ok_or(NativeHybridError::ArithmeticOverflow)?;

        lexical_outer_slots
            .checked_mul(u64::try_from(std::mem::size_of::<crate::MatchHit>()).unwrap_or(u64::MAX))
            .and_then(|bytes| {
                lexical_document_ids
                    .checked_mul(BTREE_MAX_KEY_SIZE as u64)
                    .and_then(|document_ids| bytes.checked_add(document_ids))
            })
            .and_then(|bytes| {
                ann_partitions
                    .checked_mul(ann_hits_per_partition)
                    .and_then(|hits| {
                        hits.checked_mul(
                            u64::try_from(std::mem::size_of::<VectorHit>()).unwrap_or(u64::MAX),
                        )
                    })
                    .and_then(|ann_hits| bytes.checked_add(ann_hits))
            })
            .and_then(|bytes| {
                ann_partitions
                    .checked_mul(u64::try_from(std::mem::size_of::<usize>()).unwrap_or(u64::MAX))
                    .and_then(|partitions| bytes.checked_add(partitions))
            })
            .and_then(|bytes| {
                fusion_candidates
                    .checked_mul(HYBRID_FUSION_ENTRY_BYTES)
                    .and_then(|fusion| bytes.checked_add(fusion))
            })
            .and_then(|bytes| bytes.checked_add(HYBRID_RESULT_FIXED_BYTES))
            .ok_or(NativeHybridError::ArithmeticOverflow)
    }
}

fn validate_read_view_query(
    request: &NativeHybridReadViewQuery<'_>,
) -> Result<(), NativeHybridError> {
    if !(1..=MAX_NATIVE_HYBRID_RETURNED).contains(&request.fusion.limit)
        || request.ann_options.k() > MAX_NATIVE_HYBRID_BRANCH_HITS
        || request.maximum_partitions == 0
        || !(1..=MAX_WEIGHT).contains(&request.fusion.lexical_weight)
        || !(1..=MAX_WEIGHT).contains(&request.fusion.vector_weight)
    {
        return Err(NativeHybridError::InvalidRequest);
    }
    Ok(())
}

/// Embedded hybrid validation or execution failure.
#[derive(Debug, Error)]
pub enum NativeHybridError {
    /// A weight or limit is outside its hard bound.
    #[error("native hybrid weights or limits are outside their bounded ranges")]
    InvalidRequest,
    /// A lexical document ID is not one canonical stable object ID.
    #[error("native hybrid lexical result has a noncanonical stable object ID")]
    InvalidStableId,
    /// A score cannot be represented canonically.
    #[error("native hybrid branch produced a noncanonical score")]
    InvalidScore,
    /// A branch repeats one stable ID.
    #[error("native hybrid branch contains a duplicate stable ID")]
    DuplicateBranchId,
    /// Checked fusion arithmetic failed.
    #[error("native hybrid contribution arithmetic overflow")]
    ArithmeticOverflow,
    /// A native branch failed.
    #[error(transparent)]
    Runtime(#[from] NativeRuntimeError),
}

#[derive(Clone, Copy, Default)]
struct Accumulator {
    lexical_rank: Option<u64>,
    lexical_score_nanos: Option<i64>,
    vector_rank: Option<u64>,
    vector_score_nanos: Option<i64>,
}

impl NativeSnapshot {
    /// Executes native lexical and exact vector/ANN branches on this snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid inputs, branch failures, or fusion overflow.
    pub fn retrieve_hybrid(
        &self,
        request: &NativeHybridRequest<'_>,
    ) -> Result<NativeHybridReceipt, NativeHybridError> {
        validate(request)?;
        let lexical = self.match_text(
            request.lexical_index,
            request.lexical_query,
            request.lexical_limit,
        )?;
        let (vector, ann) = match request.vector_branch {
            NativeVectorBranch::Exact => (
                self.search_vector_exact(
                    request.vector_index,
                    request.vector_query,
                    request.vector_limit,
                )?,
                None,
            ),
            NativeVectorBranch::Ann(options) => {
                let receipt =
                    self.search_ann(request.vector_index, request.vector_query, options)?;
                (receipt.hits.clone(), Some(receipt))
            }
        };
        let outcome = fuse(&lexical, &vector, &request.fusion)?;
        Ok(NativeHybridReceipt {
            snapshot_csn: self.visible_csn(),
            lexical_candidates: lexical.len(),
            vector_candidates: vector.len(),
            outcome,
            ann,
        })
    }
}

impl NativeDatabase {
    /// Opens one query-bound lexical view plus exact scalar predicate from the
    /// same captured `RootSet`, retaining only encoded records under governor
    /// authority.
    ///
    /// # Errors
    ///
    /// Returns without a partial view for invalid bounds, admission failure,
    /// malformed structure records, or identity drift.
    pub fn open_filtered_lexical_read_view(
        &self,
        request: &NativeFilteredLexicalReadViewOpenRequest<'_>,
    ) -> Result<
        (
            NativeFilteredLexicalReadView,
            NativeFilteredLexicalReadViewOpenReceipt,
        ),
        NativeRuntimeError,
    > {
        if !(1..=MAX_NATIVE_HYBRID_BRANCH_HITS).contains(&request.lexical.limit)
            || request.lexical.maximum_retained_postings == 0
            || request.lexical.maximum_retained_bytes == 0
        {
            return Err(NativeRuntimeError::InvalidSearchTree);
        }
        let (governor, _execution_pool) = self
            .resource_governor
            .as_ref()
            .zip(self.execution_pool.as_ref())
            .ok_or(NativeRuntimeError::AnnReadViewExecutionAuthorityRequired)?;
        let snapshot = self
            .coordinator
            .snapshot(request.filter.logical_time_micros)?;
        let (lexical, _) =
            self.open_lexical_hybrid_sibling(governor, &snapshot, &request.lexical)?;
        self.open_filtered_lexical_read_view_from_lexical(&lexical, &request.filter)
    }

    /// Derives a scalar-filter view from an existing query-bound lexical
    /// authority without hydrating or retaining a second lexical plan.
    ///
    /// # Errors
    ///
    /// Returns without a partial view when the lexical authority belongs to a
    /// different database/governor, filter planning or hydration fails, or
    /// retained physical evidence exceeds its preplan.
    pub fn open_filtered_lexical_read_view_from_lexical(
        &self,
        lexical: &NativeLexicalReadView,
        filter_request: &NativeStructureScalarFilter<'_>,
    ) -> Result<
        (
            NativeFilteredLexicalReadView,
            NativeFilteredLexicalReadViewOpenReceipt,
        ),
        NativeRuntimeError,
    > {
        let (governor, _execution_pool) = self
            .resource_governor
            .as_ref()
            .zip(self.execution_pool.as_ref())
            .ok_or(NativeRuntimeError::AnnReadViewExecutionAuthorityRequired)?;
        if !Arc::ptr_eq(governor, &lexical.inner.governor)
            || !Arc::ptr_eq(&self.database_live, &lexical.inner.database_live)
            || !lexical.inner.database_live.load(Ordering::Acquire)
        {
            return Err(NativeRuntimeError::InvalidCommittedRoot);
        }
        let reads_before = self.pages.physical_read_count();
        let lexical_open = lexical.inner.open_receipt.clone();
        let filter_planning = self
            .admit_foreground_bounded()?
            .ok_or(NativeRuntimeError::AnnReadViewExecutionAuthorityRequired)?;
        let filter_plan =
            NativeDatabase::plan_structure_filter_state(&lexical.inner.state, filter_request)?;
        let filter_planning = filter_planning.finish();
        let filter_retention = crate::admit_governor_work(
            governor,
            self.resource_queue_wait,
            WorkloadClass::ForegroundBounded,
            crate::GovernorRequest {
                compute_threads: 1,
                io_slots: 1,
                memory_bytes: filter_plan.hydration_memory_bytes,
            },
            None,
        )?;
        let filter = self.open_structure_filter_state(
            &lexical.inner.snapshot,
            &lexical.inner.state,
            filter_request,
            filter_retention.permit(),
            filter_plan,
        )?;
        if filter.observed_physical_entries > filter_plan.physical_entries
            || filter.observed_physical_bytes > filter_plan.physical_bytes
        {
            return Err(NativeRuntimeError::Model(
                "structure-filter-read-view-physical-plan-underflow".to_owned(),
            ));
        }
        let filter_hydration = filter_retention.evidence();
        let filter_retention = filter_retention.retain_memory(filter.retained_memory_bytes)?;
        let receipt = NativeFilteredLexicalReadViewOpenReceipt {
            root_identity: lexical_open.root_identity,
            snapshot_csn: lexical_open.snapshot_csn,
            catalog_version: lexical_open.catalog_version,
            lexical: lexical_open,
            structure_filter_identity_algorithm: NATIVE_STRUCTURE_FILTER_IDENTITY_ALGORITHM,
            structure_filter_value_scope: NATIVE_STRUCTURE_FILTER_VALUE_SCOPE,
            structure_filter_identity: filter.structure_identity,
            retained_filter_records: filter.records.len(),
            planned_filter_physical_entries: filter_plan.physical_entries,
            planned_filter_physical_bytes: filter_plan.physical_bytes,
            observed_filter_physical_entries: filter.observed_physical_entries,
            observed_filter_physical_bytes: filter.observed_physical_bytes,
            retained_filter_memory_bytes: filter.retained_memory_bytes,
            filter_planning,
            filter_hydration,
            physical_page_reads: self
                .pages
                .physical_read_count()
                .saturating_sub(reads_before),
        };
        let view = NativeFilteredLexicalReadView {
            inner: Arc::new(NativeFilteredLexicalReadViewInner {
                lexical: lexical.clone(),
                filter,
                _memory_permit: filter_retention.into_owned(),
                sequence: AtomicU64::new(0),
                open_receipt: receipt.clone(),
            }),
        };
        Ok((view, receipt))
    }

    fn open_lexical_hybrid_sibling(
        &self,
        governor: &Arc<crate::NativeResourceGovernor>,
        snapshot: &hyphae_native_mvcc::Snapshot,
        request: &NativeLexicalReadViewOpenRequest<'_>,
    ) -> Result<(NativeLexicalReadView, NativeLexicalReadViewOpenReceipt), NativeRuntimeError> {
        let reads_before = self.pages.physical_read_count();
        let planning = self
            .admit_foreground_bounded()?
            .ok_or(NativeRuntimeError::AnnReadViewExecutionAuthorityRequired)?;
        let lexical_plan = self.plan_lexical_read_state(snapshot, request)?;
        let planning = planning.finish();
        let lexical_retention = crate::admit_governor_work(
            governor,
            self.resource_queue_wait,
            WorkloadClass::ForegroundBounded,
            crate::GovernorRequest {
                compute_threads: 1,
                io_slots: 1,
                memory_bytes: lexical_plan.hydration_memory_bytes,
            },
            None,
        )?;
        let state = self.open_lexical_read_state(
            snapshot,
            &NativeLexicalReadViewOpenRequest {
                maximum_retained_bytes: lexical_plan.retained_memory_bytes,
                ..*request
            },
            lexical_retention.permit(),
            lexical_plan.retained_terms,
        )?;
        if state.observed_physical_entries > lexical_plan.physical_entries
            || state.observed_physical_bytes > lexical_plan.physical_bytes
        {
            return Err(NativeRuntimeError::Model(format!(
                "lexical-read-view-physical-plan-underflow planned_entries={} observed_entries={} planned_bytes={} observed_bytes={}",
                lexical_plan.physical_entries,
                state.observed_physical_entries,
                lexical_plan.physical_bytes,
                state.observed_physical_bytes
            )));
        }
        let hydration = lexical_retention.evidence();
        let lexical_retention = lexical_retention.retain_memory(state.retained_memory_bytes)?;
        let receipt = NativeLexicalReadViewOpenReceipt {
            lexical_plan_scope: NATIVE_LEXICAL_READ_VIEW_PLAN_SCOPE,
            lexical_index_identity_algorithm: NATIVE_LEXICAL_INDEX_IDENTITY_ALGORITHM,
            lexical_index_identity: state.lexical_index_identity,
            root_identity: state.root_identity,
            snapshot_csn: Some(state.snapshot_csn),
            catalog_version: state.catalog_version,
            index_id: state.index,
            planned_terms: state.terms.len(),
            retained_postings: state.retained_postings,
            maximum_retained_postings: request.maximum_retained_postings,
            maximum_retained_bytes: request.maximum_retained_bytes,
            planned_physical_entries: lexical_plan.physical_entries,
            planned_physical_bytes: lexical_plan.physical_bytes,
            observed_physical_entries: state.observed_physical_entries,
            observed_physical_bytes: state.observed_physical_bytes,
            admitted_retained_memory_bytes: lexical_plan.retained_memory_bytes,
            retained_memory_bytes: state.retained_memory_bytes,
            physical_page_reads: self
                .pages
                .physical_read_count()
                .saturating_sub(reads_before),
            planning,
            hydration,
        };
        let memory_permit = lexical_retention.into_owned();
        self.ann_read_views.fetch_add(1, Ordering::AcqRel);
        Ok((
            NativeLexicalReadView {
                inner: Arc::new(NativeLexicalReadViewInner {
                    state,
                    snapshot: snapshot.clone(),
                    governor: Arc::clone(governor),
                    maximum_wait: self.resource_queue_wait,
                    _memory_permit: memory_permit,
                    database_live: Arc::clone(&self.database_live),
                    live_views: Arc::clone(&self.ann_read_views),
                    sequence: AtomicU64::new(0),
                    open_receipt: receipt.clone(),
                }),
            },
            receipt,
        ))
    }

    /// Opens one lexical plan and one ANN view from one immutable `RootSet`.
    ///
    /// The returned sibling handles share their retained allocations with the
    /// composed view; no second hydration occurs when a handle is cloned.
    ///
    /// # Errors
    ///
    /// Returns without a partial view for invalid bounds, admission failure,
    /// lexical retention overflow, or ANN hydration failure.
    pub fn open_hybrid_read_view(
        &self,
        request: &NativeHybridReadViewOpenRequest<'_>,
    ) -> Result<(NativeHybridReadView, NativeHybridReadViewOpenReceipt), NativeHybridError> {
        if !(1..=MAX_NATIVE_HYBRID_BRANCH_HITS).contains(&request.lexical.limit)
            || request.lexical.maximum_retained_postings == 0
            || request.lexical.maximum_retained_bytes == 0
        {
            return Err(NativeHybridError::InvalidRequest);
        }
        let (governor, _execution_pool) = self
            .resource_governor
            .as_ref()
            .zip(self.execution_pool.as_ref())
            .ok_or(NativeRuntimeError::AnnReadViewExecutionAuthorityRequired)?;
        let snapshot = self
            .coordinator
            .snapshot(0)
            .map_err(NativeRuntimeError::from)?;
        let (lexical, lexical_open) =
            self.open_lexical_hybrid_sibling(governor, &snapshot, &request.lexical)?;
        let (ann, ann_open) =
            match self.open_ann_read_view_at_snapshot(request.vector_index, &snapshot, None) {
                Ok(opened) => opened,
                Err(error) => {
                    drop(lexical);
                    return Err(error.into());
                }
            };
        if lexical_open.root_identity != ann_open.root_identity
            || lexical_open.snapshot_csn != ann_open.snapshot_csn
            || lexical_open.catalog_version != ann_open.catalog_version
        {
            drop(ann);
            drop(lexical);
            return Err(NativeRuntimeError::Model(
                "hybrid-read-view-open-identity-mismatch".to_owned(),
            )
            .into());
        }
        let receipt = NativeHybridReadViewOpenReceipt {
            root_identity: lexical_open.root_identity,
            snapshot_csn: lexical_open.snapshot_csn,
            catalog_version: lexical_open.catalog_version,
            lexical: lexical_open,
            ann: ann_open,
        };
        Ok((
            NativeHybridReadView {
                lexical,
                ann,
                open_receipt: Arc::new(receipt.clone()),
            },
            receipt,
        ))
    }

    /// Captures one all-engine snapshot and executes both hybrid branches.
    ///
    /// # Errors
    ///
    /// Returns snapshot materialization or native hybrid failures.
    pub fn retrieve_hybrid_latest(
        &self,
        logical_time_micros: i64,
        request: &NativeHybridRequest<'_>,
    ) -> Result<NativeHybridReceipt, NativeHybridError> {
        let _permit = self.admit_foreground_bounded()?;
        self.snapshot(logical_time_micros)?.retrieve_hybrid(request)
    }
}

fn validate(request: &NativeHybridRequest<'_>) -> Result<(), NativeHybridError> {
    if !(1..=MAX_NATIVE_HYBRID_BRANCH_HITS).contains(&request.lexical_limit)
        || !(1..=MAX_NATIVE_HYBRID_BRANCH_HITS).contains(&request.vector_limit)
        || !(1..=MAX_NATIVE_HYBRID_RETURNED).contains(&request.fusion.limit)
        || !(1..=MAX_WEIGHT).contains(&request.fusion.lexical_weight)
        || !(1..=MAX_WEIGHT).contains(&request.fusion.vector_weight)
    {
        return Err(NativeHybridError::InvalidRequest);
    }
    if let NativeVectorBranch::Ann(options) = request.vector_branch
        && (options.k() > MAX_NATIVE_HYBRID_BRANCH_HITS || options.k() != request.vector_limit)
    {
        return Err(NativeHybridError::InvalidRequest);
    }
    Ok(())
}

fn fuse(
    lexical: &[crate::MatchHit],
    vector: &[VectorHit],
    request: &NativeHybridFusion,
) -> Result<NativeHybridOutcome, NativeHybridError> {
    if lexical.is_empty() && vector.is_empty() {
        return Ok(NativeHybridOutcome::Abstained);
    }
    let mut combined = BTreeMap::<ObjectId, Accumulator>::new();
    for (index, hit) in lexical.iter().enumerate() {
        let object_id = decode_object_id(&hit.document_id)?;
        let entry = combined.entry(object_id).or_default();
        if entry.lexical_rank.replace(one_based(index)?).is_some() {
            return Err(NativeHybridError::DuplicateBranchId);
        }
        entry.lexical_score_nanos = Some(scaled_score(hit.score)?);
    }
    for (index, hit) in vector.iter().enumerate() {
        let entry = combined.entry(hit.object_id).or_default();
        if entry.vector_rank.replace(one_based(index)?).is_some() {
            return Err(NativeHybridError::DuplicateBranchId);
        }
        entry.vector_score_nanos = Some(scaled_score(-hit.distance)?);
    }
    let mut matches = combined
        .into_iter()
        .map(|(object_id, entry)| build_match(object_id, entry, request))
        .collect::<Result<Vec<_>, NativeHybridError>>()?;
    matches.sort_by(|left, right| {
        right
            .explanation
            .fusion_score
            .cmp(&left.explanation.fusion_score)
            .then_with(|| left.object_id.cmp(&right.object_id))
    });
    matches.truncate(request.limit);
    for (index, matched) in matches.iter_mut().enumerate() {
        matched.explanation.final_rank = one_based(index)?;
    }
    Ok(NativeHybridOutcome::Matches(matches))
}

fn build_match(
    object_id: ObjectId,
    entry: Accumulator,
    request: &NativeHybridFusion,
) -> Result<NativeHybridMatch, NativeHybridError> {
    let lexical_contribution = contribution(request.lexical_weight, entry.lexical_rank)?;
    let vector_contribution = contribution(request.vector_weight, entry.vector_rank)?;
    let fusion_score = lexical_contribution
        .checked_add(vector_contribution)
        .ok_or(NativeHybridError::ArithmeticOverflow)?;
    Ok(NativeHybridMatch {
        object_id,
        explanation: NativeHybridExplanation {
            lexical_rank: entry.lexical_rank,
            lexical_score_nanos: entry.lexical_score_nanos,
            vector_rank: entry.vector_rank,
            vector_score_nanos: entry.vector_score_nanos,
            lexical_contribution,
            vector_contribution,
            fusion_score,
            final_rank: 0,
        },
    })
}

fn decode_object_id(encoded: &[u8]) -> Result<ObjectId, NativeHybridError> {
    let bytes: [u8; 16] = encoded
        .try_into()
        .map_err(|_| NativeHybridError::InvalidStableId)?;
    ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| NativeHybridError::InvalidStableId)
}

fn one_based(index: usize) -> Result<u64, NativeHybridError> {
    u64::try_from(index)
        .ok()
        .and_then(|rank| rank.checked_add(1))
        .ok_or(NativeHybridError::ArithmeticOverflow)
}

fn contribution(weight: u32, rank: Option<u64>) -> Result<u64, NativeHybridError> {
    let Some(rank) = rank else { return Ok(0) };
    NATIVE_HYBRID_RRF_CONSTANT
        .checked_add(rank)
        .and_then(|denominator| {
            u64::from(weight)
                .checked_mul(CONTRIBUTION_SCALE)
                .and_then(|numerator| numerator.checked_div(denominator))
        })
        .ok_or(NativeHybridError::ArithmeticOverflow)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "checked rounded f64 is intentionally canonicalized to i64 nanos"
)]
fn scaled_score(score: f64) -> Result<i64, NativeHybridError> {
    let scaled = score * 1_000_000_000.0;
    if !scaled.is_finite() || scaled < i64::MIN as f64 || scaled > i64::MAX as f64 {
        return Err(NativeHybridError::InvalidScore);
    }
    Ok(scaled.round() as i64)
}
