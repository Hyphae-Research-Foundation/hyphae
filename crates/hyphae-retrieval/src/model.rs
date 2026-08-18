// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

/// One candidate vector with a globally unique binary key.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorRecord {
    /// Globally unique nonempty key.
    pub key: Vec<u8>,
    /// Finite nonzero semantic vector.
    pub vector: Vec<f64>,
}

impl VectorRecord {
    /// Creates a candidate. Vector invariants are validated globally during
    /// search so malformed shards cannot yield partial results.
    pub fn new(key: impl Into<Vec<u8>>, vector: impl Into<Vec<f64>>) -> Self {
        Self {
            key: key.into(),
            vector: vector.into(),
        }
    }
}

/// Exact cosine search and abstention policy.
#[derive(Clone, Debug, PartialEq)]
pub struct RetrievalRequest {
    /// Finite nonzero query vector.
    pub query: Vec<f64>,
    /// Maximum returned matches; must be nonzero.
    pub limit: usize,
    /// Inclusive minimum cosine score in `[-1, 1]`.
    pub minimum_score: f64,
    /// Minimum difference between the best and runner-up score in `[0, 2]`.
    pub minimum_margin: f64,
}

/// Runtime and shape limits for exact global retrieval.
#[derive(Clone, Debug, PartialEq)]
pub struct RetrievalLimits {
    /// Maximum candidates inspected across every shard.
    pub max_candidates: u64,
    /// Maximum vector dimension.
    pub max_dimensions: usize,
    /// Maximum requested result count.
    pub max_returned: usize,
    /// Cooperative monotonic timeout.
    pub timeout: Duration,
}

impl Default for RetrievalLimits {
    fn default() -> Self {
        Self {
            max_candidates: 100_000,
            max_dimensions: 4_096,
            max_returned: 1_000,
            timeout: Duration::from_secs(30),
        }
    }
}

/// One globally ranked exact match.
#[derive(Clone, Debug, PartialEq)]
pub struct RetrievalMatch {
    /// Candidate key.
    pub key: Vec<u8>,
    /// Exact cosine similarity in `[-1, 1]`.
    pub score: f64,
}

/// Stable reason why semantic retrieval declined to return matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbstentionReason {
    /// No candidates were available.
    NoCandidates,
    /// The best score was below the configured threshold.
    BelowThreshold,
    /// The best and runner-up scores were too close.
    Ambiguous,
}

/// Evidence for a normal abstention outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct Abstention {
    /// Stable machine-readable reason.
    pub reason: AbstentionReason,
    /// Best observed score when candidates existed.
    pub best_score: Option<f64>,
    /// Runner-up score when at least two candidates existed.
    pub runner_up_score: Option<f64>,
    /// Global candidates inspected.
    pub scanned_candidates: u64,
}

/// Complete exact retrieval outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum RetrievalOutcome {
    /// Policy accepted the globally ranked matches.
    Matches {
        /// Matches after global sort and final limit.
        matches: Vec<RetrievalMatch>,
        /// Global candidates inspected.
        scanned_candidates: u64,
    },
    /// Policy explicitly declined to return semantic matches.
    Abstained(Abstention),
}
