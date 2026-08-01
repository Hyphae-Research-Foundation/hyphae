// SPDX-License-Identifier: Apache-2.0

//! Immutable copy-on-write B+tree over verified Hyphae native pages.

use std::{
    collections::BTreeSet,
    ops::{ControlFlow, Range},
    sync::Arc,
};

use hyphae_native_pages::{
    BufferPool, BufferPoolError, PAGE_PAYLOAD_SIZE, Page, PageFrame, PageKind, PageStore,
    PageStoreError,
};
use hyphae_native_types::{Csn, PageId};
use thiserror::Error;

const LEAF_MAGIC: &[u8; 8] = b"HYBTLF01";
const INTERNAL_MAGIC: &[u8; 8] = b"HYBTIN01";
const FORMAT_VERSION: u16 = 1;
const LEAF_HEADER_SIZE: usize = 16;
const INTERNAL_HEADER_SIZE: usize = 24;
const MAX_TREE_HEIGHT: usize = 64;
/// Maximum canonical inline B+tree key size.
pub const BTREE_MAX_KEY_SIZE: usize = 4_096;

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

/// Immutable root identity for one binary B+tree generation.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct BTree {
    root: Option<PageId>,
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

    /// Performs a binary point lookup.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt pages/nodes, I/O, cycles, or excessive
    /// height.
    pub fn get(self, store: &PageStore, key: &[u8]) -> Result<Option<Vec<u8>>, BTreeError> {
        let Some(mut page_id) = self.root else {
            return Ok(None);
        };
        let mut visited = [0_u64; MAX_TREE_HEIGHT];
        for depth in 0..MAX_TREE_HEIGHT {
            if visited[..depth].contains(&page_id.get()) {
                return Err(BTreeError::Cycle);
            }
            visited[depth] = page_id.get();
            let page = store.read(page_id)?;
            match lookup_page(&page, key)? {
                LookupStep::Value(range) => {
                    return Ok(range.map(|range| page.payload()[range].to_vec()));
                }
                LookupStep::Descend(child) => page_id = child,
            }
        }
        Err(BTreeError::HeightExceeded)
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
        mut visitor: F,
    ) -> Result<ControlFlow<()>, BTreeError>
    where
        F: FnMut(&[u8], &[u8]) -> ControlFlow<()>,
    {
        let Some(root) = self.root else {
            return Ok(ControlFlow::Continue(()));
        };
        let upper = prefix_upper_bound(prefix);
        let mut visited = BTreeSet::new();
        let mut last_key = None;
        visit_prefix_node_cached(
            store,
            pool,
            root,
            prefix,
            upper.as_deref(),
            start_after,
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

fn append_node(
    store: &mut PageStore,
    creating_csn: Csn,
    node: &Node,
) -> Result<PageId, BTreeError> {
    Ok(store.append(node.page_kind(), Some(creating_csn), None, node.encode()?)?)
}

fn read_node(store: &PageStore, page_id: PageId) -> Result<Node, BTreeError> {
    let page = store.read(page_id)?;
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

fn internal_encoded_length(keys: &[Vec<u8>]) -> Result<usize, BTreeError> {
    keys.iter().try_fold(INTERNAL_HEADER_SIZE, |total, key| {
        u32::try_from(key.len()).map_err(|_| BTreeError::LengthOverflow)?;
        total
            .checked_add(12)
            .and_then(|size| size.checked_add(key.len()))
            .ok_or(BTreeError::LengthOverflow)
    })
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
fn visit_prefix_node_cached<F>(
    store: &PageStore,
    pool: &BufferPool,
    page_id: PageId,
    prefix: &[u8],
    upper: Option<&[u8]>,
    start_after: Option<&[u8]>,
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
                .filter(|entry| start_after.is_none_or(|cursor| entry.key.as_slice() > cursor))
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
                let ends_after_cursor = start_after
                    .is_none_or(|cursor| child_upper.is_none_or(|bound| bound.as_slice() > cursor));
                let starts_before_upper =
                    upper.is_none_or(|bound| child_lower.is_none_or(|key| key.as_slice() < bound));
                if ends_after_prefix
                    && ends_after_cursor
                    && starts_before_upper
                    && visit_prefix_node_cached(
                        store,
                        pool,
                        child,
                        prefix,
                        upper,
                        start_after,
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
        ops::ControlFlow,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use hyphae_native_pages::{BufferPool, PageKind, PageStore};
    use hyphae_native_types::Csn;

    use super::{BTree, BTreeError, LeafEntry, Node, encode_leaf};

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
