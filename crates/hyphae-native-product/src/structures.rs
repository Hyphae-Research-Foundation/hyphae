// SPDX-License-Identifier: Apache-2.0

//! Product-owned catalogued structure and explicit transaction contracts.

#![allow(missing_docs)]

use crate::{
    CanonicalF64, ObjectId, ProductCommitReceipt, ProductDurability, ProductSqlResult,
    ProductTransactionId, ProductValue, ProductVector, StructureKind,
};

/// Catalog-bound binary key in one native structure keyspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductStructureKey {
    /// Stable catalog identity of the keyspace or compatible structure object.
    pub keyspace: ObjectId,
    /// Exact caller key inside the catalogued keyspace.
    pub key: Vec<u8>,
}

/// One atomic structure mutation inside a product batch or explicit transaction.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ProductStructureMutation {
    /// Set a string or binary scalar and optional absolute expiry.
    StringSet {
        key: ProductStructureKey,
        value: Vec<u8>,
        expires_at_micros: Option<i64>,
    },
    /// Delete one visible string or binary scalar.
    StringDelete { key: ProductStructureKey },
    /// Atomically add to one canonical signed counter.
    CounterAdd {
        key: ProductStructureKey,
        delta: i64,
    },
    /// Create an explicitly typed empty collection.
    Create {
        key: ProductStructureKey,
        family: StructureKind,
    },
    /// Delete an explicitly typed collection.
    Delete {
        key: ProductStructureKey,
        family: StructureKind,
    },
    /// Replace one top-level structure expiry.
    Expire {
        key: ProductStructureKey,
        family: StructureKind,
        expires_at_micros: i64,
    },
    /// Insert or replace one hash field.
    HashSet {
        key: ProductStructureKey,
        field: Vec<u8>,
        value: Vec<u8>,
    },
    /// Delete one hash field.
    HashDelete {
        key: ProductStructureKey,
        field: Vec<u8>,
    },
    /// Add to one canonical signed hash counter field.
    HashCounterAdd {
        key: ProductStructureKey,
        field: Vec<u8>,
        delta: i64,
    },
    /// Replace one hash field expiry.
    HashExpireField {
        key: ProductStructureKey,
        field: Vec<u8>,
        expires_at_micros: i64,
    },
    /// Push one exact value onto a list.
    ListPush {
        key: ProductStructureKey,
        side: ProductListSide,
        value: Vec<u8>,
    },
    /// Pop one exact value from a list.
    ListPop {
        key: ProductStructureKey,
        side: ProductListSide,
    },
    /// Add one exact set member.
    SetAdd {
        key: ProductStructureKey,
        member: Vec<u8>,
    },
    /// Remove one exact set member.
    SetRemove {
        key: ProductStructureKey,
        member: Vec<u8>,
    },
    /// Insert or rescore one sorted-set member.
    SortedSetAdd {
        key: ProductStructureKey,
        member: Vec<u8>,
        score: CanonicalF64,
    },
    /// Add a canonical delta to one member's score (minor 6).
    SortedSetIncrement {
        key: ProductStructureKey,
        member: Vec<u8>,
        delta: CanonicalF64,
    },
    /// Remove and return the lowest/highest member (minor 6).
    SortedSetPop {
        key: ProductStructureKey,
        /// False pops the lowest `(score, member)`; true the highest.
        highest: bool,
    },
    /// Remove one exact sorted-set member.
    SortedSetRemove {
        key: ProductStructureKey,
        member: Vec<u8>,
    },
    /// Append one exact stream field map.
    StreamAdd {
        key: ProductStructureKey,
        fields: Vec<ProductHashEntry>,
    },
}

/// One immutable structure read request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ProductStructureReadRequest {
    /// Read one string or binary scalar.
    StringGet { key: ProductStructureKey },
    /// Read one canonical signed counter.
    CounterGet { key: ProductStructureKey },
    /// Read one top-level structure TTL.
    Ttl {
        key: ProductStructureKey,
        family: StructureKind,
    },
    /// Read one hash field.
    HashGet {
        key: ProductStructureKey,
        field: Vec<u8>,
    },
    /// Read one hash field TTL.
    HashFieldTtl {
        key: ProductStructureKey,
        field: Vec<u8>,
    },
    /// Scan hash fields in ascending exact-byte order.
    HashScan {
        key: ProductStructureKey,
        start_after: Option<Vec<u8>>,
        limit: usize,
    },
    /// Read one hash cardinality.
    HashLength { key: ProductStructureKey },
    /// Read one inclusive signed list range.
    ListRange {
        key: ProductStructureKey,
        start: i64,
        stop: i64,
    },
    /// Read one list cardinality.
    ListLength { key: ProductStructureKey },
    /// Test one exact set member.
    SetContains {
        key: ProductStructureKey,
        member: Vec<u8>,
    },
    /// Scan set members in ascending exact-byte order.
    SetMembers {
        key: ProductStructureKey,
        start_after: Option<Vec<u8>>,
        limit: usize,
    },
    /// Read one set cardinality.
    SetCardinality { key: ProductStructureKey },
    /// Evaluate complete bounded set algebra over key positions in one keyspace.
    SetAlgebra {
        keyspace: ObjectId,
        operation: ProductSetAlgebraOperation,
        keys: Vec<Vec<u8>>,
        output_member_limit: usize,
        visit_limit: usize,
    },
    /// Read one sorted-set member score.
    SortedSetScore {
        key: ProductStructureKey,
        member: Vec<u8>,
    },
    /// Read one sorted-set member rank.
    SortedSetRank {
        key: ProductStructureKey,
        member: Vec<u8>,
        order: ProductSortedSetOrder,
    },
    /// Read one inclusive signed sorted-set rank range.
    SortedSetRange {
        key: ProductStructureKey,
        start: i64,
        stop: i64,
        order: ProductSortedSetOrder,
    },
    /// Read one sorted-set cardinality.
    SortedSetCardinality { key: ProductStructureKey },
    /// Read one bounded inclusive stream-ID range.
    StreamRange {
        key: ProductStructureKey,
        start: u64,
        end: u64,
        limit: usize,
    },
    /// Read one bounded canonical score range (native protocol minor 6).
    SortedSetScoreRange {
        key: ProductStructureKey,
        lower: ProductScoreBound,
        upper: ProductScoreBound,
        offset: usize,
        limit: usize,
        order: ProductSortedSetOrder,
    },
    /// Scan hash fields in descending exact-byte order (minor 6).
    HashScanReverse {
        key: ProductStructureKey,
        start_before: Option<Vec<u8>>,
        limit: usize,
    },
    /// Scan one bounded binary-glob page of hash fields (minor 6).
    HashScanMatch {
        key: ProductStructureKey,
        pattern: Vec<u8>,
        start_after: Option<Vec<u8>>,
        output_limit: usize,
        visit_limit: usize,
        match_step_limit: usize,
    },
    /// Scan one bounded binary-glob page of visible top-level keys across
    /// every structure family of one keyspace (minor 6).
    KeyScanMatch {
        keyspace: ObjectId,
        pattern: Vec<u8>,
        start_after: Option<Vec<u8>>,
        output_limit: usize,
        visit_limit: usize,
        match_step_limit: usize,
    },
}

/// One canonical sorted-set score endpoint.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum ProductScoreBound {
    /// No bound on this endpoint.
    Unbounded,
    /// Endpoint included in the interval.
    Inclusive(f64),
    /// Endpoint excluded from the interval.
    Exclusive(f64),
}

/// Snapshot-bound result of one product structure read.
pub type ProductStructureRead = crate::ProductRead<ProductStructureReadResult>;

/// Logical result of one structure read.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductStructureReadResult {
    /// Optional exact binary value.
    Value(Option<Vec<u8>>),
    /// Canonical signed counter value, absent when the key is missing.
    Counter(Option<i64>),
    /// TTL state.
    Ttl(crate::ProductTtl),
    /// Hash fields in ascending exact-byte order.
    HashEntries(Vec<ProductHashEntry>),
    /// Exact cardinality.
    Count(usize),
    /// Boolean membership result.
    Boolean(bool),
    /// Exact binary values in canonical operation order.
    Values(Vec<Vec<u8>>),
    /// Complete set-algebra result and consumed member visits.
    SetAlgebra {
        members: Vec<Vec<u8>>,
        visited: usize,
    },
    /// Optional canonical sorted-set score.
    SortedSetScore(Option<CanonicalF64>),
    /// Optional zero-based sorted-set rank.
    SortedSetRank(Option<usize>),
    /// Sorted-set members in requested canonical order.
    SortedSetEntries(Vec<ProductSortedSetEntry>),
    /// Stream entries in ascending ID order.
    StreamEntries(Vec<ProductStreamEntry>),
    /// One bounded key glob-scan page across families (minor 6).
    KeyPage {
        /// Matched keys with their family, ascending by key bytes.
        entries: Vec<ProductKeyEntry>,
        /// Exclusive physical continuation when more candidates remain.
        continuation: Option<Vec<u8>>,
        /// Why the page stopped.
        stop: ProductHashScanStop,
        /// Physical candidates visited.
        visited: usize,
        /// Matcher steps consumed.
        match_steps: usize,
    },
    /// One bounded glob-scan page with physical progress (minor 6).
    HashPage {
        /// Matched fields in ascending exact-byte order.
        entries: Vec<ProductHashEntry>,
        /// Exclusive physical continuation, present when more candidates
        /// remain even if this page matched nothing.
        continuation: Option<Vec<u8>>,
        /// Why the page stopped.
        stop: ProductHashScanStop,
        /// Physical candidates visited.
        visited: usize,
        /// Matcher steps consumed.
        match_steps: usize,
    },
}

/// Stop reason for one bounded glob-scan page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductHashScanStop {
    /// The selected physical range has no later candidate.
    Exhausted,
    /// The page emitted its requested live-match count.
    OutputLimit,
    /// The page consumed its requested physical candidate count.
    VisitLimit,
}

/// Typed result returned while staging one structure mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductStructureMutationResult {
    /// No additional logical result.
    Unit,
    /// Canonical signed counter result.
    Integer(i64),
    /// Boolean mutation outcome.
    Boolean(bool),
    /// New list length.
    Count(usize),
    /// Optional popped list value.
    Value(Option<Vec<u8>>),
    /// Stable appended stream identity.
    StreamId(u64),
    /// New canonical sorted-set score (minor 6).
    Score(CanonicalF64),
    /// Optional popped sorted-set member and score (minor 6).
    PoppedEntry(Option<ProductSortedSetEntry>),
}

/// One scanned key with its structure family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductKeyEntry {
    /// Exact binary key.
    pub key: Vec<u8>,
    /// Structure family owning the key.
    pub family: StructureKind,
}

/// Side selected by one list push or pop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductListSide {
    /// Head of the list.
    Left,
    /// Tail of the list.
    Right,
}

/// Stable ordering selected for a sorted-set range or rank.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductSortedSetOrder {
    /// Lowest score and exact member bytes first.
    Ascending,
    /// Highest score and reverse exact member bytes first.
    Descending,
}

/// Complete set-algebra operation.
pub use hyphae_native_runtime::SetAlgebraOperation as ProductSetAlgebraOperation;

/// One product-owned hash field/value result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductHashEntry {
    /// Exact binary field.
    pub field: Vec<u8>,
    /// Exact binary value.
    pub value: Vec<u8>,
}

/// One product-owned sorted-set member and canonical score.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSortedSetEntry {
    /// Exact binary member.
    pub member: Vec<u8>,
    /// Canonical binary64 score.
    pub score: CanonicalF64,
}

/// One product-owned stream entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductStreamEntry {
    /// Stable monotonically increasing stream identity.
    pub id: u64,
    /// Field/value pairs in caller insertion order.
    pub fields: Vec<ProductHashEntry>,
}

/// One explicit session-local all-engine transaction handle.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProductTransactionHandle(u64);

impl ProductTransactionHandle {
    /// Constructs a nonzero handle from its portable representation.
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the primitive handle.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Explicit transaction lifecycle visible to its owning session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductExplicitTransactionStatus {
    /// The handle is not retained by this session.
    Unknown,
    /// A detached all-engine batch remains private.
    Active {
        handle: ProductTransactionHandle,
        read_csn: Option<u64>,
        staged_operations: usize,
        durability: ProductDurability,
    },
    /// The complete batch committed under one CSN.
    Committed {
        handle: ProductTransactionHandle,
        staged_operations: usize,
        receipt: ProductCommitReceipt,
    },
    /// The complete private batch was discarded.
    RolledBack {
        handle: ProductTransactionHandle,
        discarded_operations: usize,
    },
    /// Publication may have completed and retained status resolution is required.
    OutcomeUnknown {
        handle: ProductTransactionHandle,
        transaction_id: ProductTransactionId,
        staged_operations: usize,
    },
}

/// One explicit transaction stage acknowledgement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductTransactionStageReceipt {
    /// Owning session-local transaction.
    pub handle: ProductTransactionHandle,
    /// One-based successfully staged operation ordinal.
    pub operation_ordinal: usize,
    /// Whether this operation added at least one physical mutation.
    pub changed: bool,
    /// Typed stage result.
    pub result: ProductTransactionStageResult,
}

/// Typed result of one successful explicit-transaction stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductTransactionStageResult {
    /// SQL DML completion.
    Sql(ProductSqlResult),
    /// Structure mutation result.
    Structure(ProductStructureMutationResult),
    /// Lexical document mutation staged.
    Search,
    /// Vector mutation result, false only for deletion of an absent vector.
    Vector(bool),
}

/// One transaction-bound SQL DML request.
#[derive(Clone, Debug)]
pub struct ProductTransactionSqlMutation {
    /// Bounded SQL `INSERT`, `UPDATE`, or `DELETE` text.
    pub statement: String,
    /// Canonical typed parameter values.
    pub parameters: Vec<ProductValue>,
}

/// One transaction-bound lexical document mutation.
#[derive(Clone, Debug)]
pub enum ProductTransactionSearchMutation {
    /// Insert one new document.
    Index {
        index: ObjectId,
        document_id: Vec<u8>,
        text: String,
    },
    /// Replace one existing document.
    Replace {
        index: ObjectId,
        document_id: Vec<u8>,
        text: String,
    },
    /// Delete one existing document.
    Delete {
        index: ObjectId,
        document_id: Vec<u8>,
    },
    /// Replace or insert one complete product document — source text,
    /// doc-values, and named vectors — under the transaction's CSN.
    Document {
        /// Search collection receiving the document.
        collection: ObjectId,
        /// Complete validated cross-engine document.
        document: crate::ProductDocument,
    },
}

/// One transaction-bound native vector mutation.
#[derive(Clone, Debug)]
pub enum ProductTransactionVectorMutation {
    /// Insert or replace one stable object vector.
    Upsert {
        index: ObjectId,
        object_id: ObjectId,
        vector: ProductVector,
    },
    /// Delete one stable object vector.
    Delete {
        index: ObjectId,
        object_id: ObjectId,
    },
}

/// Result of one explicit transaction commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductExplicitCommitReceipt {
    /// Session-local handle that was consumed.
    pub handle: ProductTransactionHandle,
    /// Number of successfully staged operations committed.
    pub staged_operations: usize,
    /// One commit and CSN shared by SQL, structures, lexical, and vector state.
    pub commit: ProductCommitReceipt,
}

/// Result of one explicit transaction rollback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductRollbackReceipt {
    /// Session-local handle that was consumed.
    pub handle: ProductTransactionHandle,
    /// Number of successfully staged operations discarded.
    pub discarded_operations: usize,
}

/// Product restore progress delivered to an optional embedded observer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductRestoreProgress {
    /// Restore lifecycle phase.
    pub phase: crate::RestorePhase,
}

/// Complete successful restore result.
pub type ProductRestoreInfo = crate::RestoreInfo;
