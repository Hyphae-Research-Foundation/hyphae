// SPDX-License-Identifier: Apache-2.0

//! Immutable copy-on-write B+tree over verified Hyphae native pages.

use std::{
    collections::BTreeSet,
    mem::size_of,
    ops::{Bound, ControlFlow, Range},
    sync::Arc,
};

use hyphae_native_pages::{
    BufferPool, BufferPoolError, PAGE_PAYLOAD_SIZE, PAGE_SIZE, Page, PageFrame, PageKind,
    PageStore, PageStoreError, UnpublishedTail,
};
use hyphae_native_types::{Csn, PageGeneration, PageId};
use thiserror::Error;

const LEAF_MAGIC: &[u8; 8] = b"HYBTLF01";
const INTERNAL_MAGIC: &[u8; 8] = b"HYBTIN01";
const FORMAT_VERSION: u16 = 1;
const LEAF_HEADER_SIZE: usize = 16;
const INTERNAL_HEADER_SIZE: usize = 24;
const MAX_TREE_HEIGHT: usize = 64;
const ALLOCATOR_ALLOCATION_OVERHEAD_BYTES: usize = 32;
const MAX_LEAF_ENTRY_COUNT: usize = (PAGE_PAYLOAD_SIZE - LEAF_HEADER_SIZE) / 8;
const DECODED_NODE_MEMORY_BOUND: usize = PAGE_SIZE
    + PAGE_PAYLOAD_SIZE
    + MAX_LEAF_ENTRY_COUNT * size_of::<LeafEntry>()
    + MAX_LEAF_ENTRY_COUNT * 2 * ALLOCATOR_ALLOCATION_OVERHEAD_BYTES
    + ALLOCATOR_ALLOCATION_OVERHEAD_BYTES;
/// Maximum canonical inline B+tree key size.
pub const BTREE_MAX_KEY_SIZE: usize = 4_096;

#[cfg(test)]
thread_local! {
    static PREFIX_REPLACEMENT_APPENDED_PAGES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Owned canonical binary key/value pair returned by a materialized scan.
pub type KeyValue = (Vec<u8>, Vec<u8>);

/// One value range pinned inside a verified immutable buffer-pool frame.
#[derive(Clone, Debug)]
pub struct PinnedValue {
    frame: Arc<PageFrame>,
    range: Range<usize>,
}

impl PinnedValue {
    /// Returns the borrowed canonical value bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.frame.page().payload()[self.range.clone()]
    }
}

/// B+tree codec, storage, or semantic failure.
#[derive(Debug, Error)]
pub enum BTreeError {
    /// Native page storage failed.
    #[error(transparent)]
    Store(#[from] PageStoreError),
    /// Native buffer-pool lookup failed.
    #[error(transparent)]
    BufferPool(#[from] BufferPoolError),
    /// A page kind does not match its B+tree node payload.
    #[error("native B+tree root or child has the wrong page kind")]
    WrongPageKind,
    /// A node payload is truncated or has trailing bytes.
    #[error("native B+tree node payload length is invalid")]
    InvalidLength,
    /// Node magic, version, or reserved bytes are invalid.
    #[error("native B+tree node preamble is invalid")]
    InvalidPreamble,
    /// Node entry count is impossible for its payload.
    #[error("native B+tree node entry count is invalid")]
    InvalidCount,
    /// Keys are not strictly increasing.
    #[error("native B+tree keys are not in strict canonical order")]
    NoncanonicalKeyOrder,
    /// Internal children and separator keys are inconsistent.
    #[error("native B+tree separator does not equal its right-child minimum")]
    InvalidSeparator,
    /// A child page identity is zero.
    #[error("native B+tree contains a zero child page identity")]
    ZeroChild,
    /// A key or value length exceeds canonical fields.
    #[error("native B+tree key or value length exceeds u32")]
    LengthOverflow,
    /// A key is too large to preserve internal split invariants.
    #[error("native B+tree key exceeds {BTREE_MAX_KEY_SIZE} bytes")]
    KeyTooLarge,
    /// One leaf entry cannot fit in a native page.
    #[error("native B+tree entry cannot fit in one leaf page")]
    EntryTooLarge,
    /// A node cannot be split into two valid native pages.
    #[error("native B+tree node has no valid split")]
    NoValidSplit,
    /// Insert-only mode found an existing key.
    #[error("native B+tree key already exists")]
    DuplicateKey,
    /// Traversal exceeded the bounded tree height.
    #[error("native B+tree exceeds the maximum supported height")]
    HeightExceeded,
    /// Traversal reached the same page twice.
    #[error("native B+tree contains a cycle or duplicate child reference")]
    Cycle,
    /// Leaves do not occur at one common tree depth.
    #[error("native B+tree leaves are not balanced at one depth")]
    Unbalanced,
    /// A reachable node was created after the root's visible commit.
    #[error("native B+tree contains a node from a future commit")]
    FuturePage,
    /// A planned leaf segment belongs to another immutable tree root.
    #[error("native B+tree segment belongs to another immutable root")]
    ForeignSegment,
    /// Cooperative cancellation interrupted a bounded tree rewrite.
    #[error("native B+tree mutation was cancelled")]
    Cancelled,
    /// Prefix replacement did not observe the caller's exact expected keys.
    #[error("native B+tree prefix replacement observed unexpected existing keys")]
    PrefixContentsChanged,
}

/// Result of one copy-on-write B+tree mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationResult {
    /// New immutable tree root.
    pub tree: BTree,
    /// Prior value for upsert, when one existed.
    pub previous: Option<Vec<u8>>,
    /// Number of newly appended pages.
    pub pages_written: usize,
}

/// Result of one ordered multi-key copy-on-write mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchMutationResult {
    /// New immutable tree root.
    pub tree: BTree,
    /// Number of newly appended pages.
    pub pages_written: usize,
}

/// Conservatively bounded structural memory for one immutable prefix rewrite.
///
/// The plan is authoritative only for the captured page-file generation and
/// tree root. Payload ownership remains the caller's separate accounting;
/// this bound covers B+tree traversal, leaf references, exact-key validation,
/// rewrite headers, parent assembly, allocator overhead, and one decoded node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixReplacementStructuralPlan {
    generation: PageGeneration,
    root: Option<PageId>,
    leaf_count: usize,
    reachable_page_count: usize,
    maximum_stack_depth: usize,
    leaf_boundary_key_bytes: usize,
    maximum_key_length: usize,
    maximum_replacement_entries: usize,
    maximum_replacement_key_length: usize,
    structural_peak_memory_bytes: usize,
}

/// Scalar ceilings used to admit B+tree structural work before replacement
/// keys and values are materialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrefixReplacementStructuralLimits {
    maximum_replacement_entries: usize,
    maximum_replacement_key_length: usize,
}

impl PrefixReplacementStructuralLimits {
    /// Creates conservative scalar ceilings for one later replacement batch.
    ///
    /// # Errors
    ///
    /// Rejects a key ceiling larger than the canonical inline B+tree limit.
    pub fn new(
        maximum_replacement_entries: usize,
        maximum_replacement_key_length: usize,
    ) -> Result<Self, BTreeError> {
        if maximum_replacement_key_length > BTREE_MAX_KEY_SIZE {
            return Err(BTreeError::KeyTooLarge);
        }
        Ok(Self {
            maximum_replacement_entries,
            maximum_replacement_key_length,
        })
    }

    /// Returns replacement entries covered by this bound.
    pub const fn maximum_replacement_entries(self) -> usize {
        self.maximum_replacement_entries
    }

    /// Returns replacement key bytes covered per entry.
    pub const fn maximum_replacement_key_length(self) -> usize {
        self.maximum_replacement_key_length
    }
}

/// Owned replacement payload and borrowed exact-prefix authority for one
/// planned unpublished-tail rewrite.
#[derive(Debug)]
pub struct PrefixReplacementBatch<'a> {
    /// Commit sequence recorded on newly appended B+tree pages.
    pub creating_csn: Csn,
    /// Ordered, non-overlapping prefixes replaced atomically.
    pub prefixes: &'a [Vec<u8>],
    /// Exact ordered keys that must currently exist below the prefixes.
    pub expected_keys: &'a [Vec<u8>],
    /// Complete ordered replacement key/value set below the prefixes.
    pub replacements: Vec<KeyValue>,
}

impl PrefixReplacementStructuralPlan {
    /// Returns the conservative additional memory required by the B+tree
    /// rewrite, excluding caller-owned replacement key/value payloads.
    pub const fn structural_peak_memory_bytes(&self) -> usize {
        self.structural_peak_memory_bytes
    }

    /// Returns immutable leaves referenced by this root.
    pub const fn leaf_count(&self) -> usize {
        self.leaf_count
    }

    /// Returns all reachable B+tree pages verified by the streaming planner.
    pub const fn reachable_page_count(&self) -> usize {
        self.reachable_page_count
    }

    /// Returns the planner's maximum DFS stack depth.
    pub const fn maximum_stack_depth(&self) -> usize {
        self.maximum_stack_depth
    }
}

trait BTreePageRead {
    fn read_btree_page(&self, page: PageId) -> Result<Page, PageStoreError>;
}

impl BTreePageRead for PageStore {
    fn read_btree_page(&self, page: PageId) -> Result<Page, PageStoreError> {
        self.read(page)
    }
}

impl BTreePageRead for UnpublishedTail<'_> {
    fn read_btree_page(&self, page: PageId) -> Result<Page, PageStoreError> {
        self.read(page)
    }
}

trait BTreePageWrite: BTreePageRead {
    fn append_btree_page(
        &mut self,
        kind: PageKind,
        creating_csn: Option<Csn>,
        payload: Vec<u8>,
    ) -> Result<PageId, PageStoreError>;
}

impl BTreePageWrite for PageStore {
    fn append_btree_page(
        &mut self,
        kind: PageKind,
        creating_csn: Option<Csn>,
        payload: Vec<u8>,
    ) -> Result<PageId, PageStoreError> {
        self.append(kind, creating_csn, None, payload)
    }
}

impl BTreePageWrite for UnpublishedTail<'_> {
    fn append_btree_page(
        &mut self,
        kind: PageKind,
        creating_csn: Option<Csn>,
        payload: Vec<u8>,
    ) -> Result<PageId, PageStoreError> {
        self.append(kind, creating_csn, None, payload)
    }
}

/// Immutable root identity for one binary B+tree generation.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct BTree {
    root: Option<PageId>,
}

/// One immutable leaf segment selected by a bounded B+tree plan.
///
/// Construction is private so callers cannot invent a page/range identity.
/// The originating root is retained and checked again before execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BTreeSegment {
    root: PageId,
    page: PageId,
    minimum: Vec<u8>,
    maximum: Vec<u8>,
    entry_count: usize,
    lower: Bound<Vec<u8>>,
    upper: Bound<Vec<u8>>,
}

impl BTreeSegment {
    /// Immutable leaf-page identity.
    pub const fn page_id(&self) -> PageId {
        self.page
    }

    /// First canonical key physically stored in this leaf.
    pub fn minimum_key(&self) -> &[u8] {
        &self.minimum
    }

    /// Last canonical key physically stored in this leaf.
    pub fn maximum_key(&self) -> &[u8] {
        &self.maximum
    }

    /// Complete physical entries in the leaf before query-bound filtering.
    pub const fn entry_count(&self) -> usize {
        self.entry_count
    }
}

impl BTree {
    /// Creates an empty tree without a physical root page.
    pub const fn empty() -> Self {
        Self { root: None }
    }

    /// Binds an existing root for later verified traversal.
    pub const fn from_root(root: PageId) -> Self {
        Self { root: Some(root) }
    }

    /// Returns the immutable physical root, or `None` for an empty tree.
    pub const fn root(self) -> Option<PageId> {
        self.root
    }

    /// Plans immutable leaf segments intersecting one canonical key range.
    ///
    /// Internal separator ranges prune unreachable subtrees before their leaf
    /// pages are read. Returned segments remain in canonical key order and
    /// retain the exact query bounds for independent execution.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt reached pages, cycles, or excessive tree
    /// height.
    pub fn plan_range_segments(
        self,
        store: &PageStore,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> Result<Vec<BTreeSegment>, BTreeError> {
        let Some(root) = self.root else {
            return Ok(Vec::new());
        };
        if range_is_empty(lower, upper) {
            return Ok(Vec::new());
        }
        let mut segments = Vec::new();
        let mut visited = BTreeSet::new();
        plan_range_segments_node(
            store,
            root,
            root,
            lower,
            upper,
            0,
            &mut visited,
            &mut segments,
        )?;
        Ok(segments)
    }

    /// Executes one previously planned leaf segment.
    ///
    /// The page is revalidated against its planned range and summary before
    /// returning only entries inside the original query bounds.
    ///
    /// # Errors
    ///
    /// Rejects a segment from another root and any changed, missing, corrupt,
    /// or non-leaf page.
    pub fn scan_planned_segment(
        self,
        store: &PageStore,
        segment: &BTreeSegment,
    ) -> Result<Vec<KeyValue>, BTreeError> {
        if self.root != Some(segment.root) {
            return Err(BTreeError::ForeignSegment);
        }
        let Node::Leaf(entries) = read_node(store, segment.page)? else {
            return Err(BTreeError::WrongPageKind);
        };
        let minimum = entries
            .first()
            .ok_or(BTreeError::InvalidCount)?
            .key
            .as_slice();
        let maximum = entries
            .last()
            .ok_or(BTreeError::InvalidCount)?
            .key
            .as_slice();
        if minimum != segment.minimum
            || maximum != segment.maximum
            || entries.len() != segment.entry_count
        {
            return Err(BTreeError::InvalidSeparator);
        }
        let lower = borrowed_bound(&segment.lower);
        let upper = borrowed_bound(&segment.upper);
        Ok(entries
            .into_iter()
            .filter(|entry| {
                key_satisfies_lower(&entry.key, lower) && key_satisfies_upper(&entry.key, upper)
            })
            .map(|entry| (entry.key, entry.value))
            .collect())
    }

    /// Verifies the complete tree and returns its node height.
    ///
    /// An empty tree has height zero and a single leaf has height one.
    ///
    /// # Errors
    ///
    /// Returns an error for any corruption, cycle, unbalanced leaf depth, or
    /// excessive height.
    pub fn height(self, store: &PageStore) -> Result<usize, BTreeError> {
        let Some(root) = self.root else {
            return Ok(0);
        };
        let mut visited = BTreeSet::new();
        Ok(validate_node(store, root, None, 0, &mut visited)?.height)
    }

    /// Verifies the complete tree and returns its reachable node-page count.
    ///
    /// Superseded copy-on-write pages that are no longer reachable from this
    /// root are deliberately excluded.
    ///
    /// # Errors
    ///
    /// Returns an error for any corruption, cycle, unbalanced leaf depth, or
    /// excessive height.
    pub fn reachable_page_count(self, store: &PageStore) -> Result<usize, BTreeError> {
        let Some(root) = self.root else {
            return Ok(0);
        };
        let mut visited = BTreeSet::new();
        validate_node(store, root, None, 0, &mut visited)?;
        Ok(visited.len())
    }

    /// Performs a binary point lookup.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt pages/nodes, I/O, cycles, or excessive
    /// height.
    pub fn get(self, store: &PageStore, key: &[u8]) -> Result<Option<Vec<u8>>, BTreeError> {
        self.get_direct(store, key)
    }

    /// Performs a direct point lookup against a not-yet-finalized candidate.
    ///
    /// The read cannot enter a shared [`BufferPool`], so rolling the candidate
    /// back cannot leave stale bytes for a reused page identity.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt pages/nodes, I/O, cycles, or excessive
    /// height.
    pub fn get_unpublished(
        self,
        unpublished: &UnpublishedTail<'_>,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, BTreeError> {
        self.get_direct(unpublished, key)
    }

    fn get_direct<S: BTreePageRead>(
        self,
        store: &S,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, BTreeError> {
        let Some(mut page_id) = self.root else {
            return Ok(None);
        };
        let mut visited = [0_u64; MAX_TREE_HEIGHT];
        for depth in 0..MAX_TREE_HEIGHT {
            if visited[..depth].contains(&page_id.get()) {
                return Err(BTreeError::Cycle);
            }
            visited[depth] = page_id.get();
            let page = store.read_btree_page(page_id)?;
            match lookup_page(&page, key)? {
                LookupStep::Value(range) => {
                    return Ok(range.map(|range| page.payload()[range].to_vec()));
                }
                LookupStep::Descend(child) => page_id = child,
            }
        }
        Err(BTreeError::HeightExceeded)
    }

    /// Visits a candidate prefix directly without caching or materializing the
    /// complete range.
    ///
    /// Returning [`ControlFlow::Break`] stops before reading the remaining
    /// candidate pages. Keys are verified in global canonical order.
    ///
    /// # Errors
    ///
    /// Returns an error for page, codec, key-order, cycle, or height failures
    /// reached before the visitor stops.
    pub fn visit_prefix_unpublished<F>(
        self,
        unpublished: &UnpublishedTail<'_>,
        prefix: &[u8],
        visitor: F,
    ) -> Result<ControlFlow<()>, BTreeError>
    where
        F: FnMut(&[u8], &[u8]) -> ControlFlow<()>,
    {
        let Some(root) = self.root else {
            return Ok(ControlFlow::Continue(()));
        };
        let prefix_upper = prefix_upper_bound(prefix);
        DirectPrefixVisitor {
            store: unpublished,
            prefix,
            prefix_upper: prefix_upper.as_deref(),
            visited: BTreeSet::new(),
            last_key: None,
            visitor,
        }
        .visit_node(root, 0)
    }

    /// Performs a binary point lookup through a bounded verified buffer pool.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt pages/nodes, I/O, buffer exhaustion,
    /// cycles, or excessive height.
    pub fn get_cached(
        self,
        store: &PageStore,
        pool: &BufferPool,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, BTreeError> {
        Ok(self
            .get_cached_pinned(store, pool, key)?
            .map(|value| value.bytes().to_vec()))
    }

    /// Performs an allocation-free node traversal and returns a value pinned
    /// inside its verified immutable buffer-pool frame.
    ///
    /// The returned handle keeps the leaf frame pinned until it is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt pages/nodes, I/O, buffer exhaustion,
    /// cycles, or excessive height.
    pub fn get_cached_pinned(
        self,
        store: &PageStore,
        pool: &BufferPool,
        key: &[u8],
    ) -> Result<Option<PinnedValue>, BTreeError> {
        let Some(mut page_id) = self.root else {
            return Ok(None);
        };
        let mut visited = [0_u64; MAX_TREE_HEIGHT];
        for depth in 0..MAX_TREE_HEIGHT {
            if visited[..depth].contains(&page_id.get()) {
                return Err(BTreeError::Cycle);
            }
            visited[depth] = page_id.get();
            let frame = pool.get_or_load(store, page_id)?;
            match lookup_page(frame.page(), key)? {
                LookupStep::Value(range) => {
                    return Ok(range.map(|range| PinnedValue { frame, range }));
                }
                LookupStep::Descend(child) => page_id = child,
            }
        }
        Err(BTreeError::HeightExceeded)
    }

    /// Inserts a key only when it does not already exist.
    ///
    /// # Errors
    ///
    /// Returns [`BTreeError::DuplicateKey`] for an existing key and otherwise
    /// fails on storage, codec, split, or height errors.
    pub fn insert_unique(
        self,
        store: &mut PageStore,
        creating_csn: Csn,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<MutationResult, BTreeError> {
        self.mutate(store, creating_csn, key, value, MutationMode::Insert)
    }

    /// Inserts or replaces one key and returns its previous value.
    ///
    /// # Errors
    ///
    /// Returns an error for storage, codec, split, or height failures.
    pub fn upsert(
        self,
        store: &mut PageStore,
        creating_csn: Csn,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<MutationResult, BTreeError> {
        self.mutate(store, creating_csn, key, value, MutationMode::Upsert)
    }

    /// Inserts or replaces one strictly ordered, duplicate-free key batch.
    ///
    /// An empty batch is a no-op. All input entries are validated before the
    /// first page append. Existing nodes reached by multiple keys are decoded
    /// and rewritten once for this batch.
    ///
    /// # Errors
    ///
    /// Returns [`BTreeError::NoncanonicalKeyOrder`] for duplicate or unordered
    /// keys and otherwise fails on storage, codec, split, height, or entry-size
    /// errors.
    pub fn upsert_sorted_batch(
        self,
        store: &mut PageStore,
        creating_csn: Csn,
        entries: Vec<KeyValue>,
    ) -> Result<BatchMutationResult, BTreeError> {
        if entries.is_empty() {
            return Ok(BatchMutationResult {
                tree: self,
                pages_written: 0,
            });
        }
        if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(BTreeError::NoncanonicalKeyOrder);
        }
        for (key, value) in &entries {
            ensure_leaf_entry_fits(key, value)?;
        }

        let updates = entries
            .into_iter()
            .map(|(key, value)| LeafEntry { key, value })
            .collect::<Vec<_>>();
        let starting_pages = store.page_count();
        let references = if let Some(root) = self.root {
            let minimum = leftmost_key(store, root)?;
            rewrite_node_batch(store, root, creating_csn, &updates, &minimum, 0)?
        } else {
            append_leaf_level(store, creating_csn, updates)?
        };
        let tree = assemble_batch_root(store, creating_csn, references)?;
        let pages_written = usize::try_from(store.page_count() - starting_pages)
            .map_err(|_| BTreeError::LengthOverflow)?;
        Ok(BatchMutationResult {
            tree,
            pages_written,
        })
    }

    /// Replaces the exact contents of ordered key prefixes without
    /// materializing unrelated values.
    ///
    /// The caller supplies the exact keys expected below the prefixes. This
    /// fail-closed precondition is checked before the first page append.
    /// Unaffected leaf pages are reused byte-for-byte while internal levels
    /// are rebuilt from bounded leaf references.
    ///
    /// # Errors
    ///
    /// Returns [`BTreeError::PrefixContentsChanged`] if the immutable tree no
    /// longer contains exactly `expected_keys` below `prefixes`,
    /// [`BTreeError::Cancelled`] when `control` requests cancellation, and the
    /// normal codec/storage failures for reached pages.
    pub fn replace_prefixes_sorted_batch_with_control<F>(
        self,
        store: &mut PageStore,
        creating_csn: Csn,
        prefixes: &[Vec<u8>],
        expected_keys: &[Vec<u8>],
        replacements: Vec<KeyValue>,
        mut control: F,
    ) -> Result<BatchMutationResult, BTreeError>
    where
        F: FnMut() -> ControlFlow<()>,
    {
        #[cfg(test)]
        PREFIX_REPLACEMENT_APPENDED_PAGES.set(0);
        let plan = self.plan_prefixes_sorted_batch_replacement_with_control(
            store,
            prefixes,
            expected_keys,
            &replacements,
            &mut control,
        )?;
        let mut unpublished = store.begin_unpublished_tail()?;
        let result = self.replace_prefixes_sorted_batch_in_unpublished_tail_with_control(
            &mut unpublished,
            &plan,
            PrefixReplacementBatch {
                creating_csn,
                prefixes,
                expected_keys,
                replacements,
            },
            &mut control,
        );
        match result {
            Ok(result) => {
                unpublished.finalize();
                Ok(result)
            }
            Err(error) => {
                unpublished.rollback()?;
                Err(error)
            }
        }
    }

    /// Plans and conservatively bounds one exact-prefix structural rewrite.
    ///
    /// Planning validates every reachable page, separator, global leaf range,
    /// balanced depth, and ancestor cycle with a stack bounded by
    /// `MAX_TREE_HEIGHT`. It does not retain one entry per page.
    ///
    /// # Errors
    ///
    /// Returns semantic, codec, storage, or cancellation errors before any
    /// page append occurs.
    pub fn plan_prefixes_sorted_batch_replacement_with_control<F>(
        self,
        store: &PageStore,
        prefixes: &[Vec<u8>],
        expected_keys: &[Vec<u8>],
        replacements: &[KeyValue],
        control: F,
    ) -> Result<PrefixReplacementStructuralPlan, BTreeError>
    where
        F: FnMut() -> ControlFlow<()>,
    {
        validate_prefix_replacement_inputs(prefixes, expected_keys, replacements)?;
        let limits = PrefixReplacementStructuralLimits::new(
            replacements.len(),
            replacements
                .iter()
                .map(|(key, _)| key.len())
                .max()
                .unwrap_or(0),
        )?;
        self.plan_prefixes_sorted_batch_replacement_with_limits_and_control(store, limits, control)
    }

    /// Plans structural memory from scalar ceilings before the replacement
    /// batch or exact expected-key set is materialized.
    ///
    /// Caller-owned key/value payload memory is intentionally separate. The
    /// later execution rejects a batch exceeding either scalar ceiling before
    /// it collects leaf references or appends a page.
    ///
    /// # Errors
    ///
    /// Returns codec, storage, structural, or cancellation errors while
    /// streaming the captured root.
    pub fn plan_prefixes_sorted_batch_replacement_with_limits_and_control<F>(
        self,
        store: &PageStore,
        limits: PrefixReplacementStructuralLimits,
        mut control: F,
    ) -> Result<PrefixReplacementStructuralPlan, BTreeError>
    where
        F: FnMut() -> ControlFlow<()>,
    {
        let stats = inspect_tree_structure_with_control(store, self.root, &mut control)?;
        Ok(PrefixReplacementStructuralPlan {
            generation: store.generation(),
            root: self.root,
            leaf_count: stats.leaf_count,
            reachable_page_count: stats.reachable_page_count,
            maximum_stack_depth: stats.maximum_stack_depth,
            leaf_boundary_key_bytes: stats.leaf_boundary_key_bytes,
            maximum_key_length: stats.maximum_key_length,
            maximum_replacement_entries: limits.maximum_replacement_entries,
            maximum_replacement_key_length: limits.maximum_replacement_key_length,
            structural_peak_memory_bytes: prefix_replacement_structural_memory_bound(
                &stats, limits,
            ),
        })
    }

    /// Replaces ordered prefixes inside a caller-owned unpublished tail.
    ///
    /// This is the exterior-validation seam: the caller may validate the
    /// returned candidate through `unpublished` and only then call
    /// [`UnpublishedTail::finalize`]. Returning early or dropping the
    /// capability rolls every page appended since its opaque checkpoint back.
    /// Candidate pages must not enter a shared buffer pool before finalization.
    ///
    /// # Errors
    ///
    /// Returns the same semantic, cancellation, codec, and storage errors as
    /// [`Self::replace_prefixes_sorted_batch_with_control`].
    pub fn replace_prefixes_sorted_batch_in_unpublished_tail_with_control<F>(
        self,
        unpublished: &mut UnpublishedTail<'_>,
        plan: &PrefixReplacementStructuralPlan,
        batch: PrefixReplacementBatch<'_>,
        mut control: F,
    ) -> Result<BatchMutationResult, BTreeError>
    where
        F: FnMut() -> ControlFlow<()>,
    {
        let PrefixReplacementBatch {
            creating_csn,
            prefixes,
            expected_keys,
            replacements,
        } = batch;
        validate_prefix_replacement_inputs(prefixes, expected_keys, &replacements)?;
        if plan.generation != unpublished.generation() || plan.root != self.root {
            return Err(BTreeError::ForeignSegment);
        }
        let actual_maximum_key_length = replacements
            .iter()
            .map(|(key, _)| key.len())
            .max()
            .unwrap_or(0);
        if replacements.len() > plan.maximum_replacement_entries
            || actual_maximum_key_length > plan.maximum_replacement_key_length
        {
            return Err(BTreeError::ForeignSegment);
        }
        check_mutation_control(&mut control)?;

        let (leaves, observed) = collect_leaf_references_with_control(
            unpublished,
            self.root,
            plan.leaf_count,
            &mut control,
        )?;
        if observed.leaf_count != plan.leaf_count
            || observed.reachable_page_count != plan.reachable_page_count
            || observed.maximum_stack_depth != plan.maximum_stack_depth
            || observed.leaf_boundary_key_bytes != plan.leaf_boundary_key_bytes
            || observed.maximum_key_length != plan.maximum_key_length
        {
            return Err(BTreeError::ForeignSegment);
        }
        verify_prefix_contents(unpublished, &leaves, prefixes, expected_keys, &mut control)?;
        check_mutation_control(&mut control)?;
        execute_prefix_replacement(
            unpublished,
            creating_csn,
            prefixes,
            replacements,
            &leaves,
            &mut control,
        )
    }

    fn mutate(
        self,
        store: &mut PageStore,
        creating_csn: Csn,
        key: Vec<u8>,
        value: Vec<u8>,
        mode: MutationMode,
    ) -> Result<MutationResult, BTreeError> {
        ensure_leaf_entry_fits(&key, &value)?;
        let starting_pages = store.page_count();
        let (rewrite, previous) = if let Some(root) = self.root {
            rewrite_node(store, root, creating_csn, key, value, mode, 0)?
        } else {
            let leaf = Node::Leaf(vec![LeafEntry { key, value }]);
            let root = append_node(store, creating_csn, &leaf)?;
            (Rewrite::One(root), None)
        };
        let root = match rewrite {
            Rewrite::One(root) => root,
            Rewrite::Split {
                left,
                separator,
                right,
            } => append_node(
                store,
                creating_csn,
                &Node::Internal {
                    keys: vec![separator],
                    children: vec![left, right],
                },
            )?,
        };
        let pages_written = usize::try_from(store.page_count() - starting_pages)
            .map_err(|_| BTreeError::LengthOverflow)?;
        Ok(MutationResult {
            tree: Self::from_root(root),
            previous,
            pages_written,
        })
    }

    /// Recursively materializes entries in canonical key order.
    ///
    /// # Errors
    ///
    /// Returns an error for corruption, cycles, duplicate children, or
    /// excessive height.
    pub fn scan(self, store: &PageStore) -> Result<Vec<KeyValue>, BTreeError> {
        let Some(root) = self.root else {
            return Ok(Vec::new());
        };
        let mut visited = BTreeSet::new();
        let mut output = Vec::new();
        scan_node(store, root, 0, &mut visited, &mut output)?;
        if output.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(BTreeError::NoncanonicalKeyOrder);
        }
        Ok(output)
    }

    /// Materializes only entries whose keys begin with `prefix`.
    ///
    /// Internal separator ranges are used to prune subtrees that cannot
    /// contain the requested prefix. An empty prefix is equivalent to
    /// [`Self::scan`].
    ///
    /// # Errors
    ///
    /// Returns an error for corruption, cycles, duplicate children, or
    /// excessive height in every node reached by the bounded traversal.
    pub fn scan_prefix(
        self,
        store: &PageStore,
        prefix: &[u8],
    ) -> Result<Vec<KeyValue>, BTreeError> {
        let Some(root) = self.root else {
            return Ok(Vec::new());
        };
        let upper = prefix_upper_bound(prefix);
        let mut visited = BTreeSet::new();
        let mut output = Vec::new();
        scan_prefix_node(
            store,
            root,
            prefix,
            upper.as_deref(),
            0,
            &mut visited,
            &mut output,
        )?;
        if output.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(BTreeError::NoncanonicalKeyOrder);
        }
        Ok(output)
    }

    /// Materializes one prefix range through the verified buffer pool.
    ///
    /// This has the same ordering and subtree-pruning semantics as
    /// [`Self::scan_prefix`] while retaining hot immutable pages in bounded
    /// shared memory.
    ///
    /// # Errors
    ///
    /// Returns an error for page, buffer-pool, codec, cycle, or height
    /// failures in every node reached by the bounded traversal.
    pub fn scan_prefix_cached(
        self,
        store: &PageStore,
        pool: &BufferPool,
        prefix: &[u8],
    ) -> Result<Vec<KeyValue>, BTreeError> {
        let Some(root) = self.root else {
            return Ok(Vec::new());
        };
        let upper = prefix_upper_bound(prefix);
        let mut visited = BTreeSet::new();
        let mut output = Vec::new();
        scan_prefix_node_cached(
            store,
            pool,
            root,
            prefix,
            upper.as_deref(),
            0,
            &mut visited,
            &mut output,
        )?;
        if output.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(BTreeError::NoncanonicalKeyOrder);
        }
        Ok(output)
    }

    /// Visits one prefix range in canonical key order through the buffer pool.
    ///
    /// `start_after` is an exclusive full-key cursor. Returning
    /// [`ControlFlow::Break`] from `visitor` stops traversal without reading or
    /// materializing the remaining range. The returned control flow
    /// distinguishes an early stop from exhaustion.
    ///
    /// # Errors
    ///
    /// Returns an error for page, buffer-pool, codec, key-order, cycle, or
    /// height failures in every node reached before traversal stops.
    pub fn visit_prefix_cached<F>(
        self,
        store: &PageStore,
        pool: &BufferPool,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        visitor: F,
    ) -> Result<ControlFlow<()>, BTreeError>
    where
        F: FnMut(&[u8], &[u8]) -> ControlFlow<()>,
    {
        let lower = start_after.map_or(Bound::Unbounded, Bound::Excluded);
        self.visit_prefix_range_cached(store, pool, prefix, lower, Bound::Unbounded, visitor)
    }

    /// Visits one bounded prefix range in canonical key order through the
    /// buffer pool.
    ///
    /// `lower` and `upper` are full physical-key bounds. They are intersected
    /// with the prefix namespace. Returning [`ControlFlow::Break`] from
    /// `visitor` stops traversal without reading or materializing the
    /// remaining range.
    ///
    /// # Errors
    ///
    /// Returns an error for page, buffer-pool, codec, key-order, cycle, or
    /// height failures in every node reached before traversal stops.
    pub fn visit_prefix_range_cached<F>(
        self,
        store: &PageStore,
        pool: &BufferPool,
        prefix: &[u8],
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        mut visitor: F,
    ) -> Result<ControlFlow<()>, BTreeError>
    where
        F: FnMut(&[u8], &[u8]) -> ControlFlow<()>,
    {
        let Some(root) = self.root else {
            return Ok(ControlFlow::Continue(()));
        };
        let prefix_upper = prefix_upper_bound(prefix);
        let mut visited = BTreeSet::new();
        let mut last_key = None;
        visit_prefix_range_node_cached(
            store,
            pool,
            root,
            prefix,
            prefix_upper.as_deref(),
            lower,
            upper,
            0,
            &mut visited,
            &mut last_key,
            &mut visitor,
        )
    }

    /// Visits one bounded prefix range in reverse canonical key order through
    /// the buffer pool.
    ///
    /// `lower` and `upper` are full physical-key bounds. They are intersected
    /// with the prefix namespace. Returning [`ControlFlow::Break`] from
    /// `visitor` stops traversal without reading or materializing the
    /// remaining range.
    ///
    /// # Errors
    ///
    /// Returns an error for page, buffer-pool, codec, key-order, cycle, or
    /// height failures in every node reached before traversal stops.
    pub fn visit_prefix_range_cached_reverse<F>(
        self,
        store: &PageStore,
        pool: &BufferPool,
        prefix: &[u8],
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        mut visitor: F,
    ) -> Result<ControlFlow<()>, BTreeError>
    where
        F: FnMut(&[u8], &[u8]) -> ControlFlow<()>,
    {
        let Some(root) = self.root else {
            return Ok(ControlFlow::Continue(()));
        };
        let prefix_upper = prefix_upper_bound(prefix);
        let mut visited = BTreeSet::new();
        let mut last_key = None;
        visit_prefix_range_node_cached_reverse(
            CachedTreeRead { store, pool },
            root,
            CachedPrefixRange {
                prefix,
                prefix_upper: prefix_upper.as_deref(),
                lower,
                upper,
            },
            0,
            &mut visited,
            &mut last_key,
            &mut visitor,
        )
    }

    /// Verifies the complete reachable tree and returns its entry count.
    ///
    /// # Errors
    ///
    /// Returns an error for any page/node corruption, separator divergence,
    /// cycle, duplicate child, or excessive height.
    pub fn validate(self, store: &PageStore) -> Result<usize, BTreeError> {
        self.validate_at(store, None)
    }

    /// Verifies the complete tree and rejects nodes newer than `visible_csn`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::validate`] and
    /// [`BTreeError::FuturePage`] for a missing or future creating CSN.
    pub fn validate_visible(
        self,
        store: &PageStore,
        visible_csn: Csn,
    ) -> Result<usize, BTreeError> {
        self.validate_at(store, Some(visible_csn))
    }

    fn validate_at(self, store: &PageStore, visible_csn: Option<Csn>) -> Result<usize, BTreeError> {
        let Some(root) = self.root else {
            return Ok(0);
        };
        let mut visited = BTreeSet::new();
        let summary = validate_node(store, root, visible_csn, 0, &mut visited)?;
        Ok(summary.count)
    }
}

#[derive(Clone, Copy)]
enum MutationMode {
    Insert,
    Upsert,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LeafEntry {
    key: Vec<u8>,
    value: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Node {
    Leaf(Vec<LeafEntry>),
    Internal {
        keys: Vec<Vec<u8>>,
        children: Vec<PageId>,
    },
}

impl Node {
    fn page_kind(&self) -> PageKind {
        match self {
            Self::Leaf(_) => PageKind::BTreeLeaf,
            Self::Internal { .. } => PageKind::BTreeInternal,
        }
    }

    fn encode(&self) -> Result<Vec<u8>, BTreeError> {
        match self {
            Self::Leaf(entries) => encode_leaf(entries),
            Self::Internal { keys, children } => encode_internal(keys, children),
        }
    }
}

enum Rewrite {
    One(PageId),
    Split {
        left: PageId,
        separator: Vec<u8>,
        right: PageId,
    },
}

#[derive(Debug)]
struct ChildReference {
    minimum: Vec<u8>,
    page: PageId,
}

#[derive(Debug)]
struct ExistingLeafReference {
    minimum: Vec<u8>,
    maximum: Vec<u8>,
    page: PageId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TreeStructuralStats {
    leaf_count: usize,
    reachable_page_count: usize,
    maximum_stack_depth: usize,
    leaf_boundary_key_bytes: usize,
    maximum_key_length: usize,
}

enum LookupStep {
    Descend(PageId),
    Value(Option<Range<usize>>),
}

fn lookup_page(page: &Page, target: &[u8]) -> Result<LookupStep, BTreeError> {
    match page.kind() {
        PageKind::BTreeLeaf => lookup_leaf(page.payload(), target),
        PageKind::BTreeInternal => lookup_internal(page.payload(), target),
        _ => Err(BTreeError::WrongPageKind),
    }
}

fn lookup_leaf(payload: &[u8], target: &[u8]) -> Result<LookupStep, BTreeError> {
    if payload.len() < LEAF_HEADER_SIZE {
        return Err(BTreeError::InvalidLength);
    }
    if &payload[0..8] != LEAF_MAGIC
        || read_u16(&payload[8..10]) != FORMAT_VERSION
        || payload[12..16].iter().any(|byte| *byte != 0)
    {
        return Err(BTreeError::InvalidPreamble);
    }
    let count = usize::from(read_u16(&payload[10..12]));
    if count == 0 || count > (payload.len() - LEAF_HEADER_SIZE) / 8 {
        return Err(BTreeError::InvalidCount);
    }
    let mut cursor = Cursor::new(&payload[LEAF_HEADER_SIZE..]);
    let mut previous_key = None;
    let mut found = None;
    for _ in 0..count {
        let key_length = cursor.length()?;
        let value_length = cursor.length()?;
        if key_length > BTREE_MAX_KEY_SIZE {
            return Err(BTreeError::KeyTooLarge);
        }
        let key = cursor.take(key_length)?;
        if previous_key.is_some_and(|previous| previous >= key) {
            return Err(BTreeError::NoncanonicalKeyOrder);
        }
        let value_start = cursor.position();
        cursor.take(value_length)?;
        if key == target {
            found =
                Some(LEAF_HEADER_SIZE + value_start..LEAF_HEADER_SIZE + value_start + value_length);
        }
        previous_key = Some(key);
    }
    cursor.finish()?;
    Ok(LookupStep::Value(found))
}

fn lookup_internal(payload: &[u8], target: &[u8]) -> Result<LookupStep, BTreeError> {
    if payload.len() < INTERNAL_HEADER_SIZE {
        return Err(BTreeError::InvalidLength);
    }
    if &payload[0..8] != INTERNAL_MAGIC
        || read_u16(&payload[8..10]) != FORMAT_VERSION
        || payload[12..16].iter().any(|byte| *byte != 0)
    {
        return Err(BTreeError::InvalidPreamble);
    }
    let count = usize::from(read_u16(&payload[10..12]));
    if count == 0 || count > (payload.len() - INTERNAL_HEADER_SIZE) / 12 + 1 {
        return Err(BTreeError::InvalidCount);
    }
    let first_child = PageId::new(read_u64(&payload[16..24])).map_err(|_| BTreeError::ZeroChild)?;
    let mut selected = first_child;
    let mut previous_key = None;
    let mut cursor = Cursor::new(&payload[INTERNAL_HEADER_SIZE..]);
    for _ in 0..count {
        let key_length = cursor.length()?;
        if key_length > BTREE_MAX_KEY_SIZE {
            return Err(BTreeError::KeyTooLarge);
        }
        let key = cursor.take(key_length)?;
        if previous_key.is_some_and(|previous| previous >= key) {
            return Err(BTreeError::NoncanonicalKeyOrder);
        }
        let child = PageId::new(cursor.u64()?).map_err(|_| BTreeError::ZeroChild)?;
        if target >= key {
            selected = child;
        }
        previous_key = Some(key);
    }
    cursor.finish()?;
    Ok(LookupStep::Descend(selected))
}

fn rewrite_node(
    store: &mut PageStore,
    page_id: PageId,
    creating_csn: Csn,
    key: Vec<u8>,
    value: Vec<u8>,
    mode: MutationMode,
    depth: usize,
) -> Result<(Rewrite, Option<Vec<u8>>), BTreeError> {
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    match read_node(store, page_id)? {
        Node::Leaf(mut entries) => {
            let previous = match entries.binary_search_by(|entry| entry.key.cmp(&key)) {
                Ok(index) => match mode {
                    MutationMode::Insert => return Err(BTreeError::DuplicateKey),
                    MutationMode::Upsert => {
                        Some(std::mem::replace(&mut entries[index].value, value))
                    }
                },
                Err(index) => {
                    entries.insert(index, LeafEntry { key, value });
                    None
                }
            };
            if leaf_encoded_length(&entries)? <= PAGE_PAYLOAD_SIZE {
                let root = append_node(store, creating_csn, &Node::Leaf(entries))?;
                Ok((Rewrite::One(root), previous))
            } else {
                let split = choose_leaf_split(&entries)?;
                let right_entries = entries.split_off(split);
                let separator = right_entries
                    .first()
                    .ok_or(BTreeError::NoValidSplit)?
                    .key
                    .clone();
                let left = append_node(store, creating_csn, &Node::Leaf(entries))?;
                let right = append_node(store, creating_csn, &Node::Leaf(right_entries))?;
                Ok((
                    Rewrite::Split {
                        left,
                        separator,
                        right,
                    },
                    previous,
                ))
            }
        }
        Node::Internal {
            mut keys,
            mut children,
        } => {
            let index = child_index(&keys, &key);
            let (child_rewrite, previous) = rewrite_node(
                store,
                children[index],
                creating_csn,
                key,
                value,
                mode,
                depth + 1,
            )?;
            match child_rewrite {
                Rewrite::One(child) => children[index] = child,
                Rewrite::Split {
                    left,
                    separator,
                    right,
                } => {
                    children[index] = left;
                    children.insert(index + 1, right);
                    keys.insert(index, separator);
                }
            }
            if internal_encoded_length(&keys)? <= PAGE_PAYLOAD_SIZE {
                let root = append_node(store, creating_csn, &Node::Internal { keys, children })?;
                Ok((Rewrite::One(root), previous))
            } else {
                let promote = choose_internal_split(&keys)?;
                let right_keys = keys.split_off(promote + 1);
                let separator = keys.pop().ok_or(BTreeError::NoValidSplit)?;
                let right_children = children.split_off(promote + 1);
                let left = append_node(store, creating_csn, &Node::Internal { keys, children })?;
                let right = append_node(
                    store,
                    creating_csn,
                    &Node::Internal {
                        keys: right_keys,
                        children: right_children,
                    },
                )?;
                Ok((
                    Rewrite::Split {
                        left,
                        separator,
                        right,
                    },
                    previous,
                ))
            }
        }
    }
}

fn leftmost_key(store: &PageStore, mut page_id: PageId) -> Result<Vec<u8>, BTreeError> {
    let mut visited = [0_u64; MAX_TREE_HEIGHT];
    for depth in 0..MAX_TREE_HEIGHT {
        if visited[..depth].contains(&page_id.get()) {
            return Err(BTreeError::Cycle);
        }
        visited[depth] = page_id.get();
        match read_node(store, page_id)? {
            Node::Leaf(entries) => {
                return entries
                    .first()
                    .map(|entry| entry.key.clone())
                    .ok_or(BTreeError::InvalidCount);
            }
            Node::Internal { children, .. } => {
                page_id = *children.first().ok_or(BTreeError::InvalidCount)?;
            }
        }
    }
    Err(BTreeError::HeightExceeded)
}

fn rewrite_node_batch(
    store: &mut PageStore,
    page_id: PageId,
    creating_csn: Csn,
    updates: &[LeafEntry],
    known_minimum: &[u8],
    depth: usize,
) -> Result<Vec<ChildReference>, BTreeError> {
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    match read_node(store, page_id)? {
        Node::Leaf(entries) => {
            let merged = merge_leaf_updates(entries, updates);
            append_leaf_level(store, creating_csn, merged)
        }
        Node::Internal { keys, children } => {
            let mut rewritten_children = Vec::with_capacity(children.len());
            let mut update_start = 0;
            for (index, child) in children.into_iter().enumerate() {
                let update_end = keys.get(index).map_or(updates.len(), |upper| {
                    update_start
                        + updates[update_start..]
                            .partition_point(|entry| entry.key.as_slice() < upper.as_slice())
                });
                let child_minimum = index
                    .checked_sub(1)
                    .and_then(|prior| keys.get(prior))
                    .map_or(known_minimum, Vec::as_slice);
                if update_start == update_end {
                    rewritten_children.push(ChildReference {
                        minimum: child_minimum.to_vec(),
                        page: child,
                    });
                } else {
                    rewritten_children.extend(rewrite_node_batch(
                        store,
                        child,
                        creating_csn,
                        &updates[update_start..update_end],
                        child_minimum,
                        depth + 1,
                    )?);
                }
                update_start = update_end;
            }
            if update_start != updates.len() {
                return Err(BTreeError::NoncanonicalKeyOrder);
            }
            append_internal_level(store, creating_csn, rewritten_children)
        }
    }
}

fn merge_leaf_updates(entries: Vec<LeafEntry>, updates: &[LeafEntry]) -> Vec<LeafEntry> {
    let mut existing = entries.into_iter().peekable();
    let mut merged = Vec::with_capacity(existing.len().saturating_add(updates.len()));
    for update in updates {
        while existing.peek().is_some_and(|entry| entry.key < update.key) {
            if let Some(entry) = existing.next() {
                merged.push(entry);
            }
        }
        if existing.peek().is_some_and(|entry| entry.key == update.key) {
            existing.next();
        }
        merged.push(update.clone());
    }
    merged.extend(existing);
    merged
}

fn validate_prefix_replacement_inputs(
    prefixes: &[Vec<u8>],
    expected_keys: &[Vec<u8>],
    replacements: &[KeyValue],
) -> Result<(), BTreeError> {
    if prefixes.is_empty()
        || prefixes.iter().any(Vec::is_empty)
        || prefixes
            .windows(2)
            .any(|pair| pair[0] >= pair[1] || pair[1].starts_with(pair[0].as_slice()))
        || expected_keys.windows(2).any(|pair| pair[0] >= pair[1])
        || replacements.windows(2).any(|pair| pair[0].0 >= pair[1].0)
    {
        return Err(BTreeError::NoncanonicalKeyOrder);
    }
    if expected_keys
        .iter()
        .any(|key| !key_matches_prefixes(key, prefixes))
        || replacements
            .iter()
            .any(|(key, _)| !key_matches_prefixes(key, prefixes))
    {
        return Err(BTreeError::PrefixContentsChanged);
    }
    for (key, value) in replacements {
        ensure_leaf_entry_fits(key, value)?;
    }
    Ok(())
}

fn verify_prefix_contents<S, F>(
    store: &S,
    leaves: &[ExistingLeafReference],
    prefixes: &[Vec<u8>],
    expected_keys: &[Vec<u8>],
    control: &mut F,
) -> Result<(), BTreeError>
where
    S: BTreePageRead,
    F: FnMut() -> ControlFlow<()>,
{
    let mut expected = expected_keys.iter();
    for leaf in leaves
        .iter()
        .filter(|leaf| leaf_intersects_prefixes(leaf, prefixes))
    {
        check_mutation_control(control)?;
        let Node::Leaf(entries) = read_node(store, leaf.page)? else {
            return Err(BTreeError::WrongPageKind);
        };
        for entry in entries
            .into_iter()
            .filter(|entry| key_matches_prefixes(&entry.key, prefixes))
        {
            if expected.next().is_none_or(|key| key != &entry.key) {
                return Err(BTreeError::PrefixContentsChanged);
            }
        }
    }
    if expected.next().is_none() {
        Ok(())
    } else {
        Err(BTreeError::PrefixContentsChanged)
    }
}

fn key_matches_prefixes(key: &[u8], prefixes: &[Vec<u8>]) -> bool {
    prefixes
        .binary_search_by(|prefix| prefix.as_slice().cmp(key))
        .is_ok_and(|index| key.starts_with(&prefixes[index]))
        || prefixes
            .partition_point(|prefix| prefix.as_slice() <= key)
            .checked_sub(1)
            .is_some_and(|index| key.starts_with(&prefixes[index]))
}

fn leaf_intersects_prefixes(leaf: &ExistingLeafReference, prefixes: &[Vec<u8>]) -> bool {
    prefixes.iter().any(|prefix| {
        leaf.maximum.as_slice() >= prefix.as_slice()
            && prefix_upper_bound(prefix)
                .is_none_or(|upper| leaf.minimum.as_slice() < upper.as_slice())
    })
}

fn check_mutation_control<F>(control: &mut F) -> Result<(), BTreeError>
where
    F: FnMut() -> ControlFlow<()>,
{
    match control() {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(()) => Err(BTreeError::Cancelled),
    }
}

fn prefix_replacement_structural_memory_bound(
    stats: &TreeStructuralStats,
    limits: PrefixReplacementStructuralLimits,
) -> usize {
    prefix_replacement_structural_memory_bound_parts(
        stats.leaf_count,
        stats.leaf_boundary_key_bytes,
        stats.maximum_key_length,
        limits.maximum_replacement_entries,
        limits.maximum_replacement_key_length,
    )
}

fn prefix_replacement_structural_memory_bound_parts(
    leaf_count: usize,
    leaf_boundary_key_bytes: usize,
    maximum_tree_key_length: usize,
    maximum_replacement_entries: usize,
    maximum_replacement_key_length: usize,
) -> usize {
    let maximum_key_length = maximum_tree_key_length.max(maximum_replacement_key_length);
    let leaf_snapshot = allocation_bound(
        leaf_count,
        size_of::<ExistingLeafReference>(),
        leaf_boundary_key_bytes,
        leaf_count.saturating_mul(2),
    );
    let exact_key_validation = 0;
    let update_headers = allocation_bound(
        maximum_replacement_entries,
        size_of::<LeafEntry>().saturating_mul(4),
        0,
        2,
    );
    let maximum_leaf_references = leaf_count.saturating_add(maximum_replacement_entries);
    let reference_key_bytes = maximum_leaf_references.saturating_mul(maximum_key_length);
    let one_reference_level = allocation_bound(
        maximum_leaf_references,
        size_of::<ChildReference>(),
        reference_key_bytes,
        maximum_leaf_references,
    );
    // Parent construction can retain the current level's allocation while a
    // parent vector and one encoded internal group are live.
    let internal_assembly = one_reference_level
        .saturating_mul(2)
        .saturating_add(DECODED_NODE_MEMORY_BOUND);
    let leaf_rewrite = update_headers
        .saturating_add(DECODED_NODE_MEMORY_BOUND.saturating_mul(2))
        .saturating_add(internal_assembly);
    let traversal_stack = MAX_TREE_HEIGHT
        .saturating_mul(size_of::<PageId>())
        .saturating_add(ALLOCATOR_ALLOCATION_OVERHEAD_BYTES);
    leaf_snapshot
        .saturating_add(exact_key_validation.max(leaf_rewrite))
        .saturating_add(DECODED_NODE_MEMORY_BOUND)
        .saturating_add(traversal_stack)
}

fn allocation_bound(
    elements: usize,
    element_size: usize,
    owned_payload_bytes: usize,
    owned_allocations: usize,
) -> usize {
    elements
        .saturating_mul(element_size)
        .saturating_add(owned_payload_bytes)
        .saturating_add(owned_allocations.saturating_mul(ALLOCATOR_ALLOCATION_OVERHEAD_BYTES))
        .saturating_add(ALLOCATOR_ALLOCATION_OVERHEAD_BYTES)
}

fn execute_prefix_replacement<F>(
    unpublished: &mut UnpublishedTail<'_>,
    creating_csn: Csn,
    prefixes: &[Vec<u8>],
    replacements: Vec<KeyValue>,
    leaves: &[ExistingLeafReference],
    control: &mut F,
) -> Result<BatchMutationResult, BTreeError>
where
    F: FnMut() -> ControlFlow<()>,
{
    let starting_pages = unpublished.page_count();
    let updates = replacements
        .into_iter()
        .map(|(key, value)| LeafEntry { key, value })
        .collect::<Vec<_>>();
    let references = if leaves.is_empty() {
        if updates.is_empty() {
            Vec::new()
        } else {
            let references = append_leaf_level(unpublished, creating_csn, updates)?;
            observe_prefix_replacement_appends(unpublished, starting_pages);
            references
        }
    } else {
        rewrite_prefix_leaves(
            unpublished,
            creating_csn,
            prefixes,
            updates,
            leaves,
            starting_pages,
            control,
        )?
    };
    check_mutation_control(control)?;
    let tree = if references.is_empty() {
        BTree::empty()
    } else {
        assemble_batch_root(unpublished, creating_csn, references)?
    };
    let pages_written = usize::try_from(unpublished.page_count() - starting_pages)
        .map_err(|_| BTreeError::LengthOverflow)?;
    Ok(BatchMutationResult {
        tree,
        pages_written,
    })
}

#[allow(clippy::too_many_arguments)]
fn rewrite_prefix_leaves<F>(
    unpublished: &mut UnpublishedTail<'_>,
    creating_csn: Csn,
    prefixes: &[Vec<u8>],
    updates: Vec<LeafEntry>,
    leaves: &[ExistingLeafReference],
    starting_pages: u64,
    control: &mut F,
) -> Result<Vec<ChildReference>, BTreeError>
where
    F: FnMut() -> ControlFlow<()>,
{
    let update_count = updates.len();
    let mut updates = updates.into_iter().peekable();
    let mut references = Vec::with_capacity(leaves.len().saturating_add(update_count));
    for (index, leaf) in leaves.iter().enumerate() {
        check_mutation_control(control)?;
        let next_minimum = leaves.get(index + 1).map(|next| next.minimum.as_slice());
        let mut leaf_updates = Vec::new();
        while updates
            .peek()
            .is_some_and(|entry| next_minimum.is_none_or(|next| entry.key.as_slice() < next))
        {
            if let Some(update) = updates.next() {
                leaf_updates.push(update);
            }
        }
        let touches_prefix = leaf_intersects_prefixes(leaf, prefixes);
        if !touches_prefix && leaf_updates.is_empty() {
            references.push(ChildReference {
                minimum: leaf.minimum.clone(),
                page: leaf.page,
            });
        } else {
            let Node::Leaf(entries) = read_node(unpublished, leaf.page)? else {
                return Err(BTreeError::WrongPageKind);
            };
            let retained = entries
                .into_iter()
                .filter(|entry| !key_matches_prefixes(&entry.key, prefixes))
                .collect::<Vec<_>>();
            let merged = merge_leaf_updates_owned(retained, leaf_updates);
            if !merged.is_empty() {
                references.extend(append_leaf_level(unpublished, creating_csn, merged)?);
                observe_prefix_replacement_appends(unpublished, starting_pages);
            }
        }
    }
    if updates.next().is_some() {
        return Err(BTreeError::NoncanonicalKeyOrder);
    }
    Ok(references)
}

fn merge_leaf_updates_owned(entries: Vec<LeafEntry>, updates: Vec<LeafEntry>) -> Vec<LeafEntry> {
    let mut existing = entries.into_iter().peekable();
    let mut merged = Vec::with_capacity(existing.len().saturating_add(updates.len()));
    for update in updates {
        while existing.peek().is_some_and(|entry| entry.key < update.key) {
            if let Some(entry) = existing.next() {
                merged.push(entry);
            }
        }
        if existing.peek().is_some_and(|entry| entry.key == update.key) {
            existing.next();
        }
        merged.push(update);
    }
    merged.extend(existing);
    merged
}

fn observe_prefix_replacement_appends(unpublished: &UnpublishedTail<'_>, starting_pages: u64) {
    #[cfg(test)]
    PREFIX_REPLACEMENT_APPENDED_PAGES.set(unpublished.page_count().saturating_sub(starting_pages));
    #[cfg(not(test))]
    let _ = (unpublished, starting_pages);
}

fn inspect_tree_structure_with_control<S, F>(
    store: &S,
    root: Option<PageId>,
    control: &mut F,
) -> Result<TreeStructuralStats, BTreeError>
where
    S: BTreePageRead,
    F: FnMut() -> ControlFlow<()>,
{
    let Some(root) = root else {
        return Ok(TreeStructuralStats::default());
    };
    let mut stats = TreeStructuralStats::default();
    let mut ancestry = Vec::with_capacity(MAX_TREE_HEIGHT);
    let mut leaf_depth = None;
    let mut previous_maximum = None;
    walk_leaf_references_node(
        store,
        root,
        None,
        0,
        &mut ancestry,
        &mut leaf_depth,
        &mut previous_maximum,
        &mut stats,
        control,
        &mut |_, _, _| {},
    )?;
    Ok(stats)
}

fn collect_leaf_references_with_control<S, F>(
    store: &S,
    root: Option<PageId>,
    leaf_capacity: usize,
    control: &mut F,
) -> Result<(Vec<ExistingLeafReference>, TreeStructuralStats), BTreeError>
where
    S: BTreePageRead,
    F: FnMut() -> ControlFlow<()>,
{
    let Some(root) = root else {
        return Ok((Vec::new(), TreeStructuralStats::default()));
    };
    let mut output = Vec::with_capacity(leaf_capacity);
    let mut stats = TreeStructuralStats::default();
    let mut ancestry = Vec::with_capacity(MAX_TREE_HEIGHT);
    let mut leaf_depth = None;
    let mut previous_maximum = None;
    walk_leaf_references_node(
        store,
        root,
        None,
        0,
        &mut ancestry,
        &mut leaf_depth,
        &mut previous_maximum,
        &mut stats,
        control,
        &mut |page, minimum, maximum| {
            output.push(ExistingLeafReference {
                minimum: minimum.to_vec(),
                maximum: maximum.to_vec(),
                page,
            });
        },
    )?;
    Ok((output, stats))
}

#[allow(clippy::too_many_arguments)]
fn walk_leaf_references_node<S, F, V>(
    store: &S,
    page_id: PageId,
    expected_minimum: Option<&[u8]>,
    depth: usize,
    ancestry: &mut Vec<PageId>,
    leaf_depth: &mut Option<usize>,
    previous_maximum: &mut Option<Vec<u8>>,
    stats: &mut TreeStructuralStats,
    control: &mut F,
    visitor: &mut V,
) -> Result<Vec<u8>, BTreeError>
where
    S: BTreePageRead,
    F: FnMut() -> ControlFlow<()>,
    V: FnMut(PageId, &[u8], &[u8]),
{
    check_mutation_control(control)?;
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    if ancestry.contains(&page_id) {
        return Err(BTreeError::Cycle);
    }
    ancestry.push(page_id);
    stats.reachable_page_count = stats.reachable_page_count.saturating_add(1);
    stats.maximum_stack_depth = stats.maximum_stack_depth.max(ancestry.len());
    let result = match read_node(store, page_id)? {
        Node::Leaf(entries) => {
            if leaf_depth.is_some_and(|known| known != depth) {
                return Err(BTreeError::Unbalanced);
            }
            *leaf_depth = Some(depth);
            let minimum = entries.first().ok_or(BTreeError::InvalidCount)?.key.clone();
            let maximum = entries.last().ok_or(BTreeError::InvalidCount)?.key.clone();
            if expected_minimum.is_some_and(|expected| minimum.as_slice() != expected) {
                return Err(BTreeError::InvalidSeparator);
            }
            if previous_maximum
                .as_ref()
                .is_some_and(|previous| previous.as_slice() >= minimum.as_slice())
            {
                return Err(BTreeError::NoncanonicalKeyOrder);
            }
            stats.leaf_count = stats.leaf_count.saturating_add(1);
            stats.leaf_boundary_key_bytes = stats
                .leaf_boundary_key_bytes
                .saturating_add(minimum.len())
                .saturating_add(maximum.len());
            stats.maximum_key_length = stats
                .maximum_key_length
                .max(minimum.len())
                .max(maximum.len());
            visitor(page_id, &minimum, &maximum);
            *previous_maximum = Some(maximum);
            Ok(minimum)
        }
        Node::Internal { keys, children } => {
            let mut minimum = None;
            for (index, child) in children.into_iter().enumerate() {
                let child_expected = if index == 0 {
                    expected_minimum
                } else {
                    keys.get(index - 1).map(Vec::as_slice)
                };
                let child_minimum = walk_leaf_references_node(
                    store,
                    child,
                    child_expected,
                    depth + 1,
                    ancestry,
                    leaf_depth,
                    previous_maximum,
                    stats,
                    control,
                    visitor,
                )?;
                if minimum.is_none() {
                    minimum = Some(child_minimum);
                }
            }
            let minimum = minimum.ok_or(BTreeError::InvalidCount)?;
            if expected_minimum.is_some_and(|expected| minimum.as_slice() != expected) {
                return Err(BTreeError::InvalidSeparator);
            }
            Ok(minimum)
        }
    };
    ancestry.pop();
    result
}

fn append_leaf_level<S: BTreePageWrite>(
    store: &mut S,
    creating_csn: Csn,
    entries: Vec<LeafEntry>,
) -> Result<Vec<ChildReference>, BTreeError> {
    if entries.is_empty() {
        return Err(BTreeError::InvalidCount);
    }
    let mut references = Vec::new();
    let mut page_entries = Vec::new();
    let mut encoded_length = LEAF_HEADER_SIZE;
    for entry in entries {
        let entry_length = leaf_entry_encoded_length(&entry)?;
        if !page_entries.is_empty()
            && encoded_length
                .checked_add(entry_length)
                .ok_or(BTreeError::LengthOverflow)?
                > PAGE_PAYLOAD_SIZE
        {
            references.push(append_leaf_reference(
                store,
                creating_csn,
                std::mem::take(&mut page_entries),
            )?);
            encoded_length = LEAF_HEADER_SIZE;
        }
        encoded_length = encoded_length
            .checked_add(entry_length)
            .ok_or(BTreeError::LengthOverflow)?;
        page_entries.push(entry);
    }
    if !page_entries.is_empty() {
        references.push(append_leaf_reference(store, creating_csn, page_entries)?);
    }
    Ok(references)
}

fn append_leaf_reference<S: BTreePageWrite>(
    store: &mut S,
    creating_csn: Csn,
    entries: Vec<LeafEntry>,
) -> Result<ChildReference, BTreeError> {
    let minimum = entries
        .first()
        .map(|entry| entry.key.clone())
        .ok_or(BTreeError::InvalidCount)?;
    let page = append_node(store, creating_csn, &Node::Leaf(entries))?;
    Ok(ChildReference { minimum, page })
}

fn append_internal_level<S: BTreePageWrite>(
    store: &mut S,
    creating_csn: Csn,
    references: Vec<ChildReference>,
) -> Result<Vec<ChildReference>, BTreeError> {
    let group_sizes = internal_group_sizes(&references)?;
    let mut references = references.into_iter();
    let mut parents = Vec::with_capacity(group_sizes.len());
    for group_size in group_sizes {
        let group = references.by_ref().take(group_size).collect::<Vec<_>>();
        if group.len() != group_size {
            return Err(BTreeError::InvalidCount);
        }
        let minimum = group
            .first()
            .map(|reference| reference.minimum.clone())
            .ok_or(BTreeError::InvalidCount)?;
        let keys = group
            .iter()
            .skip(1)
            .map(|reference| reference.minimum.clone())
            .collect();
        let children = group.iter().map(|reference| reference.page).collect();
        let page = append_node(store, creating_csn, &Node::Internal { keys, children })?;
        parents.push(ChildReference { minimum, page });
    }
    if references.next().is_some() {
        return Err(BTreeError::InvalidCount);
    }
    Ok(parents)
}

fn internal_group_sizes(references: &[ChildReference]) -> Result<Vec<usize>, BTreeError> {
    if references.len() < 2 {
        return Err(BTreeError::InvalidCount);
    }
    let mut groups = Vec::new();
    let mut start = 0;
    while start < references.len() {
        let remaining = references.len() - start;
        if remaining < 2 {
            return Err(BTreeError::NoValidSplit);
        }
        let mut group_size = 2;
        let mut encoded_length = INTERNAL_HEADER_SIZE
            .checked_add(internal_separator_encoded_length(
                &references[start + 1].minimum,
            )?)
            .ok_or(BTreeError::LengthOverflow)?;
        while group_size < remaining {
            let next_length =
                internal_separator_encoded_length(&references[start + group_size].minimum)?;
            if encoded_length
                .checked_add(next_length)
                .ok_or(BTreeError::LengthOverflow)?
                > PAGE_PAYLOAD_SIZE
            {
                break;
            }
            encoded_length += next_length;
            group_size += 1;
        }
        if remaining - group_size == 1 {
            if group_size == 2 {
                return Err(BTreeError::NoValidSplit);
            }
            group_size -= 1;
        }
        groups.push(group_size);
        start += group_size;
    }
    Ok(groups)
}

fn assemble_batch_root<S: BTreePageWrite>(
    store: &mut S,
    creating_csn: Csn,
    mut references: Vec<ChildReference>,
) -> Result<BTree, BTreeError> {
    for _ in 0..MAX_TREE_HEIGHT {
        match references.len() {
            0 => return Err(BTreeError::InvalidCount),
            1 => return Ok(BTree::from_root(references[0].page)),
            _ => {
                references = append_internal_level(store, creating_csn, references)?;
            }
        }
    }
    Err(BTreeError::HeightExceeded)
}

fn append_node<S: BTreePageWrite>(
    store: &mut S,
    creating_csn: Csn,
    node: &Node,
) -> Result<PageId, BTreeError> {
    Ok(store.append_btree_page(node.page_kind(), Some(creating_csn), node.encode()?)?)
}

fn read_node<S: BTreePageRead>(store: &S, page_id: PageId) -> Result<Node, BTreeError> {
    let page = store.read_btree_page(page_id)?;
    decode_page(&page)
}

fn decode_page(page: &Page) -> Result<Node, BTreeError> {
    match page.kind() {
        PageKind::BTreeLeaf => decode_leaf(page.payload()),
        PageKind::BTreeInternal => decode_internal(page.payload()),
        _ => Err(BTreeError::WrongPageKind),
    }
}

fn encode_leaf(entries: &[LeafEntry]) -> Result<Vec<u8>, BTreeError> {
    validate_leaf_entries(entries)?;
    let total = leaf_encoded_length(entries)?;
    if total > PAGE_PAYLOAD_SIZE {
        return Err(BTreeError::NoValidSplit);
    }
    let count = u16::try_from(entries.len()).map_err(|_| BTreeError::InvalidCount)?;
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(LEAF_MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    for entry in entries {
        put_length(&mut bytes, entry.key.len())?;
        put_length(&mut bytes, entry.value.len())?;
        bytes.extend_from_slice(&entry.key);
        bytes.extend_from_slice(&entry.value);
    }
    Ok(bytes)
}

fn decode_leaf(payload: &[u8]) -> Result<Node, BTreeError> {
    if payload.len() < LEAF_HEADER_SIZE {
        return Err(BTreeError::InvalidLength);
    }
    if &payload[0..8] != LEAF_MAGIC
        || read_u16(&payload[8..10]) != FORMAT_VERSION
        || payload[12..16].iter().any(|byte| *byte != 0)
    {
        return Err(BTreeError::InvalidPreamble);
    }
    let count = usize::from(read_u16(&payload[10..12]));
    if count == 0 || count > (payload.len() - LEAF_HEADER_SIZE) / 8 {
        return Err(BTreeError::InvalidCount);
    }
    let mut cursor = Cursor::new(&payload[LEAF_HEADER_SIZE..]);
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key_length = cursor.length()?;
        let value_length = cursor.length()?;
        entries.push(LeafEntry {
            key: cursor.take(key_length)?.to_vec(),
            value: cursor.take(value_length)?.to_vec(),
        });
    }
    cursor.finish()?;
    validate_leaf_entries(&entries)?;
    Ok(Node::Leaf(entries))
}

fn encode_internal(keys: &[Vec<u8>], children: &[PageId]) -> Result<Vec<u8>, BTreeError> {
    validate_internal_shape(keys, children)?;
    let total = internal_encoded_length(keys)?;
    if total > PAGE_PAYLOAD_SIZE {
        return Err(BTreeError::NoValidSplit);
    }
    let count = u16::try_from(keys.len()).map_err(|_| BTreeError::InvalidCount)?;
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(INTERNAL_MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&children[0].get().to_le_bytes());
    for (key, child) in keys.iter().zip(children.iter().skip(1)) {
        put_length(&mut bytes, key.len())?;
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&child.get().to_le_bytes());
    }
    Ok(bytes)
}

fn decode_internal(payload: &[u8]) -> Result<Node, BTreeError> {
    if payload.len() < INTERNAL_HEADER_SIZE {
        return Err(BTreeError::InvalidLength);
    }
    if &payload[0..8] != INTERNAL_MAGIC
        || read_u16(&payload[8..10]) != FORMAT_VERSION
        || payload[12..16].iter().any(|byte| *byte != 0)
    {
        return Err(BTreeError::InvalidPreamble);
    }
    let count = usize::from(read_u16(&payload[10..12]));
    if count == 0 || count > (payload.len() - INTERNAL_HEADER_SIZE) / 12 + 1 {
        return Err(BTreeError::InvalidCount);
    }
    let first_child = PageId::new(read_u64(&payload[16..24])).map_err(|_| BTreeError::ZeroChild)?;
    let mut cursor = Cursor::new(&payload[INTERNAL_HEADER_SIZE..]);
    let mut keys = Vec::with_capacity(count);
    let mut children = Vec::with_capacity(count + 1);
    children.push(first_child);
    for _ in 0..count {
        let key_length = cursor.length()?;
        keys.push(cursor.take(key_length)?.to_vec());
        children.push(PageId::new(cursor.u64()?).map_err(|_| BTreeError::ZeroChild)?);
    }
    cursor.finish()?;
    validate_internal_shape(&keys, &children)?;
    Ok(Node::Internal { keys, children })
}

fn validate_leaf_entries(entries: &[LeafEntry]) -> Result<(), BTreeError> {
    if entries.is_empty() {
        return Err(BTreeError::InvalidCount);
    }
    if entries.windows(2).any(|pair| pair[0].key >= pair[1].key) {
        return Err(BTreeError::NoncanonicalKeyOrder);
    }
    for entry in entries {
        if entry.key.len() > BTREE_MAX_KEY_SIZE {
            return Err(BTreeError::KeyTooLarge);
        }
        u32::try_from(entry.key.len()).map_err(|_| BTreeError::LengthOverflow)?;
        u32::try_from(entry.value.len()).map_err(|_| BTreeError::LengthOverflow)?;
    }
    Ok(())
}

fn validate_internal_shape(keys: &[Vec<u8>], children: &[PageId]) -> Result<(), BTreeError> {
    if keys.is_empty() || children.len() != keys.len() + 1 {
        return Err(BTreeError::InvalidCount);
    }
    if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(BTreeError::NoncanonicalKeyOrder);
    }
    for key in keys {
        if key.len() > BTREE_MAX_KEY_SIZE {
            return Err(BTreeError::KeyTooLarge);
        }
        u32::try_from(key.len()).map_err(|_| BTreeError::LengthOverflow)?;
    }
    Ok(())
}

fn leaf_encoded_length(entries: &[LeafEntry]) -> Result<usize, BTreeError> {
    entries.iter().try_fold(LEAF_HEADER_SIZE, |total, entry| {
        u32::try_from(entry.key.len()).map_err(|_| BTreeError::LengthOverflow)?;
        u32::try_from(entry.value.len()).map_err(|_| BTreeError::LengthOverflow)?;
        total
            .checked_add(8)
            .and_then(|size| size.checked_add(entry.key.len()))
            .and_then(|size| size.checked_add(entry.value.len()))
            .ok_or(BTreeError::LengthOverflow)
    })
}

fn leaf_entry_encoded_length(entry: &LeafEntry) -> Result<usize, BTreeError> {
    u32::try_from(entry.key.len()).map_err(|_| BTreeError::LengthOverflow)?;
    u32::try_from(entry.value.len()).map_err(|_| BTreeError::LengthOverflow)?;
    8_usize
        .checked_add(entry.key.len())
        .and_then(|length| length.checked_add(entry.value.len()))
        .ok_or(BTreeError::LengthOverflow)
}

fn internal_encoded_length(keys: &[Vec<u8>]) -> Result<usize, BTreeError> {
    keys.iter().try_fold(INTERNAL_HEADER_SIZE, |total, key| {
        u32::try_from(key.len()).map_err(|_| BTreeError::LengthOverflow)?;
        total
            .checked_add(12)
            .and_then(|size| size.checked_add(key.len()))
            .ok_or(BTreeError::LengthOverflow)
    })
}

fn internal_separator_encoded_length(key: &[u8]) -> Result<usize, BTreeError> {
    u32::try_from(key.len()).map_err(|_| BTreeError::LengthOverflow)?;
    12_usize
        .checked_add(key.len())
        .ok_or(BTreeError::LengthOverflow)
}

fn ensure_leaf_entry_fits(key: &[u8], value: &[u8]) -> Result<(), BTreeError> {
    if key.len() > BTREE_MAX_KEY_SIZE {
        return Err(BTreeError::KeyTooLarge);
    }
    let entry = LeafEntry {
        key: key.to_vec(),
        value: value.to_vec(),
    };
    if leaf_encoded_length(&[entry])? > PAGE_PAYLOAD_SIZE {
        Err(BTreeError::EntryTooLarge)
    } else {
        Ok(())
    }
}

fn choose_leaf_split(entries: &[LeafEntry]) -> Result<usize, BTreeError> {
    let mut best = None;
    for split in 1..entries.len() {
        let left = leaf_encoded_length(&entries[..split])?;
        let right = leaf_encoded_length(&entries[split..])?;
        if left <= PAGE_PAYLOAD_SIZE && right <= PAGE_PAYLOAD_SIZE {
            let imbalance = left.abs_diff(right);
            if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
                best = Some((split, imbalance));
            }
        }
    }
    best.map(|(split, _)| split).ok_or(BTreeError::NoValidSplit)
}

fn choose_internal_split(keys: &[Vec<u8>]) -> Result<usize, BTreeError> {
    if keys.len() < 3 {
        return Err(BTreeError::NoValidSplit);
    }
    let mut best = None;
    for promote in 1..keys.len() - 1 {
        let left = internal_encoded_length(&keys[..promote])?;
        let right = internal_encoded_length(&keys[promote + 1..])?;
        if left <= PAGE_PAYLOAD_SIZE && right <= PAGE_PAYLOAD_SIZE {
            let imbalance = left.abs_diff(right);
            if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
                best = Some((promote, imbalance));
            }
        }
    }
    best.map(|(promote, _)| promote)
        .ok_or(BTreeError::NoValidSplit)
}

fn child_index(keys: &[Vec<u8>], key: &[u8]) -> usize {
    keys.partition_point(|separator| key >= separator.as_slice())
}

fn put_length(bytes: &mut Vec<u8>, length: usize) -> Result<(), BTreeError> {
    let length = u32::try_from(length).map_err(|_| BTreeError::LengthOverflow)?;
    bytes.extend_from_slice(&length.to_le_bytes());
    Ok(())
}

struct Cursor<'payload> {
    payload: &'payload [u8],
    offset: usize,
}

impl<'payload> Cursor<'payload> {
    const fn new(payload: &'payload [u8]) -> Self {
        Self { payload, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'payload [u8], BTreeError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(BTreeError::InvalidLength)?;
        let bytes = self
            .payload
            .get(self.offset..end)
            .ok_or(BTreeError::InvalidLength)?;
        self.offset = end;
        Ok(bytes)
    }

    fn length(&mut self) -> Result<usize, BTreeError> {
        let bytes = self.take(4)?;
        usize::try_from(read_u32(bytes)).map_err(|_| BTreeError::LengthOverflow)
    }

    fn u64(&mut self) -> Result<u64, BTreeError> {
        Ok(read_u64(self.take(8)?))
    }

    fn finish(self) -> Result<(), BTreeError> {
        if self.offset == self.payload.len() {
            Ok(())
        } else {
            Err(BTreeError::InvalidLength)
        }
    }

    const fn position(&self) -> usize {
        self.offset
    }
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut value = [0_u8; 4];
    value.copy_from_slice(bytes);
    u32::from_le_bytes(value)
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    u64::from_le_bytes(value)
}

fn scan_node(
    store: &PageStore,
    page_id: PageId,
    depth: usize,
    visited: &mut BTreeSet<PageId>,
    output: &mut Vec<KeyValue>,
) -> Result<(), BTreeError> {
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    if !visited.insert(page_id) {
        return Err(BTreeError::Cycle);
    }
    match read_node(store, page_id)? {
        Node::Leaf(entries) => {
            output.extend(entries.into_iter().map(|entry| (entry.key, entry.value)));
        }
        Node::Internal { children, .. } => {
            for child in children {
                scan_node(store, child, depth + 1, visited, output)?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn plan_range_segments_node(
    store: &PageStore,
    root: PageId,
    page_id: PageId,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
    depth: usize,
    visited: &mut BTreeSet<PageId>,
    output: &mut Vec<BTreeSegment>,
) -> Result<(), BTreeError> {
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    if !visited.insert(page_id) {
        return Err(BTreeError::Cycle);
    }
    match read_node(store, page_id)? {
        Node::Leaf(entries) => {
            let minimum = entries.first().ok_or(BTreeError::InvalidCount)?.key.clone();
            let maximum = entries.last().ok_or(BTreeError::InvalidCount)?.key.clone();
            if segment_intersects_bounds(&minimum, &maximum, lower, upper) {
                output.push(BTreeSegment {
                    root,
                    page: page_id,
                    minimum,
                    maximum,
                    entry_count: entries.len(),
                    lower: owned_bound(lower),
                    upper: owned_bound(upper),
                });
            }
        }
        Node::Internal { keys, children } => {
            for (index, child) in children.into_iter().enumerate() {
                let child_lower = index.checked_sub(1).and_then(|prior| keys.get(prior));
                let child_upper = keys.get(index);
                if child_intersects_bounds(child_lower, child_upper, lower, upper) {
                    plan_range_segments_node(
                        store,
                        root,
                        child,
                        lower,
                        upper,
                        depth + 1,
                        visited,
                        output,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn owned_bound(bound: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match bound {
        Bound::Included(value) => Bound::Included(value.to_vec()),
        Bound::Excluded(value) => Bound::Excluded(value.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn borrowed_bound(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(value) => Bound::Included(value.as_slice()),
        Bound::Excluded(value) => Bound::Excluded(value.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn range_is_empty(lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> bool {
    match (lower, upper) {
        (Bound::Unbounded, _) | (_, Bound::Unbounded) => false,
        (Bound::Included(lower), Bound::Included(upper)) => lower > upper,
        (Bound::Included(lower) | Bound::Excluded(lower), Bound::Excluded(upper))
        | (Bound::Excluded(lower), Bound::Included(upper)) => lower >= upper,
    }
}

fn segment_intersects_bounds(
    minimum: &[u8],
    maximum: &[u8],
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
) -> bool {
    let ends_after_lower = match lower {
        Bound::Included(bound) => maximum >= bound,
        Bound::Excluded(bound) => maximum > bound,
        Bound::Unbounded => true,
    };
    let starts_before_upper = match upper {
        Bound::Included(bound) => minimum <= bound,
        Bound::Excluded(bound) => minimum < bound,
        Bound::Unbounded => true,
    };
    ends_after_lower && starts_before_upper
}

fn scan_prefix_node(
    store: &PageStore,
    page_id: PageId,
    prefix: &[u8],
    upper: Option<&[u8]>,
    depth: usize,
    visited: &mut BTreeSet<PageId>,
    output: &mut Vec<KeyValue>,
) -> Result<(), BTreeError> {
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    if !visited.insert(page_id) {
        return Err(BTreeError::Cycle);
    }
    match read_node(store, page_id)? {
        Node::Leaf(entries) => {
            output.extend(
                entries
                    .into_iter()
                    .skip_while(|entry| entry.key.as_slice() < prefix)
                    .take_while(|entry| entry.key.starts_with(prefix))
                    .map(|entry| (entry.key, entry.value)),
            );
        }
        Node::Internal { keys, children } => {
            for (index, child) in children.into_iter().enumerate() {
                let child_lower = index.checked_sub(1).and_then(|prior| keys.get(prior));
                let child_upper = keys.get(index);
                let ends_after_prefix = child_upper.is_none_or(|bound| bound.as_slice() > prefix);
                let starts_before_upper =
                    upper.is_none_or(|bound| child_lower.is_none_or(|key| key.as_slice() < bound));
                if ends_after_prefix && starts_before_upper {
                    scan_prefix_node(store, child, prefix, upper, depth + 1, visited, output)?;
                }
            }
        }
    }
    Ok(())
}

struct DirectPrefixVisitor<'visit, 'store, F> {
    store: &'visit UnpublishedTail<'store>,
    prefix: &'visit [u8],
    prefix_upper: Option<&'visit [u8]>,
    visited: BTreeSet<PageId>,
    last_key: Option<Vec<u8>>,
    visitor: F,
}

impl<F> DirectPrefixVisitor<'_, '_, F>
where
    F: FnMut(&[u8], &[u8]) -> ControlFlow<()>,
{
    fn visit_node(&mut self, page_id: PageId, depth: usize) -> Result<ControlFlow<()>, BTreeError> {
        if depth >= MAX_TREE_HEIGHT {
            return Err(BTreeError::HeightExceeded);
        }
        if !self.visited.insert(page_id) {
            return Err(BTreeError::Cycle);
        }
        match read_node(self.store, page_id)? {
            Node::Leaf(entries) => {
                for entry in entries
                    .into_iter()
                    .skip_while(|entry| entry.key.as_slice() < self.prefix)
                    .take_while(|entry| entry.key.starts_with(self.prefix))
                {
                    if self
                        .last_key
                        .as_deref()
                        .is_some_and(|previous| previous >= entry.key.as_slice())
                    {
                        return Err(BTreeError::NoncanonicalKeyOrder);
                    }
                    self.last_key.replace(entry.key.clone());
                    if (self.visitor)(&entry.key, &entry.value).is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
            Node::Internal { keys, children } => {
                for (index, child) in children.into_iter().enumerate() {
                    let child_lower = index.checked_sub(1).and_then(|prior| keys.get(prior));
                    let child_upper = keys.get(index);
                    let ends_after_prefix =
                        child_upper.is_none_or(|bound| bound.as_slice() > self.prefix);
                    let starts_before_prefix_upper = self
                        .prefix_upper
                        .is_none_or(|bound| child_lower.is_none_or(|key| key.as_slice() < bound));
                    if ends_after_prefix
                        && starts_before_prefix_upper
                        && self.visit_node(child, depth + 1)?.is_break()
                    {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_prefix_node_cached(
    store: &PageStore,
    pool: &BufferPool,
    page_id: PageId,
    prefix: &[u8],
    upper: Option<&[u8]>,
    depth: usize,
    visited: &mut BTreeSet<PageId>,
    output: &mut Vec<KeyValue>,
) -> Result<(), BTreeError> {
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    if !visited.insert(page_id) {
        return Err(BTreeError::Cycle);
    }
    let frame = pool.get_or_load(store, page_id)?;
    match decode_page(frame.page())? {
        Node::Leaf(entries) => {
            output.extend(
                entries
                    .into_iter()
                    .skip_while(|entry| entry.key.as_slice() < prefix)
                    .take_while(|entry| entry.key.starts_with(prefix))
                    .map(|entry| (entry.key, entry.value)),
            );
        }
        Node::Internal { keys, children } => {
            for (index, child) in children.into_iter().enumerate() {
                let child_lower = index.checked_sub(1).and_then(|prior| keys.get(prior));
                let child_upper = keys.get(index);
                let ends_after_prefix = child_upper.is_none_or(|bound| bound.as_slice() > prefix);
                let starts_before_upper =
                    upper.is_none_or(|bound| child_lower.is_none_or(|key| key.as_slice() < bound));
                if ends_after_prefix && starts_before_upper {
                    scan_prefix_node_cached(
                        store,
                        pool,
                        child,
                        prefix,
                        upper,
                        depth + 1,
                        visited,
                        output,
                    )?;
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn visit_prefix_range_node_cached<F>(
    store: &PageStore,
    pool: &BufferPool,
    page_id: PageId,
    prefix: &[u8],
    prefix_upper: Option<&[u8]>,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
    depth: usize,
    visited: &mut BTreeSet<PageId>,
    last_key: &mut Option<Vec<u8>>,
    visitor: &mut F,
) -> Result<ControlFlow<()>, BTreeError>
where
    F: FnMut(&[u8], &[u8]) -> ControlFlow<()>,
{
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    if !visited.insert(page_id) {
        return Err(BTreeError::Cycle);
    }
    let frame = pool.get_or_load(store, page_id)?;
    match decode_page(frame.page())? {
        Node::Leaf(entries) => {
            for entry in entries
                .into_iter()
                .skip_while(|entry| entry.key.as_slice() < prefix)
                .take_while(|entry| entry.key.starts_with(prefix))
                .skip_while(|entry| !key_satisfies_lower(&entry.key, lower))
                .take_while(|entry| key_satisfies_upper(&entry.key, upper))
            {
                if last_key
                    .as_deref()
                    .is_some_and(|previous| previous >= entry.key.as_slice())
                {
                    return Err(BTreeError::NoncanonicalKeyOrder);
                }
                last_key.replace(entry.key.clone());
                if visitor(&entry.key, &entry.value).is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        Node::Internal { keys, children } => {
            for (index, child) in children.into_iter().enumerate() {
                let child_lower = index.checked_sub(1).and_then(|prior| keys.get(prior));
                let child_upper = keys.get(index);
                let ends_after_prefix = child_upper.is_none_or(|bound| bound.as_slice() > prefix);
                let starts_before_prefix_upper = prefix_upper
                    .is_none_or(|bound| child_lower.is_none_or(|key| key.as_slice() < bound));
                let intersects_bounds =
                    child_intersects_bounds(child_lower, child_upper, lower, upper);
                if ends_after_prefix
                    && starts_before_prefix_upper
                    && intersects_bounds
                    && visit_prefix_range_node_cached(
                        store,
                        pool,
                        child,
                        prefix,
                        prefix_upper,
                        lower,
                        upper,
                        depth + 1,
                        visited,
                        last_key,
                        visitor,
                    )?
                    .is_break()
                {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

#[derive(Clone, Copy)]
struct CachedTreeRead<'a> {
    store: &'a PageStore,
    pool: &'a BufferPool,
}

#[derive(Clone, Copy)]
struct CachedPrefixRange<'a> {
    prefix: &'a [u8],
    prefix_upper: Option<&'a [u8]>,
    lower: Bound<&'a [u8]>,
    upper: Bound<&'a [u8]>,
}

fn visit_prefix_range_node_cached_reverse<F>(
    read: CachedTreeRead<'_>,
    page_id: PageId,
    range: CachedPrefixRange<'_>,
    depth: usize,
    visited: &mut BTreeSet<PageId>,
    last_key: &mut Option<Vec<u8>>,
    visitor: &mut F,
) -> Result<ControlFlow<()>, BTreeError>
where
    F: FnMut(&[u8], &[u8]) -> ControlFlow<()>,
{
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    if !visited.insert(page_id) {
        return Err(BTreeError::Cycle);
    }
    let frame = read.pool.get_or_load(read.store, page_id)?;
    match decode_page(frame.page())? {
        Node::Leaf(entries) => {
            for entry in entries.into_iter().rev() {
                if entry.key.as_slice() < range.prefix {
                    break;
                }
                if !entry.key.starts_with(range.prefix)
                    || !key_satisfies_upper(&entry.key, range.upper)
                {
                    continue;
                }
                if !key_satisfies_lower(&entry.key, range.lower) {
                    break;
                }
                if last_key
                    .as_deref()
                    .is_some_and(|previous| previous <= entry.key.as_slice())
                {
                    return Err(BTreeError::NoncanonicalKeyOrder);
                }
                last_key.replace(entry.key.clone());
                if visitor(&entry.key, &entry.value).is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        Node::Internal { keys, children } => {
            for (index, child) in children.into_iter().enumerate().rev() {
                let child_lower = index.checked_sub(1).and_then(|prior| keys.get(prior));
                let child_upper = keys.get(index);
                let ends_after_prefix =
                    child_upper.is_none_or(|bound| bound.as_slice() > range.prefix);
                let starts_before_prefix_upper = range
                    .prefix_upper
                    .is_none_or(|bound| child_lower.is_none_or(|key| key.as_slice() < bound));
                let intersects_bounds =
                    child_intersects_bounds(child_lower, child_upper, range.lower, range.upper);
                if ends_after_prefix
                    && starts_before_prefix_upper
                    && intersects_bounds
                    && visit_prefix_range_node_cached_reverse(
                        read,
                        child,
                        range,
                        depth + 1,
                        visited,
                        last_key,
                        visitor,
                    )?
                    .is_break()
                {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

fn key_satisfies_lower(key: &[u8], lower: Bound<&[u8]>) -> bool {
    match lower {
        Bound::Included(bound) => key >= bound,
        Bound::Excluded(bound) => key > bound,
        Bound::Unbounded => true,
    }
}

fn key_satisfies_upper(key: &[u8], upper: Bound<&[u8]>) -> bool {
    match upper {
        Bound::Included(bound) => key <= bound,
        Bound::Excluded(bound) => key < bound,
        Bound::Unbounded => true,
    }
}

fn child_intersects_bounds(
    child_lower: Option<&Vec<u8>>,
    child_upper: Option<&Vec<u8>>,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
) -> bool {
    let ends_after_lower = match lower {
        Bound::Included(bound) | Bound::Excluded(bound) => {
            child_upper.is_none_or(|key| key.as_slice() > bound)
        }
        Bound::Unbounded => true,
    };
    let starts_before_upper = match upper {
        Bound::Included(bound) => child_lower.is_none_or(|key| key.as_slice() <= bound),
        Bound::Excluded(bound) => child_lower.is_none_or(|key| key.as_slice() < bound),
        Bound::Unbounded => true,
    };
    ends_after_lower && starts_before_upper
}

fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    let index = upper.iter().rposition(|byte| *byte != u8::MAX)?;
    upper[index] += 1;
    upper.truncate(index + 1);
    Some(upper)
}

struct ValidationSummary {
    minimum: Vec<u8>,
    maximum: Vec<u8>,
    count: usize,
    height: usize,
}

fn validate_node(
    store: &PageStore,
    page_id: PageId,
    visible_csn: Option<Csn>,
    depth: usize,
    visited: &mut BTreeSet<PageId>,
) -> Result<ValidationSummary, BTreeError> {
    if depth >= MAX_TREE_HEIGHT {
        return Err(BTreeError::HeightExceeded);
    }
    if !visited.insert(page_id) {
        return Err(BTreeError::Cycle);
    }
    let page = store.read(page_id)?;
    if visible_csn.is_some_and(|visible| {
        page.creating_csn()
            .is_none_or(|creating| creating > visible)
    }) {
        return Err(BTreeError::FuturePage);
    }
    match decode_page(&page)? {
        Node::Leaf(entries) => {
            let minimum = entries.first().ok_or(BTreeError::InvalidCount)?.key.clone();
            let maximum = entries.last().ok_or(BTreeError::InvalidCount)?.key.clone();
            Ok(ValidationSummary {
                minimum,
                maximum,
                count: entries.len(),
                height: 1,
            })
        }
        Node::Internal { keys, children } => {
            let mut summaries = Vec::with_capacity(children.len());
            for child in children {
                summaries.push(validate_node(
                    store,
                    child,
                    visible_csn,
                    depth + 1,
                    visited,
                )?);
            }
            for (separator, right) in keys.iter().zip(summaries.iter().skip(1)) {
                if separator != &right.minimum {
                    return Err(BTreeError::InvalidSeparator);
                }
            }
            if summaries
                .windows(2)
                .any(|pair| pair[0].maximum >= pair[1].minimum)
            {
                return Err(BTreeError::NoncanonicalKeyOrder);
            }
            if summaries
                .windows(2)
                .any(|pair| pair[0].height != pair[1].height)
            {
                return Err(BTreeError::Unbalanced);
            }
            let first = summaries.first().ok_or(BTreeError::InvalidCount)?;
            let last = summaries.last().ok_or(BTreeError::InvalidCount)?;
            let count = summaries.iter().try_fold(0_usize, |total, summary| {
                total
                    .checked_add(summary.count)
                    .ok_or(BTreeError::LengthOverflow)
            })?;
            Ok(ValidationSummary {
                minimum: first.minimum.clone(),
                maximum: last.maximum.clone(),
                count,
                height: first
                    .height
                    .checked_add(1)
                    .ok_or(BTreeError::HeightExceeded)?,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        ops::{Bound, ControlFlow},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use hyphae_native_pages::{BufferPool, PAGE_PAYLOAD_SIZE, PageKind, PageStore};
    use hyphae_native_types::{Csn, PageId};

    use super::{
        BTree, BTreeError, LeafEntry, Node, PREFIX_REPLACEMENT_APPENDED_PAGES,
        PrefixReplacementBatch, PrefixReplacementStructuralLimits, encode_leaf,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Result<Self, std::io::Error> {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-btree-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn page_file(&self) -> PathBuf {
            self.0.join("pages.hydb")
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(self.path());
        }
    }

    #[test]
    fn insertion_splits_and_point_reads_validate() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let mut tree = BTree::empty();
        for index in 0..1_000_u32 {
            let key = index.to_be_bytes().to_vec();
            let value = vec![u8::try_from(index % 251)?; 96];
            tree = tree
                .insert_unique(&mut store, Csn::new(u64::from(index) + 1)?, key, value)?
                .tree;
        }
        assert!(store.page_count() > 1_000);
        assert_eq!(tree.validate(&store)?, 1_000);
        assert!(tree.height(&store)? >= 2);
        for index in [0_u32, 1, 499, 999] {
            assert_eq!(
                tree.get(&store, &index.to_be_bytes())?,
                Some(vec![u8::try_from(index % 251)?; 96])
            );
        }
        let pool = BufferPool::new(16, 4)?;
        let pinned = tree
            .get_cached_pinned(&store, &pool, &499_u32.to_be_bytes())?
            .ok_or("missing pinned value")?;
        assert_eq!(pinned.bytes(), vec![u8::try_from(499 % 251)?; 96]);
        let scan = tree.scan(&store)?;
        assert_eq!(scan.len(), 1_000);
        assert!(scan.windows(2).all(|pair| pair[0].0 < pair[1].0));
        Ok(())
    }

    #[test]
    fn prefix_scan_prunes_multilevel_ranges_and_preserves_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let mut tree = BTree::empty();
        for namespace in 0..4_u8 {
            for index in 0..512_u32 {
                let mut key = vec![namespace];
                key.extend_from_slice(&index.to_be_bytes());
                tree = tree
                    .insert_unique(
                        &mut store,
                        Csn::new(u64::from(namespace) * 512 + u64::from(index) + 1)?,
                        key,
                        index.to_be_bytes().to_vec(),
                    )?
                    .tree;
            }
        }
        assert!(tree.height(&store)? >= 2);
        let matches = tree.scan_prefix(&store, &[2])?;
        assert_eq!(matches.len(), 512);
        assert!(
            matches
                .iter()
                .all(|(key, value)| key[0] == 2 && key[1..] == value[..])
        );
        assert!(matches.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(tree.scan_prefix(&store, &[4])?.is_empty());
        assert_eq!(tree.scan_prefix(&store, &[])?, tree.scan(&store)?);
        let pool = BufferPool::new(64, 4)?;
        assert_eq!(
            tree.scan_prefix_cached(&store, &pool, &[2])?,
            tree.scan_prefix(&store, &[2])?
        );
        Ok(())
    }

    #[test]
    fn immutable_leaf_segments_plan_prune_execute_and_bind_their_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let entries = (0..2_048_u32)
            .map(|index| {
                (
                    index.to_be_bytes().to_vec(),
                    vec![u8::try_from(index % 251).unwrap_or(u8::MAX); 96],
                )
            })
            .collect::<Vec<_>>();
        let tree = BTree::empty()
            .upsert_sorted_batch(&mut store, Csn::new(1)?, entries)?
            .tree;
        assert!(tree.height(&store)? >= 2);

        let all = tree.plan_range_segments(&store, Bound::Unbounded, Bound::Unbounded)?;
        let lower = 700_u32.to_be_bytes();
        let upper = 1_400_u32.to_be_bytes();
        let planned = tree.plan_range_segments(
            &store,
            Bound::Included(lower.as_slice()),
            Bound::Excluded(upper.as_slice()),
        )?;
        assert!(!planned.is_empty());
        assert!(planned.len() < all.len());
        assert!(
            planned
                .windows(2)
                .all(|pair| pair[0].maximum_key() < pair[1].minimum_key())
        );
        assert!(planned.iter().all(|segment| segment.entry_count() > 0));

        let observed = planned
            .iter()
            .map(|segment| tree.scan_planned_segment(&store, segment))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let expected = tree
            .scan(&store)?
            .into_iter()
            .filter(|(key, _)| {
                key.as_slice() >= lower.as_slice() && key.as_slice() < upper.as_slice()
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, expected);
        assert!(
            tree.plan_range_segments(
                &store,
                Bound::Excluded(upper.as_slice()),
                Bound::Included(lower.as_slice()),
            )?
            .is_empty()
        );

        let newer = tree
            .upsert(
                &mut store,
                Csn::new(2)?,
                2_048_u32.to_be_bytes().to_vec(),
                b"new-root".to_vec(),
            )?
            .tree;
        assert!(matches!(
            newer.scan_planned_segment(&store, &planned[0]),
            Err(BTreeError::ForeignSegment)
        ));
        Ok(())
    }

    #[test]
    fn prefix_scan_supports_unbounded_ff_suffixes() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let mut tree = BTree::empty();
        for key in [
            vec![0xfe, 0xff],
            vec![0xff],
            vec![0xff, 0x00],
            vec![0xff, 0xff],
            vec![0xff, 0xff, 0x00],
        ] {
            tree = tree
                .insert_unique(&mut store, Csn::new(1)?, key.clone(), key)?
                .tree;
        }
        assert_eq!(tree.scan_prefix(&store, &[0xff])?.len(), 4);
        assert_eq!(tree.scan_prefix(&store, &[0xff, 0xff])?.len(), 2);
        Ok(())
    }

    #[test]
    fn cached_prefix_visitor_resumes_exclusively_and_stops_early()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let mut tree = BTree::empty();
        for namespace in 0..4_u8 {
            for index in 0..512_u32 {
                let mut key = vec![namespace];
                key.extend_from_slice(&index.to_be_bytes());
                tree = tree
                    .insert_unique(
                        &mut store,
                        Csn::new(u64::from(namespace) * 512 + u64::from(index) + 1)?,
                        key,
                        index.to_be_bytes().to_vec(),
                    )?
                    .tree;
            }
        }
        let expected = tree.scan_prefix(&store, &[2])?;
        let pool = BufferPool::new(64, 4)?;
        let mut first = Vec::new();
        let outcome = tree.visit_prefix_cached(&store, &pool, &[2], None, |key, value| {
            first.push((key.to_vec(), value.to_vec()));
            if first.len() == 7 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        assert_eq!(outcome, ControlFlow::Break(()));
        assert_eq!(first, expected[..7]);

        let cursor = first.last().ok_or("missing first page")?.0.clone();
        let mut second = Vec::new();
        let outcome =
            tree.visit_prefix_cached(&store, &pool, &[2], Some(&cursor), |key, value| {
                second.push((key.to_vec(), value.to_vec()));
                if second.len() == 5 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })?;
        assert_eq!(outcome, ControlFlow::Break(()));
        assert_eq!(second, expected[7..12]);

        let final_cursor = expected.last().ok_or("missing expected rows")?.0.as_slice();
        let mut exhausted = Vec::new();
        let outcome =
            tree.visit_prefix_cached(&store, &pool, &[2], Some(final_cursor), |key, value| {
                exhausted.push((key.to_vec(), value.to_vec()));
                ControlFlow::Continue(())
            })?;
        assert_eq!(outcome, ControlFlow::Continue(()));
        assert!(exhausted.is_empty());
        Ok(())
    }

    #[test]
    fn cached_prefix_range_visitor_honors_bounds_and_stops_early()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let mut tree = BTree::empty();
        for namespace in 0..4_u8 {
            for index in 0..512_u32 {
                let mut key = vec![namespace];
                key.extend_from_slice(&index.to_be_bytes());
                tree = tree
                    .insert_unique(
                        &mut store,
                        Csn::new(u64::from(namespace) * 512 + u64::from(index) + 1)?,
                        key,
                        index.to_be_bytes().to_vec(),
                    )?
                    .tree;
            }
        }
        assert!(tree.height(&store)? >= 2);
        let pool = BufferPool::new(64, 4)?;
        let key = |index: u32| {
            let mut key = vec![2];
            key.extend_from_slice(&index.to_be_bytes());
            key
        };

        let lower = key(100);
        let upper = key(110);
        let mut half_open = Vec::new();
        let outcome = tree.visit_prefix_range_cached(
            &store,
            &pool,
            &[2],
            Bound::Included(lower.as_slice()),
            Bound::Excluded(upper.as_slice()),
            |key, value| {
                half_open.push((key.to_vec(), value.to_vec()));
                ControlFlow::Continue(())
            },
        )?;
        assert_eq!(outcome, ControlFlow::Continue(()));
        let decoded = half_open
            .iter()
            .map(|(_, value)| value.as_slice().try_into().map(u32::from_be_bytes))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(decoded, (100_u32..110).collect::<Vec<_>>());

        let inclusive_upper = key(105);
        let mut mixed_bytes = Vec::new();
        let outcome = tree.visit_prefix_range_cached(
            &store,
            &pool,
            &[2],
            Bound::Excluded(lower.as_slice()),
            Bound::Included(inclusive_upper.as_slice()),
            |_, value| {
                mixed_bytes.push(value.to_vec());
                if mixed_bytes.len() == 3 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )?;
        assert_eq!(outcome, ControlFlow::Break(()));
        let mixed = mixed_bytes
            .iter()
            .map(|value| value.as_slice().try_into().map(u32::from_be_bytes))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(mixed, [101, 102, 103]);

        let outside_namespace = key(512);
        let mut empty = Vec::new();
        let outcome = tree.visit_prefix_range_cached(
            &store,
            &pool,
            &[2],
            Bound::Included(outside_namespace.as_slice()),
            Bound::Unbounded,
            |key, value| {
                empty.push((key.to_vec(), value.to_vec()));
                ControlFlow::Continue(())
            },
        )?;
        assert_eq!(outcome, ControlFlow::Continue(()));
        assert!(empty.is_empty());

        let reversed_lower = key(300);
        let reversed_upper = key(200);
        let mut reversed = Vec::new();
        let outcome = tree.visit_prefix_range_cached(
            &store,
            &pool,
            &[2],
            Bound::Included(reversed_lower.as_slice()),
            Bound::Included(reversed_upper.as_slice()),
            |key, value| {
                reversed.push((key.to_vec(), value.to_vec()));
                ControlFlow::Continue(())
            },
        )?;
        assert_eq!(outcome, ControlFlow::Continue(()));
        assert!(reversed.is_empty());
        Ok(())
    }

    #[test]
    fn cached_reverse_prefix_range_visitor_honors_bounds_and_stops_early()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let mut tree = BTree::empty();
        for namespace in 0..4_u8 {
            for index in 0..256_u32 {
                let mut key = vec![namespace];
                key.extend_from_slice(&index.to_be_bytes());
                tree = tree
                    .insert_unique(
                        &mut store,
                        Csn::new(u64::from(namespace) * 256 + u64::from(index) + 1)?,
                        key,
                        index.to_be_bytes().to_vec(),
                    )?
                    .tree;
            }
        }
        assert!(tree.height(&store)? >= 2);
        let pool = BufferPool::new(64, 4)?;
        let key = |index: u32| {
            let mut key = vec![2];
            key.extend_from_slice(&index.to_be_bytes());
            key
        };

        let lower = key(100);
        let upper = key(110);
        let mut half_open = Vec::new();
        let outcome = tree.visit_prefix_range_cached_reverse(
            &store,
            &pool,
            &[2],
            Bound::Included(lower.as_slice()),
            Bound::Excluded(upper.as_slice()),
            |_, value| {
                half_open.push(value.to_vec());
                ControlFlow::Continue(())
            },
        )?;
        assert_eq!(outcome, ControlFlow::Continue(()));
        let decoded = half_open
            .iter()
            .map(|value| value.as_slice().try_into().map(u32::from_be_bytes))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(decoded, (100_u32..110).rev().collect::<Vec<_>>());

        let inclusive_upper = key(105);
        let mut stopped = Vec::new();
        let outcome = tree.visit_prefix_range_cached_reverse(
            &store,
            &pool,
            &[2],
            Bound::Excluded(lower.as_slice()),
            Bound::Included(inclusive_upper.as_slice()),
            |_, value| {
                stopped.push(value.to_vec());
                if stopped.len() == 3 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )?;
        assert_eq!(outcome, ControlFlow::Break(()));
        let stopped = stopped
            .iter()
            .map(|value| value.as_slice().try_into().map(u32::from_be_bytes))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(stopped, [105, 104, 103]);

        let mut empty = Vec::new();
        let outcome = tree.visit_prefix_range_cached_reverse(
            &store,
            &pool,
            &[2],
            Bound::Included(key(200).as_slice()),
            Bound::Included(key(100).as_slice()),
            |key, _| {
                empty.push(key.to_vec());
                ControlFlow::Continue(())
            },
        )?;
        assert_eq!(outcome, ControlFlow::Continue(()));
        assert!(empty.is_empty());
        Ok(())
    }

    #[test]
    fn old_root_remains_readable_after_copy_on_write() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let first = BTree::empty()
            .insert_unique(&mut store, Csn::new(1)?, b"a".to_vec(), b"one".to_vec())?
            .tree;
        let second = first
            .insert_unique(&mut store, Csn::new(2)?, b"b".to_vec(), b"two".to_vec())?
            .tree;
        assert_eq!(first.get(&store, b"a")?, Some(b"one".to_vec()));
        assert_eq!(first.get(&store, b"b")?, None);
        assert_eq!(second.get(&store, b"b")?, Some(b"two".to_vec()));
        Ok(())
    }

    #[test]
    fn duplicate_and_upsert_semantics_are_explicit() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let first = BTree::empty()
            .insert_unique(&mut store, Csn::new(1)?, b"k".to_vec(), b"one".to_vec())?
            .tree;
        assert!(matches!(
            first.insert_unique(&mut store, Csn::new(2)?, b"k".to_vec(), b"two".to_vec(),),
            Err(BTreeError::DuplicateKey)
        ));
        let updated = first.upsert(&mut store, Csn::new(2)?, b"k".to_vec(), b"two".to_vec())?;
        assert_eq!(updated.previous, Some(b"one".to_vec()));
        assert_eq!(updated.tree.get(&store, b"k")?, Some(b"two".to_vec()));
        let pool = BufferPool::new(4, 2)?;
        assert_eq!(
            updated.tree.get_cached(&store, &pool, b"k")?,
            Some(b"two".to_vec())
        );
        Ok(())
    }

    #[test]
    fn ordered_batch_coalesces_copy_on_write_paths_and_preserves_old_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let batch_directory = TestDirectory::create()?;
        let sequential_directory = TestDirectory::create()?;
        let mut batch_store = PageStore::create(batch_directory.page_file())?;
        let mut sequential_store = PageStore::create(sequential_directory.page_file())?;
        let mut batch_tree = BTree::empty();
        let mut sequential_tree = BTree::empty();
        for index in 0..2_048_u32 {
            let key = index.to_be_bytes().to_vec();
            let value = vec![u8::try_from(index % 251)?; 64];
            batch_tree = batch_tree
                .insert_unique(&mut batch_store, Csn::new(1)?, key.clone(), value.clone())?
                .tree;
            sequential_tree = sequential_tree
                .insert_unique(&mut sequential_store, Csn::new(1)?, key, value)?
                .tree;
        }
        assert!(batch_tree.height(&batch_store)? >= 2);
        let old_batch_tree = batch_tree;
        let updates = (512..768_u32)
            .map(|index| -> Result<_, std::num::TryFromIntError> {
                Ok((
                    index.to_be_bytes().to_vec(),
                    vec![u8::try_from(index % 197)?; 96],
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let batch =
            batch_tree.upsert_sorted_batch(&mut batch_store, Csn::new(2)?, updates.clone())?;
        let sequential_start = sequential_store.page_count();
        for (key, value) in updates {
            sequential_tree = sequential_tree
                .upsert(&mut sequential_store, Csn::new(2)?, key, value)?
                .tree;
        }
        let sequential_pages = usize::try_from(
            sequential_store
                .page_count()
                .checked_sub(sequential_start)
                .ok_or("sequential page count regressed")?,
        )?;

        assert!(batch.pages_written < sequential_pages);
        assert_eq!(
            batch.tree.scan(&batch_store)?,
            sequential_tree.scan(&sequential_store)?
        );
        assert_eq!(
            old_batch_tree.get(&batch_store, &512_u32.to_be_bytes())?,
            Some(vec![u8::try_from(512 % 251)?; 64])
        );
        assert_eq!(old_batch_tree.validate(&batch_store)?, 2_048);
        assert_eq!(batch.tree.validate(&batch_store)?, 2_048);
        Ok(())
    }

    #[test]
    fn ordered_batch_rejects_invalid_input_before_appending_pages()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let tree = BTree::empty()
            .insert_unique(
                &mut store,
                Csn::new(1)?,
                b"existing".to_vec(),
                b"value".to_vec(),
            )?
            .tree;
        let initial_pages = store.page_count();

        let empty = tree.upsert_sorted_batch(&mut store, Csn::new(2)?, Vec::new())?;
        assert_eq!(empty.tree, tree);
        assert_eq!(empty.pages_written, 0);
        assert_eq!(store.page_count(), initial_pages);

        for invalid in [
            vec![
                (b"duplicate".to_vec(), b"one".to_vec()),
                (b"duplicate".to_vec(), b"two".to_vec()),
            ],
            vec![
                (b"second".to_vec(), b"two".to_vec()),
                (b"first".to_vec(), b"one".to_vec()),
            ],
        ] {
            assert!(matches!(
                tree.upsert_sorted_batch(&mut store, Csn::new(2)?, invalid),
                Err(BTreeError::NoncanonicalKeyOrder)
            ));
            assert_eq!(store.page_count(), initial_pages);
        }
        assert!(matches!(
            tree.upsert_sorted_batch(
                &mut store,
                Csn::new(2)?,
                vec![(b"oversized".to_vec(), vec![0; PAGE_PAYLOAD_SIZE])],
            ),
            Err(BTreeError::EntryTooLarge)
        ));
        assert_eq!(store.page_count(), initial_pages);
        Ok(())
    }

    #[test]
    fn ordered_batch_builds_a_balanced_multilevel_tree_from_empty()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let entries = (0..4_096_u32)
            .map(|index| -> Result<_, std::num::TryFromIntError> {
                Ok((
                    index.to_be_bytes().to_vec(),
                    vec![u8::try_from(index % 251)?; 96],
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = BTree::empty().upsert_sorted_batch(&mut store, Csn::new(1)?, entries)?;

        assert_eq!(u64::try_from(batch.pages_written)?, store.page_count());
        assert!(batch.tree.height(&store)? >= 2);
        assert_eq!(
            batch.tree.reachable_page_count(&store)?,
            batch.pages_written
        );
        assert_eq!(batch.tree.validate(&store)?, 4_096);
        for index in [0_u32, 1, 2_047, 4_095] {
            assert_eq!(
                batch.tree.get(&store, &index.to_be_bytes())?,
                Some(vec![u8::try_from(index % 251)?; 96])
            );
        }
        Ok(())
    }

    #[test]
    fn ordered_batch_retains_unaffected_root_children() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let mut tree = BTree::empty();
        for index in 0..2_048_u32 {
            tree = tree
                .insert_unique(
                    &mut store,
                    Csn::new(1)?,
                    index.to_be_bytes().to_vec(),
                    vec![u8::try_from(index % 251)?; 64],
                )?
                .tree;
        }
        let target = 1_024_u32.to_be_bytes();
        let (old_keys, old_children) =
            match super::read_node(&store, tree.root().ok_or("missing old root")?)? {
                Node::Internal { keys, children } => (keys, children),
                Node::Leaf(_) => return Err("expected a multilevel old tree".into()),
            };
        let changed_child = super::child_index(&old_keys, &target);
        let batch = tree.upsert_sorted_batch(
            &mut store,
            Csn::new(2)?,
            vec![(target.to_vec(), vec![b'x'; 64])],
        )?;
        let new_children =
            match super::read_node(&store, batch.tree.root().ok_or("missing new root")?)? {
                Node::Internal { children, .. } => children,
                Node::Leaf(_) => return Err("expected a multilevel new tree".into()),
            };

        assert_eq!(new_children.len(), old_children.len());
        for (index, (old, new)) in old_children.iter().zip(&new_children).enumerate() {
            if index == changed_child {
                assert_ne!(new, old);
            } else {
                assert_eq!(new, old);
            }
        }
        Ok(())
    }

    #[test]
    fn visible_validation_rejects_future_nodes() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.path().join("pages.hydb"))?;
        let tree = BTree::empty()
            .insert_unique(&mut store, Csn::new(2)?, b"k".to_vec(), b"value".to_vec())?
            .tree;
        assert!(matches!(
            tree.validate_visible(&store, Csn::new(1)?),
            Err(BTreeError::FuturePage)
        ));
        assert_eq!(tree.validate_visible(&store, Csn::new(2)?)?, 1);
        Ok(())
    }

    #[test]
    fn oversized_entry_and_noncanonical_leaf_fail_closed() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        assert!(matches!(
            BTree::empty().insert_unique(
                &mut store,
                Csn::new(1)?,
                b"k".to_vec(),
                vec![0; hyphae_native_pages::PAGE_PAYLOAD_SIZE],
            ),
            Err(BTreeError::EntryTooLarge)
        ));
        let duplicate = vec![
            LeafEntry {
                key: b"k".to_vec(),
                value: b"1".to_vec(),
            },
            LeafEntry {
                key: b"k".to_vec(),
                value: b"2".to_vec(),
            },
        ];
        assert!(matches!(
            encode_leaf(&duplicate),
            Err(BTreeError::NoncanonicalKeyOrder)
        ));
        Ok(())
    }

    #[test]
    fn borrowed_lookup_validates_the_complete_leaf_after_a_match()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let mut payload = encode_leaf(&[
            LeafEntry {
                key: b"a".to_vec(),
                value: b"1".to_vec(),
            },
            LeafEntry {
                key: b"b".to_vec(),
                value: b"2".to_vec(),
            },
        ])?;
        payload[34] = b'a';
        let root = store.append(PageKind::BTreeLeaf, Some(Csn::new(1)?), None, payload)?;
        let pool = BufferPool::new(2, 1)?;
        assert!(matches!(
            BTree::from_root(root).get_cached_pinned(&store, &pool, b"a"),
            Err(BTreeError::NoncanonicalKeyOrder)
        ));
        Ok(())
    }

    #[test]
    fn bounded_prefix_replacement_reuses_unaffected_leaves_and_is_exact()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let entries = (0..3_u8)
            .flat_map(|namespace| {
                (0..512_u32).map(move |index| {
                    let mut key = vec![namespace];
                    key.extend_from_slice(&index.to_be_bytes());
                    (key, vec![namespace; 32])
                })
            })
            .collect::<Vec<_>>();
        let tree = BTree::empty()
            .upsert_sorted_batch(&mut store, Csn::new(1)?, entries)?
            .tree;
        let before_unaffected = tree.scan_prefix(&store, &[0])?;
        let before_pages = tree
            .plan_range_segments(&store, Bound::Unbounded, Bound::Unbounded)?
            .into_iter()
            .filter(|segment| segment.maximum_key().first() == Some(&0))
            .map(|segment| segment.page_id())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = tree
            .scan_prefix(&store, &[1])?
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>();
        let replacements = (0..7_u32)
            .map(|index| {
                let mut key = vec![1];
                key.extend_from_slice(&index.to_be_bytes());
                (key, b"replacement".to_vec())
            })
            .collect::<Vec<_>>();
        let replacement = tree.replace_prefixes_sorted_batch_with_control(
            &mut store,
            Csn::new(2)?,
            &[vec![1]],
            &expected,
            replacements.clone(),
            || ControlFlow::Continue(()),
        )?;

        assert_eq!(
            replacement.tree.scan_prefix(&store, &[0])?,
            before_unaffected
        );
        assert_eq!(replacement.tree.scan_prefix(&store, &[1])?, replacements);
        let after_pages = replacement
            .tree
            .plan_range_segments(&store, Bound::Unbounded, Bound::Unbounded)?
            .into_iter()
            .map(|segment| segment.page_id())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(before_pages.iter().all(|page| after_pages.contains(page)));
        replacement.tree.validate(&store)?;
        Ok(())
    }

    #[test]
    fn prefix_replacement_planner_bounds_unrelated_leaf_structure_before_append()
    -> Result<(), Box<dyn std::error::Error>> {
        let small_directory = TestDirectory::create()?;
        let large_directory = TestDirectory::create()?;
        let mut small_store = PageStore::create(small_directory.page_file())?;
        let mut large_store = PageStore::create(large_directory.page_file())?;
        let target_key = b"target/only".to_vec();
        let replacement = vec![(target_key.clone(), b"replacement".to_vec())];
        let small = BTree::empty()
            .upsert_sorted_batch(
                &mut small_store,
                Csn::new(1)?,
                vec![(target_key.clone(), b"old".to_vec())],
            )?
            .tree;
        let mut large_entries = (0..4_096_u32)
            .map(|index| {
                let mut key = b"unrelated/".to_vec();
                key.extend_from_slice(&index.to_be_bytes());
                (key, vec![b'u'; 256])
            })
            .collect::<Vec<_>>();
        large_entries.insert(0, (target_key.clone(), b"old".to_vec()));
        large_entries.sort_by(|left, right| left.0.cmp(&right.0));
        let large = BTree::empty()
            .upsert_sorted_batch(&mut large_store, Csn::new(1)?, large_entries)?
            .tree;

        let small_plan = small.plan_prefixes_sorted_batch_replacement_with_control(
            &small_store,
            &[b"target/".to_vec()],
            std::slice::from_ref(&target_key),
            &replacement,
            || ControlFlow::Continue(()),
        )?;
        let large_plan = large.plan_prefixes_sorted_batch_replacement_with_limits_and_control(
            &large_store,
            PrefixReplacementStructuralLimits::new(1, target_key.len())?,
            || ControlFlow::Continue(()),
        )?;

        assert!(large_plan.leaf_count() > small_plan.leaf_count());
        assert!(large_plan.reachable_page_count() > large_plan.leaf_count());
        assert!(large_plan.maximum_stack_depth() <= super::MAX_TREE_HEIGHT);
        assert!(
            large_plan.structural_peak_memory_bytes() > small_plan.structural_peak_memory_bytes()
        );
        Ok(())
    }

    #[test]
    fn unpublished_candidate_supports_direct_validation_and_scalar_limit_rejection()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let target_key = b"target/only".to_vec();
        let tree = BTree::empty()
            .upsert_sorted_batch(
                &mut store,
                Csn::new(1)?,
                vec![(target_key.clone(), b"old".to_vec())],
            )?
            .tree;
        let plan = tree.plan_prefixes_sorted_batch_replacement_with_limits_and_control(
            &store,
            PrefixReplacementStructuralLimits::new(1, target_key.len())?,
            || ControlFlow::Continue(()),
        )?;
        let pages_before = store.page_count();
        {
            let mut unpublished = store.begin_unpublished_tail()?;
            let candidate = tree.replace_prefixes_sorted_batch_in_unpublished_tail_with_control(
                &mut unpublished,
                &plan,
                PrefixReplacementBatch {
                    creating_csn: Csn::new(2)?,
                    prefixes: &[b"target/".to_vec()],
                    expected_keys: std::slice::from_ref(&target_key),
                    replacements: vec![(target_key.clone(), b"replacement".to_vec())],
                },
                || ControlFlow::Continue(()),
            )?;
            let candidate_stats = super::inspect_tree_structure_with_control(
                &unpublished,
                candidate.tree.root(),
                &mut || ControlFlow::Continue(()),
            )?;
            assert_eq!(candidate_stats.leaf_count, plan.leaf_count());
            assert_eq!(
                candidate.tree.get_unpublished(&unpublished, &target_key)?,
                Some(b"replacement".to_vec())
            );
            let mut visited_target = 0;
            assert_eq!(
                candidate.tree.visit_prefix_unpublished(
                    &unpublished,
                    b"target/",
                    |key, value| {
                        assert_eq!(key, target_key.as_slice());
                        assert_eq!(value, b"replacement");
                        visited_target += 1;
                        ControlFlow::Continue(())
                    },
                )?,
                ControlFlow::Continue(())
            );
            assert_eq!(visited_target, 1);
            unpublished.rollback()?;
        }
        assert_eq!(store.page_count(), pages_before);
        assert_eq!(tree.get(&store, &target_key)?, Some(b"old".to_vec()));

        let mut unpublished = store.begin_unpublished_tail()?;
        assert!(matches!(
            tree.replace_prefixes_sorted_batch_in_unpublished_tail_with_control(
                &mut unpublished,
                &plan,
                PrefixReplacementBatch {
                    creating_csn: Csn::new(2)?,
                    prefixes: &[b"target/".to_vec()],
                    expected_keys: std::slice::from_ref(&target_key),
                    replacements: vec![
                        (b"target/a".to_vec(), b"one".to_vec()),
                        (b"target/b".to_vec(), b"two".to_vec()),
                    ],
                },
                || ControlFlow::Continue(()),
            ),
            Err(BTreeError::ForeignSegment)
        ));
        assert_eq!(unpublished.appended_page_count(), 0);
        unpublished.rollback()?;
        Ok(())
    }

    #[test]
    fn prefix_replacement_planner_rejects_duplicate_children_and_cycles()
    -> Result<(), Box<dyn std::error::Error>> {
        let duplicate_directory = TestDirectory::create()?;
        let mut duplicate_store = PageStore::create(duplicate_directory.page_file())?;
        let leaf = duplicate_store.append(
            PageKind::BTreeLeaf,
            Some(Csn::new(1)?),
            None,
            Node::Leaf(vec![LeafEntry {
                key: b"a".to_vec(),
                value: b"value".to_vec(),
            }])
            .encode()?,
        )?;
        let duplicate_root = duplicate_store.append(
            PageKind::BTreeInternal,
            Some(Csn::new(1)?),
            None,
            Node::Internal {
                keys: vec![b"a".to_vec()],
                children: vec![leaf, leaf],
            }
            .encode()?,
        )?;
        assert!(matches!(
            BTree::from_root(duplicate_root).plan_prefixes_sorted_batch_replacement_with_control(
                &duplicate_store,
                &[b"z".to_vec()],
                &[],
                &[],
                || ControlFlow::Continue(()),
            ),
            Err(BTreeError::NoncanonicalKeyOrder)
        ));

        let cycle_directory = TestDirectory::create()?;
        let mut cycle_store = PageStore::create(cycle_directory.page_file())?;
        let cycle_leaf = cycle_store.append(
            PageKind::BTreeLeaf,
            Some(Csn::new(1)?),
            None,
            Node::Leaf(vec![LeafEntry {
                key: b"a".to_vec(),
                value: b"value".to_vec(),
            }])
            .encode()?,
        )?;
        let self_page = PageId::new(cycle_store.page_count() + 1)?;
        let cycle_root = cycle_store.append(
            PageKind::BTreeInternal,
            Some(Csn::new(1)?),
            None,
            Node::Internal {
                keys: vec![b"a".to_vec()],
                children: vec![self_page, cycle_leaf],
            }
            .encode()?,
        )?;
        assert_eq!(cycle_root, self_page);
        assert!(matches!(
            BTree::from_root(cycle_root).plan_prefixes_sorted_batch_replacement_with_control(
                &cycle_store,
                &[b"z".to_vec()],
                &[],
                &[],
                || ControlFlow::Continue(()),
            ),
            Err(BTreeError::Cycle)
        ));
        Ok(())
    }

    #[test]
    fn bounded_prefix_replacement_fails_before_append_on_stale_or_cancelled_capture()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let tree = BTree::empty()
            .upsert_sorted_batch(
                &mut store,
                Csn::new(1)?,
                vec![(b"target/a".to_vec(), b"one".to_vec())],
            )?
            .tree;
        let pages_before = store.page_count();
        assert!(matches!(
            tree.replace_prefixes_sorted_batch_with_control(
                &mut store,
                Csn::new(2)?,
                &[b"target/".to_vec()],
                &[b"target/missing".to_vec()],
                Vec::new(),
                || ControlFlow::Continue(()),
            ),
            Err(BTreeError::PrefixContentsChanged)
        ));
        assert_eq!(store.page_count(), pages_before);
        assert!(matches!(
            tree.replace_prefixes_sorted_batch_with_control(
                &mut store,
                Csn::new(2)?,
                &[b"target/".to_vec()],
                &[b"target/a".to_vec()],
                Vec::new(),
                || ControlFlow::Break(()),
            ),
            Err(BTreeError::Cancelled)
        ));
        assert_eq!(store.page_count(), pages_before);
        Ok(())
    }

    #[test]
    fn bounded_prefix_replacement_rejects_cross_leaf_overlap_before_append()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let mut store = PageStore::create(temporary.page_file())?;
        let left = store.append(
            PageKind::BTreeLeaf,
            Some(Csn::new(1)?),
            None,
            Node::Leaf(vec![
                LeafEntry {
                    key: b"a".to_vec(),
                    value: vec![1],
                },
                LeafEntry {
                    key: b"c".to_vec(),
                    value: vec![1],
                },
            ])
            .encode()?,
        )?;
        let right = store.append(
            PageKind::BTreeLeaf,
            Some(Csn::new(1)?),
            None,
            Node::Leaf(vec![
                LeafEntry {
                    key: b"b".to_vec(),
                    value: vec![2],
                },
                LeafEntry {
                    key: b"d".to_vec(),
                    value: vec![2],
                },
            ])
            .encode()?,
        )?;
        let root = store.append(
            PageKind::BTreeInternal,
            Some(Csn::new(1)?),
            None,
            Node::Internal {
                keys: vec![b"b".to_vec()],
                children: vec![left, right],
            }
            .encode()?,
        )?;
        let pages_before = store.page_count();
        assert!(matches!(
            BTree::from_root(root).replace_prefixes_sorted_batch_with_control(
                &mut store,
                Csn::new(2)?,
                &[b"z".to_vec()],
                &[],
                Vec::new(),
                || ControlFlow::Continue(()),
            ),
            Err(BTreeError::NoncanonicalKeyOrder)
        ));
        assert_eq!(store.page_count(), pages_before);
        Ok(())
    }

    #[test]
    fn bounded_prefix_replacement_rolls_back_a_cancelled_appended_tail()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TestDirectory::create()?;
        let page_file = temporary.page_file();
        let mut store = PageStore::create(&page_file)?;
        let entries = (0..1_024_u32)
            .map(|index| {
                let mut key = b"target/".to_vec();
                key.extend_from_slice(&index.to_be_bytes());
                (key, vec![1; 64])
            })
            .collect::<Vec<_>>();
        let tree = BTree::empty()
            .upsert_sorted_batch(&mut store, Csn::new(1)?, entries.clone())?
            .tree;
        let expected = entries.into_iter().map(|(key, _)| key).collect::<Vec<_>>();
        let pages_before = store.page_count();
        assert!(matches!(
            tree.replace_prefixes_sorted_batch_with_control(
                &mut store,
                Csn::new(2)?,
                &[b"target/".to_vec()],
                &expected,
                vec![(b"target/replacement".to_vec(), vec![2; 64])],
                || {
                    if PREFIX_REPLACEMENT_APPENDED_PAGES.get() == 0 {
                        ControlFlow::Continue(())
                    } else {
                        ControlFlow::Break(())
                    }
                },
            ),
            Err(BTreeError::Cancelled)
        ));
        assert!(PREFIX_REPLACEMENT_APPENDED_PAGES.get() > 0);
        assert_eq!(store.page_count(), pages_before);
        assert_eq!(tree.scan_prefix(&store, b"target/")?.len(), 1_024);
        drop(store);
        let reopened = PageStore::open(page_file)?;
        assert_eq!(reopened.page_count(), pages_before);
        assert_eq!(tree.scan_prefix(&reopened, b"target/")?.len(), 1_024);
        Ok(())
    }

    #[test]
    fn leaf_encoding_has_a_stable_golden() -> Result<(), Box<dyn std::error::Error>> {
        let encoded = Node::Leaf(vec![
            LeafEntry {
                key: b"a".to_vec(),
                value: b"one".to_vec(),
            },
            LeafEntry {
                key: b"b".to_vec(),
                value: b"two".to_vec(),
            },
        ])
        .encode()?;
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "92def2e785f2d3e185cd52f89b98d548659f1471a7a2605472f1bf85eb7ec8ac"
        );
        Ok(())
    }
}
