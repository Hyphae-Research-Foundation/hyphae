// SPDX-License-Identifier: Apache-2.0

//! Bounded relation-valued convergence over one immutable native snapshot.

#![allow(
    missing_docs,
    reason = "bounded G5 source variants are self-describing"
)]

use std::collections::{BTreeMap, BTreeSet};

use hyphae_native_types::{Csn, ObjectId};
use thiserror::Error;

use crate::{
    AnnSearchOptions, NativeHybridError, NativeHybridFusion, NativeHybridOutcome,
    NativeHybridRequest, NativeRuntimeError, NativeSnapshot, NativeVectorBranch, Vector,
};

/// Hard maximum number of native sources in one convergence plan.
pub const MAX_CONVERGENCE_SOURCES: usize = 16;
/// Hard maximum rows admitted from any source or returned by a plan.
pub const MAX_CONVERGENCE_ROWS: usize = 10_000;
/// Hard maximum aggregate expressions in one plan.
pub const MAX_CONVERGENCE_AGGREGATES: usize = 32;

/// Complete caller-controlled bounds for one execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct ConvergenceLimits {
    /// Maximum sources in the plan.
    pub max_sources: usize,
    /// Maximum rows admitted from each source.
    pub max_rows_per_source: usize,
    /// Maximum rows retained after the join.
    pub max_output_rows: usize,
    /// Maximum aggregate expressions.
    pub max_aggregates: usize,
}

impl Default for ConvergenceLimits {
    fn default() -> Self {
        Self {
            max_sources: 8,
            max_rows_per_source: 1_000,
            max_output_rows: 1_000,
            max_aggregates: 16,
        }
    }
}

/// One native structure projected to `(ObjectId, optional numeric value)`.
#[derive(Clone, Debug, PartialEq)]
pub enum StructureSource {
    /// The key is the object ID; the scalar bytes are retained as row payload.
    Scalar {
        /// Scalar key encoded as one canonical `ObjectId`.
        key: Vec<u8>,
    },
    /// Hash fields are object IDs; field values are retained as row payloads.
    Hash {
        /// Native hash key.
        key: Vec<u8>,
    },
    /// Set members are object IDs.
    Set {
        /// Native set key.
        key: Vec<u8>,
    },
    /// List items are object IDs.
    List {
        /// Native list key.
        key: Vec<u8>,
    },
    /// Sorted-set members are object IDs and scores are numeric metadata.
    SortedSet {
        /// Native sorted-set key.
        key: Vec<u8>,
    },
    /// Stream fields are object IDs; field values are retained as row payloads.
    Stream {
        /// Native stream key.
        key: Vec<u8>,
    },
}

/// Owned settings for one exact or ANN hybrid source.
#[derive(Clone, Debug, PartialEq)]
pub struct HybridSource {
    /// Native lexical collection.
    pub lexical_index: ObjectId,
    /// Analyzer-normalized lexical query input.
    pub lexical_query: String,
    /// Maximum lexical branch candidates.
    pub lexical_limit: usize,
    /// Native vector index.
    pub vector_index: ObjectId,
    /// Validated query vector.
    pub vector_query: Vector,
    /// Exact or ANN vector execution.
    pub vector_branch: NativeVectorBranch,
    /// Maximum vector branch candidates.
    pub vector_limit: usize,
    /// Deterministic RRF settings.
    pub fusion: NativeHybridFusion,
}

/// One source in a typed convergence plan.
#[derive(Clone, Debug, PartialEq)]
pub enum ConvergenceSource {
    Structure(StructureSource),
    /// Native lexical ranking; document IDs must be canonical object IDs.
    Lexical {
        /// Native lexical collection.
        index: ObjectId,
        /// Analyzer input.
        query: String,
        /// Maximum retained top-k matches.
        limit: usize,
    },
    /// Complete exact vector oracle.
    Exact {
        /// Native vector index.
        index: ObjectId,
        /// Validated query vector.
        query: Vector,
        /// Maximum retained top-k matches.
        limit: usize,
    },
    /// Native ANN plus an automatic exact top-k oracle comparison.
    Ann {
        /// Native vector index.
        index: ObjectId,
        /// Validated query vector.
        query: Vector,
        /// Bounded native graph-search settings.
        options: AnnSearchOptions,
    },
    /// Native RRF source, with an exact vector oracle when its vector branch is ANN.
    Hybrid(HybridSource),
}

/// Supported relation aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggregateOperation {
    Count,
    Sum,
    Min,
    Max,
}

/// One aggregate over joined rows. `Count` ignores `source`; numeric operations require it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AggregateSpec {
    /// Aggregate function.
    pub operation: AggregateOperation,
    /// Zero-based source whose numeric value is aggregated.
    pub source: Option<usize>,
}

/// Complete typed plan. Sources are inner-joined by stable object ID.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvergencePlan {
    /// Ordered native sources.
    pub sources: Vec<ConvergenceSource>,
    /// Aggregates evaluated after convergence.
    pub aggregates: Vec<AggregateSpec>,
    /// Complete execution limits.
    pub limits: ConvergenceLimits,
}

/// One deterministic joined row. Values retain source-plan order.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvergenceRow {
    /// Stable inner-join identity.
    pub object_id: ObjectId,
    /// Source values in plan order.
    pub values: Vec<ConvergenceValue>,
}

/// One source value retained by a joined relation row.
#[derive(Clone, Debug, PartialEq)]
pub enum ConvergenceValue {
    /// Source has identity but no scalar payload (set/list).
    Missing,
    /// Exact native bytes (scalar/hash/stream).
    Bytes(Vec<u8>),
    /// Finite native score (lexical/vector/sorted set).
    Number(f64),
}

/// One aggregate result. Empty min/max/sum inputs produce `Number(None)`.
#[derive(Clone, Debug, PartialEq)]
pub enum AggregateResult {
    Count(u64),
    Number(Option<f64>),
}

/// Physical strategy selected for one source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvergenceStrategy {
    ScalarLookup,
    HashRange,
    SetRange,
    ListRange,
    SortedSetRange,
    StreamRange,
    LexicalTopK,
    ExactVectorTopK,
    AnnTopK,
    HybridRrf,
}

/// Auditable source work and optional exact-oracle quality evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvergenceSourceMetrics {
    /// Physical source operation.
    pub strategy: ConvergenceStrategy,
    /// Whether the source read received a bounded limit.
    pub limit_pushed_down: bool,
    /// Rows examined at the native source boundary.
    pub rows_examined: usize,
    /// Rows admitted to convergence.
    pub rows_emitted: usize,
    /// Source-specific physical work, currently ANN node visits.
    pub native_work: usize,
    /// Exact top-k oracle result count when applicable.
    pub oracle_hits: Option<usize>,
    /// Approximate/exact top-k overlap when applicable.
    pub oracle_overlap: Option<usize>,
    /// Recall ratio in integer parts per million when applicable.
    pub oracle_recall_ppm: Option<u32>,
}

/// Complete convergence work counters.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvergenceMetrics {
    /// Per-source work counters.
    pub sources: Vec<ConvergenceSourceMetrics>,
    /// Object-ID membership probes performed by the inner join.
    pub join_key_probes: usize,
    /// Joined-row visits across aggregate expressions.
    pub aggregate_rows_examined: usize,
    /// Aggregates currently run after `ObjectId` convergence, never below a join.
    pub aggregates_pushed_down: bool,
}

/// Stable, non-textual explanation of the selected plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceExplanation {
    /// Same-snapshot commit sequence used by every source.
    pub snapshot_csn: Option<Csn>,
    /// Physical source strategies in typed-plan order.
    pub strategies: Vec<ConvergenceStrategy>,
    /// Whether execution performs an `ObjectId` inner join.
    pub inner_join_by_object_id: bool,
    /// Whether output uses ascending stable identity order.
    pub stable_object_id_order: bool,
}

/// Rows, aggregates, same-snapshot evidence, and execution metrics.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvergenceReceipt {
    /// Same-snapshot commit sequence used by every source.
    pub snapshot_csn: Option<Csn>,
    /// Stable relation-valued output.
    pub rows: Vec<ConvergenceRow>,
    /// Aggregate results in plan order.
    pub aggregates: Vec<AggregateResult>,
    /// Auditable execution counters.
    pub metrics: ConvergenceMetrics,
    /// Stable physical-plan explanation.
    pub explanation: ConvergenceExplanation,
}

/// Plan validation or native source failure. No partial relation is returned.
#[derive(Debug, Error)]
pub enum ConvergenceError {
    #[error("convergence plan limits are zero or exceed hard bounds")]
    InvalidLimits,
    #[error("convergence plan requires at least one source")]
    EmptyPlan,
    #[error("convergence source or aggregate exceeds its configured bound")]
    PlanLimitExceeded,
    #[error("convergence aggregate references an invalid or valueless source")]
    InvalidAggregate,
    #[error("convergence source emitted more than its row bound")]
    SourceRowLimitExceeded,
    #[error("convergence output exceeds its row bound")]
    OutputRowLimitExceeded,
    #[error("convergence source identity is not a canonical ObjectId")]
    InvalidObjectId,
    #[error("convergence source repeats one ObjectId")]
    DuplicateObjectId,
    #[error("convergence numeric bytes are not one finite decimal")]
    InvalidNumber,
    #[error("convergence numeric aggregate overflowed to a non-finite value")]
    NumericOverflow,
    #[error(transparent)]
    Runtime(#[from] NativeRuntimeError),
    #[error(transparent)]
    Hybrid(#[from] NativeHybridError),
}

type SourceRows = BTreeMap<ObjectId, ConvergenceValue>;

impl NativeSnapshot {
    /// Explains a valid convergence plan without reading source data.
    ///
    /// # Errors
    ///
    /// Returns an error when plan structure or limits are invalid.
    pub fn explain_convergence(
        &self,
        plan: &ConvergencePlan,
    ) -> Result<ConvergenceExplanation, ConvergenceError> {
        validate(plan)?;
        Ok(ConvergenceExplanation {
            snapshot_csn: self.visible_csn(),
            strategies: plan.sources.iter().map(strategy).collect(),
            inner_join_by_object_id: plan.sources.len() > 1,
            stable_object_id_order: true,
        })
    }

    /// Executes all sources, joins, and aggregates against this exact snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid plans, malformed source values, exhausted
    /// bounds, or native source failures. No partial relation is returned.
    pub fn converge(&self, plan: &ConvergencePlan) -> Result<ConvergenceReceipt, ConvergenceError> {
        let explanation = self.explain_convergence(plan)?;
        let mut sources = Vec::with_capacity(plan.sources.len());
        let mut source_metrics = Vec::with_capacity(plan.sources.len());
        for source in &plan.sources {
            let (rows, metrics) = self.read_convergence_source(source, plan.limits)?;
            sources.push(rows);
            source_metrics.push(metrics);
        }

        let mut join_key_probes = 0_usize;
        let mut rows = Vec::new();
        for object_id in sources[0].keys().copied() {
            let mut values = Vec::with_capacity(sources.len());
            let mut present = true;
            for source in &sources {
                join_key_probes = join_key_probes.saturating_add(1);
                if let Some(value) = source.get(&object_id) {
                    values.push(value.clone());
                } else {
                    present = false;
                    break;
                }
            }
            if present {
                if rows.len() == plan.limits.max_output_rows {
                    return Err(ConvergenceError::OutputRowLimitExceeded);
                }
                rows.push(ConvergenceRow { object_id, values });
            }
        }
        let aggregates = aggregate(&rows, &plan.aggregates)?;
        Ok(ConvergenceReceipt {
            snapshot_csn: self.visible_csn(),
            metrics: ConvergenceMetrics {
                sources: source_metrics,
                join_key_probes,
                aggregate_rows_examined: rows.len().saturating_mul(plan.aggregates.len()),
                aggregates_pushed_down: false,
            },
            rows,
            aggregates,
            explanation,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn read_convergence_source(
        &self,
        source: &ConvergenceSource,
        limits: ConvergenceLimits,
    ) -> Result<(SourceRows, ConvergenceSourceMetrics), ConvergenceError> {
        let cap = limits.max_rows_per_source;
        let mut native_work = 0;
        let mut oracle = None;
        let raw: Vec<(ObjectId, ConvergenceValue)> = match source {
            ConvergenceSource::Structure(structure) => {
                self.read_structure_source(structure, cap)?
            }
            ConvergenceSource::Lexical {
                index,
                query,
                limit,
            } => self
                .match_text(*index, query, *limit)?
                .into_iter()
                .map(|hit| {
                    Ok((
                        decode_id(&hit.document_id)?,
                        ConvergenceValue::Number(hit.score),
                    ))
                })
                .collect::<Result<_, ConvergenceError>>()?,
            ConvergenceSource::Exact {
                index,
                query,
                limit,
            } => self
                .search_vector_exact(*index, query, *limit)?
                .into_iter()
                .map(|hit| (hit.object_id, ConvergenceValue::Number(-hit.distance)))
                .collect(),
            ConvergenceSource::Ann {
                index,
                query,
                options,
            } => {
                let receipt = self.search_ann(*index, query, *options)?;
                native_work = receipt.visited_nodes;
                let exact = self.search_vector_exact(*index, query, options.k())?;
                oracle = Some(oracle_metrics(&receipt.hits, &exact));
                receipt
                    .hits
                    .into_iter()
                    .map(|hit| (hit.object_id, ConvergenceValue::Number(-hit.distance)))
                    .collect()
            }
            ConvergenceSource::Hybrid(hybrid) => {
                let request = NativeHybridRequest {
                    lexical_index: hybrid.lexical_index,
                    lexical_query: &hybrid.lexical_query,
                    lexical_limit: hybrid.lexical_limit,
                    vector_index: hybrid.vector_index,
                    vector_query: &hybrid.vector_query,
                    vector_branch: hybrid.vector_branch,
                    vector_limit: hybrid.vector_limit,
                    fusion: hybrid.fusion.clone(),
                };
                let receipt = self.retrieve_hybrid(&request)?;
                if let Some(ann) = &receipt.ann {
                    native_work = ann.visited_nodes;
                    let exact = self.search_vector_exact(
                        hybrid.vector_index,
                        &hybrid.vector_query,
                        hybrid.vector_limit,
                    )?;
                    oracle = Some(oracle_metrics(&ann.hits, &exact));
                }
                match receipt.outcome {
                    NativeHybridOutcome::Abstained => Vec::new(),
                    NativeHybridOutcome::Matches(matches) => matches
                        .into_iter()
                        .map(|matched| {
                            (
                                matched.object_id,
                                ConvergenceValue::Number(fusion_number(
                                    matched.explanation.fusion_score,
                                )),
                            )
                        })
                        .collect(),
                }
            }
        };
        if raw.len() > cap {
            return Err(ConvergenceError::SourceRowLimitExceeded);
        }
        let examined = raw.len();
        let mut rows = BTreeMap::new();
        for (id, value) in raw {
            if matches!(value, ConvergenceValue::Number(number) if !number.is_finite()) {
                return Err(ConvergenceError::InvalidNumber);
            }
            if rows.insert(id, value).is_some() {
                return Err(ConvergenceError::DuplicateObjectId);
            }
        }
        let (oracle_hits, oracle_overlap, oracle_recall_ppm) = oracle
            .map_or((None, None, None), |(hits, overlap, recall)| {
                (Some(hits), Some(overlap), Some(recall))
            });
        Ok((
            rows,
            ConvergenceSourceMetrics {
                strategy: strategy(source),
                limit_pushed_down: true,
                rows_examined: examined,
                rows_emitted: examined,
                native_work,
                oracle_hits,
                oracle_overlap,
                oracle_recall_ppm,
            },
        ))
    }

    fn read_structure_source(
        &self,
        source: &StructureSource,
        cap: usize,
    ) -> Result<Vec<(ObjectId, ConvergenceValue)>, ConvergenceError> {
        let probe = cap.saturating_add(1);
        match source {
            StructureSource::Scalar { key } => match self.get(key) {
                Some(value) => Ok(vec![(
                    decode_id(key)?,
                    ConvergenceValue::Bytes(value.to_vec()),
                )]),
                None => Ok(Vec::new()),
            },
            StructureSource::Hash { key } => self
                .hscan(key, None, probe)?
                .into_iter()
                .map(|entry| {
                    Ok((
                        decode_id(entry.field())?,
                        ConvergenceValue::Bytes(entry.value().to_vec()),
                    ))
                })
                .collect(),
            StructureSource::Set { key } => self
                .sscan(key, None, probe)?
                .into_iter()
                .map(|member| Ok((decode_id(&member)?, ConvergenceValue::Missing)))
                .collect(),
            StructureSource::List { key } => self
                .lrange(
                    key,
                    0,
                    i64::try_from(probe.saturating_sub(1)).unwrap_or(i64::MAX),
                )?
                .into_iter()
                .map(|item| Ok((decode_id(&item)?, ConvergenceValue::Missing)))
                .collect(),
            StructureSource::SortedSet { key } => self
                .zrange(
                    key,
                    0,
                    i64::try_from(probe.saturating_sub(1)).unwrap_or(i64::MAX),
                )?
                .into_iter()
                .map(|entry| {
                    Ok((
                        decode_id(entry.member())?,
                        ConvergenceValue::Number(entry.score()),
                    ))
                })
                .collect(),
            StructureSource::Stream { key } => self
                .xrange_stream(key, 0, u64::MAX, probe)?
                .into_iter()
                .flat_map(|(_, fields)| fields)
                .map(|(field, value)| Ok((decode_id(&field)?, ConvergenceValue::Bytes(value))))
                .collect(),
        }
    }
}

fn validate(plan: &ConvergencePlan) -> Result<(), ConvergenceError> {
    let limits = plan.limits;
    if limits.max_sources == 0
        || limits.max_sources > MAX_CONVERGENCE_SOURCES
        || limits.max_rows_per_source == 0
        || limits.max_rows_per_source > MAX_CONVERGENCE_ROWS
        || limits.max_output_rows == 0
        || limits.max_output_rows > MAX_CONVERGENCE_ROWS
        || limits.max_aggregates > MAX_CONVERGENCE_AGGREGATES
    {
        return Err(ConvergenceError::InvalidLimits);
    }
    if plan.sources.is_empty() {
        return Err(ConvergenceError::EmptyPlan);
    }
    if plan.sources.len() > limits.max_sources || plan.aggregates.len() > limits.max_aggregates {
        return Err(ConvergenceError::PlanLimitExceeded);
    }
    for source in &plan.sources {
        let requested = match source {
            ConvergenceSource::Structure(_) => None,
            ConvergenceSource::Lexical { limit, .. } | ConvergenceSource::Exact { limit, .. } => {
                Some(*limit)
            }
            ConvergenceSource::Ann { options, .. } => Some(options.k()),
            ConvergenceSource::Hybrid(value) => {
                if value.lexical_limit > limits.max_rows_per_source
                    || value.vector_limit > limits.max_rows_per_source
                {
                    return Err(ConvergenceError::PlanLimitExceeded);
                }
                Some(value.fusion.limit)
            }
        };
        if requested.is_some_and(|value| value == 0 || value > limits.max_rows_per_source) {
            return Err(ConvergenceError::PlanLimitExceeded);
        }
    }
    for aggregate in &plan.aggregates {
        match aggregate.operation {
            AggregateOperation::Count if aggregate.source.is_none() => {}
            AggregateOperation::Count => return Err(ConvergenceError::InvalidAggregate),
            _ if aggregate.source.is_some_and(|source| {
                source < plan.sources.len() && source_has_numeric_values(&plan.sources[source])
            }) => {}
            _ => return Err(ConvergenceError::InvalidAggregate),
        }
    }
    Ok(())
}

fn source_has_numeric_values(source: &ConvergenceSource) -> bool {
    !matches!(
        source,
        ConvergenceSource::Structure(StructureSource::Set { .. } | StructureSource::List { .. })
    )
}

fn strategy(source: &ConvergenceSource) -> ConvergenceStrategy {
    match source {
        ConvergenceSource::Structure(StructureSource::Scalar { .. }) => {
            ConvergenceStrategy::ScalarLookup
        }
        ConvergenceSource::Structure(StructureSource::Hash { .. }) => {
            ConvergenceStrategy::HashRange
        }
        ConvergenceSource::Structure(StructureSource::Set { .. }) => ConvergenceStrategy::SetRange,
        ConvergenceSource::Structure(StructureSource::List { .. }) => {
            ConvergenceStrategy::ListRange
        }
        ConvergenceSource::Structure(StructureSource::SortedSet { .. }) => {
            ConvergenceStrategy::SortedSetRange
        }
        ConvergenceSource::Structure(StructureSource::Stream { .. }) => {
            ConvergenceStrategy::StreamRange
        }
        ConvergenceSource::Lexical { .. } => ConvergenceStrategy::LexicalTopK,
        ConvergenceSource::Exact { .. } => ConvergenceStrategy::ExactVectorTopK,
        ConvergenceSource::Ann { .. } => ConvergenceStrategy::AnnTopK,
        ConvergenceSource::Hybrid(_) => ConvergenceStrategy::HybridRrf,
    }
}

fn decode_id(bytes: &[u8]) -> Result<ObjectId, ConvergenceError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| ConvergenceError::InvalidObjectId)?;
    ObjectId::new(u128::from_be_bytes(bytes)).map_err(|_| ConvergenceError::InvalidObjectId)
}

fn numeric_value(value: &ConvergenceValue) -> Result<Option<f64>, ConvergenceError> {
    let ConvergenceValue::Bytes(bytes) = value else {
        return Ok(match value {
            ConvergenceValue::Number(number) => Some(*number),
            ConvergenceValue::Missing | ConvergenceValue::Bytes(_) => None,
        });
    };
    let number = std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .ok_or(ConvergenceError::InvalidNumber)?;
    Ok(Some(if number == 0.0 { 0.0 } else { number }))
}

#[allow(clippy::cast_precision_loss)]
fn fusion_number(value: u64) -> f64 {
    // Native hybrid validation bounds this below 2^53, so conversion is exact.
    value as f64
}

fn oracle_metrics(
    approximate: &[crate::VectorHit],
    exact: &[crate::VectorHit],
) -> (usize, usize, u32) {
    let ids = approximate
        .iter()
        .map(|hit| hit.object_id)
        .collect::<BTreeSet<_>>();
    let overlap = exact
        .iter()
        .filter(|hit| ids.contains(&hit.object_id))
        .count();
    let recall = if exact.is_empty() {
        1_000_000
    } else {
        u32::try_from(overlap.saturating_mul(1_000_000) / exact.len()).unwrap_or(1_000_000)
    };
    (exact.len(), overlap, recall)
}

fn aggregate(
    rows: &[ConvergenceRow],
    specifications: &[AggregateSpec],
) -> Result<Vec<AggregateResult>, ConvergenceError> {
    specifications
        .iter()
        .map(|specification| match specification.operation {
            AggregateOperation::Count => Ok(AggregateResult::Count(
                u64::try_from(rows.len()).map_err(|_| ConvergenceError::NumericOverflow)?,
            )),
            operation => {
                let source = specification
                    .source
                    .ok_or(ConvergenceError::InvalidAggregate)?;
                let values = rows
                    .iter()
                    .map(|row| numeric_value(&row.values[source]))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten();
                let value = match operation {
                    AggregateOperation::Sum => values.reduce(|left, right| left + right),
                    AggregateOperation::Min => values.reduce(f64::min),
                    AggregateOperation::Max => values.reduce(f64::max),
                    AggregateOperation::Count => unreachable!(),
                };
                if value.is_some_and(|number| !number.is_finite()) {
                    return Err(ConvergenceError::NumericOverflow);
                }
                Ok(AggregateResult::Number(value))
            }
        })
        .collect()
}
