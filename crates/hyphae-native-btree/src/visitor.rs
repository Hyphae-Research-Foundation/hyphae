// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeSet,
    ops::{Bound, ControlFlow},
};

use hyphae_native_pages::{PageKind, PageStore};
use hyphae_native_types::PageId;
use thiserror::Error;

use super::{
    BTree, BTreeError, BorrowedLeaf, Cursor, FORMAT_VERSION, INTERNAL_HEADER_SIZE, INTERNAL_MAGIC,
    MAX_TREE_HEIGHT, range_is_empty, read_u16, read_u64,
};

/// Hard entry and borrowed key/value-byte limits for one ordered visit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BorrowedVisitLimits {
    /// Maximum entries delivered to the callback.
    pub maximum_entries: usize,
    /// Maximum sum of delivered key and value bytes.
    pub maximum_bytes: u64,
}

/// Completed work observed by one borrowed visit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BorrowedVisitStats {
    /// Entries admitted and delivered to the callback.
    pub entries: usize,
    /// Sum of admitted key and value bytes.
    pub bytes: u64,
    /// Whether the range was exhausted rather than stopped by the callback.
    pub complete: bool,
}

/// Bounded borrowed-visitor failure.
#[derive(Debug, Error)]
pub enum BorrowedVisitError {
    /// B+tree page, codec, or structural validation failed.
    #[error(transparent)]
    Tree(#[from] BTreeError),
    /// The caller's cooperative control requested cancellation.
    #[error("native B+tree borrowed visit was cancelled")]
    Cancelled,
    /// The next matched entry exceeded the declared entry or byte bound.
    #[error("native B+tree borrowed visit exceeded its declared bound")]
    LimitExceeded,
}

impl BTree {
    /// Visits one bounded key range with borrowed key and value slices.
    ///
    /// Leaf values are decoded and admitted in place. The entry and byte
    /// limits are checked before `visitor` runs, so a rejected value is never
    /// copied by this path. Callback references are tied to the current page
    /// frame and cannot escape the callback invocation.
    ///
    /// # Errors
    ///
    /// Returns [`BorrowedVisitError::Tree`] for reached page or structural
    /// corruption, [`BorrowedVisitError::Cancelled`] when `control` breaks, or
    /// [`BorrowedVisitError::LimitExceeded`] before delivering an entry beyond
    /// `limits`.
    pub fn visit_range_borrowed_with_control<F, C>(
        self,
        store: &PageStore,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        limits: BorrowedVisitLimits,
        control: C,
        visitor: F,
    ) -> Result<BorrowedVisitStats, BorrowedVisitError>
    where
        F: for<'entry> FnMut(&'entry [u8], &'entry [u8]) -> ControlFlow<()>,
        C: FnMut() -> ControlFlow<()>,
    {
        if range_is_empty(lower, upper) {
            return Ok(BorrowedVisitStats {
                complete: true,
                ..BorrowedVisitStats::default()
            });
        }
        let Some(root) = self.root else {
            return Ok(BorrowedVisitStats {
                complete: true,
                ..BorrowedVisitStats::default()
            });
        };
        let mut traversal = BorrowedRangeVisitor {
            store,
            lower,
            upper,
            limits,
            control,
            visitor,
            visited: BTreeSet::new(),
            leaf_depth: None,
            stats: BorrowedVisitStats::default(),
        };
        let outcome = traversal.visit_node(root, 0, None, None)?;
        traversal.stats.complete = outcome.is_continue();
        Ok(traversal.stats)
    }
}

struct BorrowedRangeVisitor<'visit, F, C> {
    store: &'visit PageStore,
    lower: Bound<&'visit [u8]>,
    upper: Bound<&'visit [u8]>,
    limits: BorrowedVisitLimits,
    control: C,
    visitor: F,
    visited: BTreeSet<PageId>,
    leaf_depth: Option<usize>,
    stats: BorrowedVisitStats,
}

impl<F, C> BorrowedRangeVisitor<'_, F, C>
where
    F: for<'entry> FnMut(&'entry [u8], &'entry [u8]) -> ControlFlow<()>,
    C: FnMut() -> ControlFlow<()>,
{
    fn visit_node(
        &mut self,
        page_id: PageId,
        depth: usize,
        expected_minimum: Option<&[u8]>,
        expected_upper: Option<&[u8]>,
    ) -> Result<ControlFlow<()>, BorrowedVisitError> {
        if (self.control)().is_break() {
            return Err(BorrowedVisitError::Cancelled);
        }
        if depth >= MAX_TREE_HEIGHT {
            return Err(BTreeError::HeightExceeded.into());
        }
        if !self.visited.insert(page_id) {
            return Err(BTreeError::Cycle.into());
        }
        let page = self.store.read(page_id).map_err(BTreeError::from)?;
        match page.kind() {
            PageKind::BTreeLeaf => {
                let leaf = BorrowedLeaf::decode(page.payload())?;
                if expected_minimum.is_some_and(|minimum| leaf.minimum != minimum)
                    || expected_upper.is_some_and(|upper| leaf.maximum >= upper)
                {
                    return Err(BTreeError::InvalidSeparator.into());
                }
                if self
                    .leaf_depth
                    .replace(depth)
                    .is_some_and(|found| found != depth)
                {
                    return Err(BTreeError::Unbalanced.into());
                }
                for (key, value) in leaf.entries() {
                    if !key_satisfies_lower(key, self.lower)
                        || !key_satisfies_upper(key, self.upper)
                    {
                        continue;
                    }
                    if (self.control)().is_break() {
                        return Err(BorrowedVisitError::Cancelled);
                    }
                    let entries = self
                        .stats
                        .entries
                        .checked_add(1)
                        .ok_or(BorrowedVisitError::LimitExceeded)?;
                    let entry_bytes = u64::try_from(key.len().saturating_add(value.len()))
                        .map_err(|_| BorrowedVisitError::LimitExceeded)?;
                    let bytes = self
                        .stats
                        .bytes
                        .checked_add(entry_bytes)
                        .ok_or(BorrowedVisitError::LimitExceeded)?;
                    if entries > self.limits.maximum_entries || bytes > self.limits.maximum_bytes {
                        return Err(BorrowedVisitError::LimitExceeded);
                    }
                    self.stats.entries = entries;
                    self.stats.bytes = bytes;
                    if (self.visitor)(key, value).is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            }
            PageKind::BTreeInternal => {
                let internal = BorrowedInternal::decode(page.payload())?;
                internal.validate_envelope(expected_minimum, expected_upper)?;
                let mut entries = internal.entries();
                let first_separator = entries.next().transpose()?;
                let first_upper = tighter_upper(
                    expected_upper,
                    first_separator.as_ref().map(|(key, _)| *key),
                );
                if range_intersects(expected_minimum, first_upper, self.lower, self.upper)
                    && self
                        .visit_node(
                            internal.first_child,
                            depth + 1,
                            expected_minimum,
                            first_upper,
                        )?
                        .is_break()
                {
                    return Ok(ControlFlow::Break(()));
                }
                let mut current = first_separator;
                while let Some((minimum, child)) = current {
                    let next = entries.next().transpose()?;
                    let child_minimum = tighter_lower(expected_minimum, Some(minimum));
                    let child_upper =
                        tighter_upper(expected_upper, next.as_ref().map(|(key, _)| *key));
                    if range_intersects(child_minimum, child_upper, self.lower, self.upper)
                        && self
                            .visit_node(child, depth + 1, child_minimum, child_upper)?
                            .is_break()
                    {
                        return Ok(ControlFlow::Break(()));
                    }
                    current = next;
                }
                Ok(ControlFlow::Continue(()))
            }
            _ => Err(BTreeError::WrongPageKind.into()),
        }
    }
}

struct BorrowedInternal<'payload> {
    first_child: PageId,
    body: &'payload [u8],
    count: usize,
}

impl<'payload> BorrowedInternal<'payload> {
    fn decode(payload: &'payload [u8]) -> Result<Self, BTreeError> {
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
        let first_child =
            PageId::new(read_u64(&payload[16..24])).map_err(|_| BTreeError::ZeroChild)?;
        let body = &payload[INTERNAL_HEADER_SIZE..];
        let mut cursor = Cursor::new(body);
        let mut previous: Option<&[u8]> = None;
        for _ in 0..count {
            let key_length = cursor.length()?;
            let key = cursor.take(key_length)?;
            if key.len() > super::BTREE_MAX_KEY_SIZE {
                return Err(BTreeError::KeyTooLarge);
            }
            if previous.is_some_and(|previous| previous >= key) {
                return Err(BTreeError::NoncanonicalKeyOrder);
            }
            PageId::new(cursor.u64()?).map_err(|_| BTreeError::ZeroChild)?;
            previous = Some(key);
        }
        cursor.finish()?;
        Ok(Self {
            first_child,
            body,
            count,
        })
    }

    fn entries(
        &self,
    ) -> impl Iterator<Item = Result<(&'payload [u8], PageId), BTreeError>> + 'payload {
        let mut cursor = Cursor::new(self.body);
        (0..self.count).map(move |_| {
            let key_length = cursor.length()?;
            let key = cursor.take(key_length)?;
            let child = PageId::new(cursor.u64()?).map_err(|_| BTreeError::ZeroChild)?;
            Ok((key, child))
        })
    }

    fn validate_envelope(
        &self,
        inherited_lower: Option<&[u8]>,
        inherited_upper: Option<&[u8]>,
    ) -> Result<(), BTreeError> {
        for entry in self.entries() {
            let (separator, _) = entry?;
            if inherited_lower.is_some_and(|lower| separator <= lower)
                || inherited_upper.is_some_and(|upper| separator >= upper)
            {
                return Err(BTreeError::InvalidSeparator);
            }
        }
        Ok(())
    }
}

fn tighter_lower<'bound>(
    inherited: Option<&'bound [u8]>,
    child: Option<&'bound [u8]>,
) -> Option<&'bound [u8]> {
    match (inherited, child) {
        (Some(inherited), Some(child)) => Some(inherited.max(child)),
        (Some(inherited), None) => Some(inherited),
        (None, Some(child)) => Some(child),
        (None, None) => None,
    }
}

fn tighter_upper<'bound>(
    inherited: Option<&'bound [u8]>,
    child: Option<&'bound [u8]>,
) -> Option<&'bound [u8]> {
    match (inherited, child) {
        (Some(inherited), Some(child)) => Some(inherited.min(child)),
        (Some(inherited), None) => Some(inherited),
        (None, Some(child)) => Some(child),
        (None, None) => None,
    }
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

fn range_intersects(
    structural_lower: Option<&[u8]>,
    structural_upper: Option<&[u8]>,
    requested_lower: Bound<&[u8]>,
    requested_upper: Bound<&[u8]>,
) -> bool {
    let after_lower = match requested_lower {
        Bound::Included(lower) => structural_upper.is_none_or(|upper| upper > lower),
        Bound::Excluded(lower) => structural_upper.is_none_or(|upper| upper > lower),
        Bound::Unbounded => true,
    };
    let before_upper = match requested_upper {
        Bound::Included(upper) => structural_lower.is_none_or(|lower| lower <= upper),
        Bound::Excluded(upper) => structural_lower.is_none_or(|lower| lower < upper),
        Bound::Unbounded => true,
    };
    after_lower && before_upper
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        ops::{Bound, ControlFlow},
        sync::atomic::{AtomicU64, Ordering},
    };

    use hyphae_native_pages::{PageKind, PageStore};
    use hyphae_native_types::{Csn, PageId};

    use super::{BorrowedVisitError, BorrowedVisitLimits};
    use crate::{BTree, LeafEntry, Node, OWNED_LEAF_ENTRY_ALLOCATIONS};

    fn append_leaf(
        pages: &mut PageStore,
        keys: &[&[u8]],
    ) -> Result<PageId, Box<dyn std::error::Error>> {
        Ok(pages.append(
            PageKind::BTreeLeaf,
            Some(Csn::new(1)?),
            None,
            Node::Leaf(
                keys.iter()
                    .map(|key| LeafEntry {
                        key: key.to_vec(),
                        value: vec![1],
                    })
                    .collect(),
            )
            .encode()?,
        )?)
    }

    fn append_internal(
        pages: &mut PageStore,
        separator: &[u8],
        left: PageId,
        right: PageId,
    ) -> Result<PageId, Box<dyn std::error::Error>> {
        Ok(pages.append(
            PageKind::BTreeInternal,
            Some(Csn::new(1)?),
            None,
            Node::Internal {
                keys: vec![separator.to_vec()],
                children: vec![left, right],
            }
            .encode()?,
        )?)
    }

    #[test]
    fn borrowed_range_admits_before_callback_copy_and_never_decodes_owned_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "hyphae-borrowed-visitor-{}-{}.pages",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut pages = PageStore::create(&path)?;
        let large = vec![7; 7_000];
        let tree = BTree::empty()
            .upsert_sorted_batch(
                &mut pages,
                Csn::new(1)?,
                vec![
                    (b"a".to_vec(), large.clone()),
                    (b"b".to_vec(), large),
                    (b"c".to_vec(), vec![3]),
                ],
            )?
            .tree;

        OWNED_LEAF_ENTRY_ALLOCATIONS.set(0);
        let mut copied_values = 0_usize;
        let rejected = tree.visit_range_borrowed_with_control(
            &pages,
            Bound::Included(b"b"),
            Bound::Excluded(b"c"),
            BorrowedVisitLimits {
                maximum_entries: 1,
                maximum_bytes: 6_999,
            },
            || ControlFlow::Continue(()),
            |_, value| {
                copied_values = copied_values.saturating_add(value.to_vec().len());
                ControlFlow::Continue(())
            },
        );
        assert!(matches!(rejected, Err(BorrowedVisitError::LimitExceeded)));
        assert_eq!(copied_values, 0);
        assert_eq!(OWNED_LEAF_ENTRY_ALLOCATIONS.get(), 0);

        let mut keys = Vec::new();
        let visited = tree.visit_range_borrowed_with_control(
            &pages,
            Bound::Included(b"b"),
            Bound::Included(b"c"),
            BorrowedVisitLimits {
                maximum_entries: 2,
                maximum_bytes: 8_000,
            },
            || ControlFlow::Continue(()),
            |key, _| {
                keys.push(key.to_vec());
                ControlFlow::Continue(())
            },
        )?;
        assert_eq!(keys, [b"b".to_vec(), b"c".to_vec()]);
        assert_eq!(visited.entries, 2);
        assert!(visited.complete);
        assert_eq!(OWNED_LEAF_ENTRY_ALLOCATIONS.get(), 0);

        let mut controls = 0_usize;
        let cancelled = tree.visit_range_borrowed_with_control(
            &pages,
            Bound::Unbounded,
            Bound::Unbounded,
            BorrowedVisitLimits {
                maximum_entries: usize::MAX,
                maximum_bytes: u64::MAX,
            },
            || {
                controls = controls.saturating_add(1);
                if controls == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
            |_, _| ControlFlow::Continue(()),
        );
        assert!(matches!(cancelled, Err(BorrowedVisitError::Cancelled)));
        assert_eq!(OWNED_LEAF_ENTRY_ALLOCATIONS.get(), 0);
        drop(pages);
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn borrowed_range_rejects_multilevel_separators_outside_ancestor_envelopes_before_pruning()
    -> Result<(), Box<dyn std::error::Error>> {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "hyphae-borrowed-envelope-{}-{}.pages",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut pages = PageStore::create(&path)?;
        let limits = BorrowedVisitLimits {
            maximum_entries: usize::MAX,
            maximum_bytes: u64::MAX,
        };

        let low = append_leaf(&mut pages, &[b"a", b"b"])?;
        let above = append_leaf(&mut pages, &[b"z"])?;
        let forged_left = append_internal(&mut pages, b"z", low, above)?;
        let root_right = append_leaf(&mut pages, &[b"s", b"t"])?;
        let above_root = append_internal(&mut pages, b"s", forged_left, root_right)?;
        let mut callbacks = 0_usize;
        let above_result = BTree::from_root(above_root).visit_range_borrowed_with_control(
            &pages,
            Bound::Included(b"a"),
            Bound::Excluded(b"c"),
            limits,
            || ControlFlow::Continue(()),
            |_, _| {
                callbacks = callbacks.saturating_add(1);
                ControlFlow::Continue(())
            },
        );
        assert!(matches!(
            above_result,
            Err(BorrowedVisitError::Tree(
                crate::BTreeError::InvalidSeparator
            ))
        ));
        assert_eq!(callbacks, 0);

        let root_left = append_leaf(&mut pages, &[b"a"])?;
        let inherited_first = append_leaf(&mut pages, &[b"m", b"n"])?;
        let below = append_leaf(&mut pages, &[b"b", b"c"])?;
        let forged_right = append_internal(&mut pages, b"b", inherited_first, below)?;
        let below_root = append_internal(&mut pages, b"m", root_left, forged_right)?;
        let mut callbacks = 0_usize;
        let below_result = BTree::from_root(below_root).visit_range_borrowed_with_control(
            &pages,
            Bound::Included(b"m"),
            Bound::Included(b"m"),
            limits,
            || ControlFlow::Continue(()),
            |_, _| {
                callbacks = callbacks.saturating_add(1);
                ControlFlow::Continue(())
            },
        );
        assert!(matches!(
            below_result,
            Err(BorrowedVisitError::Tree(
                crate::BTreeError::InvalidSeparator
            ))
        ));
        assert_eq!(callbacks, 0);
        drop(pages);
        fs::remove_file(path)?;
        Ok(())
    }
}
