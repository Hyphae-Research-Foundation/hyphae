// SPDX-License-Identifier: GPL-3.0-only

//! Snapshot/version semantics and atomic cross-engine root publication.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, RwLock},
};

use hyphae_native_types::{CatalogVersion, Csn, EngineKind, Lsn, PageGeneration, PageId};
use thiserror::Error;

/// One logical write-conflict namespace entry.
///
/// The engine identifies the owning subsystem, `object` narrows the namespace
/// to one catalog object when applicable, and `key` is the engine-defined
/// canonical storage key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WriteKey {
    engine: EngineKind,
    object: Option<hyphae_native_types::ObjectId>,
    key: Vec<u8>,
}

impl WriteKey {
    /// Constructs one canonical conflict key.
    pub fn new(
        engine: EngineKind,
        object: Option<hyphae_native_types::ObjectId>,
        key: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            engine,
            object,
            key: key.into(),
        }
    }

    /// Returns the engine owning this key.
    pub const fn engine(&self) -> EngineKind {
        self.engine
    }

    /// Returns the optional catalog object namespace.
    pub const fn object(&self) -> Option<hyphae_native_types::ObjectId> {
        self.object
    }

    /// Returns the canonical engine key.
    pub fn key(&self) -> &[u8] {
        &self.key
    }
}

/// First-committer-wins validation failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error(
    "native write conflict: key was committed at CSN {latest_commit} after transaction snapshot {read_csn:?}"
)]
pub struct WriteConflict {
    /// Conflicting logical key.
    pub key: WriteKey,
    /// Latest commit that changed the key.
    pub latest_commit: Csn,
    /// Snapshot observed by the rejected transaction.
    pub read_csn: Option<Csn>,
}

/// In-memory latest-writer table reconstructed from committed WAL.
///
/// The table is an admission index, not durability authority. Every accepted
/// write must already be represented by a committed WAL transaction before it
/// is published here, and recovery rebuilds it from those transactions.
#[derive(Clone, Debug, Default)]
pub struct ConflictTable {
    latest: BTreeMap<WriteKey, Csn>,
}

impl ConflictTable {
    /// Validates a write set using first-committer-wins semantics.
    ///
    /// # Errors
    ///
    /// Returns the first canonical key whose latest committed writer is newer
    /// than the transaction's read snapshot.
    pub fn validate(&self, read_csn: Option<Csn>, keys: &[WriteKey]) -> Result<(), WriteConflict> {
        for key in keys {
            let Some(latest_commit) = self.latest.get(key).copied() else {
                continue;
            };
            if read_csn.is_none_or(|read| latest_commit > read) {
                return Err(WriteConflict {
                    key: key.clone(),
                    latest_commit,
                    read_csn,
                });
            }
        }
        Ok(())
    }

    /// Records a WAL-committed write set.
    ///
    /// Repeated keys within one transaction and recovery replays are
    /// idempotent. An older commit can never move a key's latest writer
    /// backwards.
    pub fn publish_committed(&mut self, commit_csn: Csn, keys: impl IntoIterator<Item = WriteKey>) {
        for key in keys {
            self.latest
                .entry(key)
                .and_modify(|latest| *latest = (*latest).max(commit_csn))
                .or_insert(commit_csn);
        }
    }

    /// Returns the latest committed writer for one key.
    pub fn latest_commit(&self, key: &WriteKey) -> Option<Csn> {
        self.latest.get(key).copied()
    }

    /// Returns the number of distinct conflict keys retained.
    pub fn len(&self) -> usize {
        self.latest.len()
    }

    /// Returns whether no committed conflict keys are retained.
    pub fn is_empty(&self) -> bool {
        self.latest.is_empty()
    }
}

/// MVCC coordinator or publication failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MvccError {
    /// A coordinator mutex or root lock was poisoned.
    #[error("native MVCC synchronization primitive is poisoned")]
    Poisoned,
    /// Commit sequence number space is exhausted.
    #[error("native MVCC commit sequence is exhausted")]
    CsnExhausted,
    /// The supplied WAL anchor does not describe a committed record.
    #[error("native MVCC WAL anchor digest is zero")]
    InvalidWalAnchor,
    /// A recovered root set is not anchored to a committed WAL record.
    #[error("native MVCC recovered root set is not committed")]
    UncommittedRootSet,
    /// Page generation or retention-floor publication is invalid.
    #[error("native MVCC page generation or retention floor is invalid")]
    InvalidStorageTransition,
    /// A group publication was requested without a staged root set.
    #[error("native MVCC group publication requires at least one staged commit")]
    EmptyGroup,
}

/// WAL identity required before a root set can be published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalAnchor {
    /// Commit-record LSN.
    pub lsn: Lsn,
    /// Digest of the complete WAL commit/block authority.
    pub digest: [u8; 32],
}

impl WalAnchor {
    /// Constructs a nonzero WAL anchor.
    ///
    /// # Errors
    ///
    /// Returns an error when the digest is all zero.
    pub fn new(lsn: Lsn, digest: [u8; 32]) -> Result<Self, MvccError> {
        if digest == [0; 32] {
            return Err(MvccError::InvalidWalAnchor);
        }
        Ok(Self { lsn, digest })
    }
}

/// One engine-partition root slot.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RootSlot {
    /// Owning native engine.
    pub engine: EngineKind,
    /// Engine-defined partition identity.
    pub partition: u16,
}

/// Immutable root set visible to one snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootSet {
    visible_csn: Option<Csn>,
    catalog_version: CatalogVersion,
    wal_anchor: Option<WalAnchor>,
    roots: BTreeMap<RootSlot, PageId>,
    blob_generation: u64,
    page_generation: PageGeneration,
    retention_floor_csn: Option<Csn>,
    digest: [u8; 32],
}

impl RootSet {
    /// Creates the empty pre-commit root set.
    pub fn genesis(catalog_version: CatalogVersion) -> Self {
        let mut root = Self {
            visible_csn: None,
            catalog_version,
            wal_anchor: None,
            roots: BTreeMap::new(),
            blob_generation: 0,
            page_generation: PageGeneration::FIRST,
            retention_floor_csn: None,
            digest: [0; 32],
        };
        root.digest = root.compute_digest();
        root
    }

    /// Constructs a recovered committed root set.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid WAL anchor.
    pub fn committed(
        visible_csn: Csn,
        catalog_version: CatalogVersion,
        wal_anchor: WalAnchor,
        roots: BTreeMap<RootSlot, PageId>,
        blob_generation: u64,
    ) -> Result<Self, MvccError> {
        Self::committed_with_storage(
            visible_csn,
            catalog_version,
            wal_anchor,
            roots,
            blob_generation,
            PageGeneration::FIRST,
            Csn::FIRST,
        )
    }

    /// Constructs a recovered committed root set with explicit page storage.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid WAL anchor or a retention floor newer
    /// than the visible commit.
    #[allow(clippy::too_many_arguments)]
    pub fn committed_with_storage(
        visible_csn: Csn,
        catalog_version: CatalogVersion,
        wal_anchor: WalAnchor,
        roots: BTreeMap<RootSlot, PageId>,
        blob_generation: u64,
        page_generation: PageGeneration,
        retention_floor_csn: Csn,
    ) -> Result<Self, MvccError> {
        if wal_anchor.digest == [0; 32] {
            return Err(MvccError::InvalidWalAnchor);
        }
        if retention_floor_csn > visible_csn {
            return Err(MvccError::InvalidStorageTransition);
        }
        let mut root = Self {
            visible_csn: Some(visible_csn),
            catalog_version,
            wal_anchor: Some(wal_anchor),
            roots,
            blob_generation,
            page_generation,
            retention_floor_csn: Some(retention_floor_csn),
            digest: [0; 32],
        };
        root.digest = root.compute_digest();
        Ok(root)
    }

    /// Returns the latest visible commit, or `None` before the first commit.
    pub const fn visible_csn(&self) -> Option<Csn> {
        self.visible_csn
    }

    /// Returns the immutable catalog version.
    pub const fn catalog_version(&self) -> CatalogVersion {
        self.catalog_version
    }

    /// Returns the WAL commit anchor.
    pub const fn wal_anchor(&self) -> Option<WalAnchor> {
        self.wal_anchor
    }

    /// Returns one physical engine root.
    pub fn root(&self, slot: RootSlot) -> Option<PageId> {
        self.roots.get(&slot).copied()
    }

    /// Iterates over physical engine roots in canonical slot order.
    pub fn iter_roots(&self) -> impl Iterator<Item = (RootSlot, PageId)> + '_ {
        self.roots.iter().map(|(slot, page)| (*slot, *page))
    }

    /// Returns the current blob-file generation.
    pub const fn blob_generation(&self) -> u64 {
        self.blob_generation
    }

    /// Returns the immutable page-file generation.
    pub const fn page_generation(&self) -> PageGeneration {
        self.page_generation
    }

    /// Returns the oldest physical root that remains reopenable.
    pub const fn retention_floor_csn(&self) -> Option<Csn> {
        self.retention_floor_csn
    }

    /// Returns the complete canonical root-set digest.
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    fn compute_digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"hyphae-native-root-set-v1");
        hasher.update(&self.visible_csn.map_or(0, Csn::get).to_le_bytes());
        hasher.update(&self.catalog_version.get().to_le_bytes());
        if let Some(anchor) = self.wal_anchor {
            hasher.update(&anchor.lsn.get().to_le_bytes());
            hasher.update(&anchor.digest);
        } else {
            hasher.update(&0_u64.to_le_bytes());
            hasher.update(&[0; 32]);
        }
        hasher.update(&self.blob_generation.to_le_bytes());
        for (slot, page) in &self.roots {
            hasher.update(&[slot.engine as u8]);
            hasher.update(&slot.partition.to_le_bytes());
            hasher.update(&page.get().to_le_bytes());
        }
        if self.page_generation != PageGeneration::FIRST
            || self
                .retention_floor_csn
                .is_some_and(|floor| floor != Csn::FIRST)
        {
            hasher.update(b"hyphae-native-page-storage-v1");
            hasher.update(&self.page_generation.get().to_le_bytes());
            hasher.update(&self.retention_floor_csn.map_or(0, Csn::get).to_le_bytes());
        }
        *hasher.finalize().as_bytes()
    }
}

/// Immutable read snapshot across all native engines.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Latest commit visible to this snapshot.
    pub visible_csn: Option<Csn>,
    /// Catalog version bound to this snapshot.
    pub catalog_version: CatalogVersion,
    /// Captured logical UTC microseconds.
    pub logical_time_micros: i64,
    roots: Arc<RootSet>,
}

impl Snapshot {
    /// Returns the immutable root set retained by this snapshot.
    pub fn roots(&self) -> &RootSet {
        &self.roots
    }

    /// Materializes snapshot metadata from one independently verified
    /// committed root set.
    ///
    /// This constructor is for durable historical authorities whose pages and
    /// WAL/manifest binding were validated outside the live coordinator.
    ///
    /// # Errors
    ///
    /// Returns an error when the root set is not committed.
    pub fn from_committed_root(
        roots: RootSet,
        logical_time_micros: i64,
    ) -> Result<Self, MvccError> {
        let visible_csn = roots.visible_csn.ok_or(MvccError::UncommittedRootSet)?;
        if roots.wal_anchor.is_none() {
            return Err(MvccError::UncommittedRootSet);
        }
        Ok(Self {
            visible_csn: Some(visible_csn),
            catalog_version: roots.catalog_version,
            logical_time_micros,
            roots: Arc::new(roots),
        })
    }
}

/// Half-open MVCC version interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionWindow {
    /// Commit that created this version.
    pub begin: Csn,
    /// First commit where the version is no longer visible.
    pub end: Option<Csn>,
}

impl VersionWindow {
    /// Returns whether this version is visible at one snapshot CSN.
    pub fn is_visible_at(self, visible_csn: Option<Csn>) -> bool {
        let Some(visible) = visible_csn else {
            return false;
        };
        self.begin <= visible && self.end.is_none_or(|end| visible < end)
    }
}

#[derive(Debug)]
struct WriterState {
    next_csn: u64,
}

/// Snapshot publisher and serialized initial commit coordinator.
#[derive(Debug)]
pub struct CommitCoordinator {
    current: RwLock<Arc<RootSet>>,
    writer: Mutex<WriterState>,
}

impl CommitCoordinator {
    /// Creates a coordinator from one genesis root set.
    pub fn new(catalog_version: CatalogVersion) -> Self {
        Self {
            current: RwLock::new(Arc::new(RootSet::genesis(catalog_version))),
            writer: Mutex::new(WriterState { next_csn: 1 }),
        }
    }

    /// Restores a coordinator from one WAL-verified committed root set.
    ///
    /// # Errors
    ///
    /// Returns an error if the supplied root set is not committed.
    pub fn restore(root: RootSet) -> Result<Self, MvccError> {
        let visible_csn = root.visible_csn.ok_or(MvccError::UncommittedRootSet)?;
        if root.wal_anchor.is_none() {
            return Err(MvccError::UncommittedRootSet);
        }
        Ok(Self {
            current: RwLock::new(Arc::new(root)),
            writer: Mutex::new(WriterState {
                next_csn: visible_csn.get().checked_add(1).unwrap_or(0),
            }),
        })
    }

    /// Acquires one immutable cross-engine read snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if a prior panic poisoned the publication lock.
    pub fn snapshot(&self, logical_time_micros: i64) -> Result<Snapshot, MvccError> {
        let current = self.current.read().map_err(|_| MvccError::Poisoned)?;
        let roots = Arc::clone(&current);
        Ok(Snapshot {
            visible_csn: roots.visible_csn(),
            catalog_version: roots.catalog_version(),
            logical_time_micros,
            roots,
        })
    }

    /// Begins one serialized copy-on-write root transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if coordinator state is poisoned or CSN space is
    /// exhausted.
    pub fn begin_write(&self) -> Result<RootTransaction<'_>, MvccError> {
        let writer = self.writer.lock().map_err(|_| MvccError::Poisoned)?;
        if writer.next_csn == 0 {
            return Err(MvccError::CsnExhausted);
        }
        let current = self.current.read().map_err(|_| MvccError::Poisoned)?;
        let base = Arc::clone(&current);
        let roots = base.roots.clone();
        Ok(RootTransaction {
            coordinator: self,
            writer,
            base,
            roots,
            blob_generation: None,
            page_generation: None,
            retention_floor_csn: None,
        })
    }

    /// Begins one serialized group of privately staged root transactions.
    ///
    /// Every staged commit receives its own CSN and WAL anchor. No root becomes
    /// visible until [`RootGroupTransaction::publish`] installs the final root
    /// after the caller's shared durability synchronization.
    ///
    /// # Errors
    ///
    /// Returns an error if coordinator state is poisoned or CSN space is
    /// exhausted.
    pub fn begin_group_write(&self) -> Result<RootGroupTransaction<'_>, MvccError> {
        let writer = self.writer.lock().map_err(|_| MvccError::Poisoned)?;
        if writer.next_csn == 0 {
            return Err(MvccError::CsnExhausted);
        }
        let current = self.current.read().map_err(|_| MvccError::Poisoned)?;
        Ok(RootGroupTransaction {
            coordinator: self,
            writer,
            initial: Arc::clone(&current),
            staged: Vec::new(),
        })
    }
}

/// Private cross-engine root write set.
#[derive(Debug)]
pub struct RootTransaction<'coordinator> {
    coordinator: &'coordinator CommitCoordinator,
    writer: MutexGuard<'coordinator, WriterState>,
    base: Arc<RootSet>,
    roots: BTreeMap<RootSlot, PageId>,
    blob_generation: Option<u64>,
    page_generation: Option<PageGeneration>,
    retention_floor_csn: Option<Csn>,
}

impl RootTransaction<'_> {
    /// Returns the pre-commit snapshot CSN.
    pub fn read_csn(&self) -> Option<Csn> {
        self.base.visible_csn()
    }

    /// Returns the immutable root set current when writer admission succeeded.
    ///
    /// Detached optimistic transactions use this base to reapply disjoint
    /// writes after admission instead of publishing roots derived from a stale
    /// snapshot.
    pub fn base_roots(&self) -> &RootSet {
        &self.base
    }

    /// Captures the admitted base as a snapshot with caller-supplied logical
    /// time.
    pub fn base_snapshot(&self, logical_time_micros: i64) -> Snapshot {
        Snapshot {
            visible_csn: self.base.visible_csn(),
            catalog_version: self.base.catalog_version(),
            logical_time_micros,
            roots: Arc::clone(&self.base),
        }
    }

    /// Returns the CSN reserved for this serialized write attempt.
    ///
    /// No reader can observe this sequence until [`Self::commit`] publishes
    /// the complete root set.
    ///
    /// # Errors
    ///
    /// Returns an error when sequence space is exhausted.
    pub fn commit_csn(&self) -> Result<Csn, MvccError> {
        Csn::new(self.writer.next_csn).map_err(|_| MvccError::CsnExhausted)
    }

    /// Stages one engine-partition root replacement.
    pub fn set_root(&mut self, slot: RootSlot, page: PageId) {
        self.roots.insert(slot, page);
    }

    /// Stages a blob-generation replacement.
    pub fn set_blob_generation(&mut self, generation: u64) {
        self.blob_generation = Some(generation);
    }

    /// Stages one page-file generation replacement.
    pub fn set_page_generation(&mut self, generation: PageGeneration) {
        self.page_generation = Some(generation);
    }

    /// Stages the oldest physical commit retained after publication.
    pub fn set_retention_floor(&mut self, floor: Csn) {
        self.retention_floor_csn = Some(floor);
    }

    /// Publishes every staged root under one commit sequence.
    ///
    /// The caller must supply an already durable WAL anchor according to the
    /// transaction's selected durability class.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid WAL anchor, poisoned root publication,
    /// or exhausted CSN space.
    pub fn commit(
        mut self,
        catalog_version: CatalogVersion,
        wal_anchor: WalAnchor,
    ) -> Result<Arc<RootSet>, MvccError> {
        if wal_anchor.digest == [0; 32] {
            return Err(MvccError::InvalidWalAnchor);
        }
        let commit_csn = Csn::new(self.writer.next_csn).map_err(|_| MvccError::CsnExhausted)?;
        let next_csn = self
            .writer
            .next_csn
            .checked_add(1)
            .ok_or(MvccError::CsnExhausted)?;
        let page_generation = self.page_generation.unwrap_or(self.base.page_generation());
        let base_floor = self.base.retention_floor_csn();
        let retention_floor_csn = self
            .retention_floor_csn
            .or(base_floor)
            .unwrap_or(commit_csn);
        let generation_changed = page_generation != self.base.page_generation();
        let valid_transition = if self.base.visible_csn().is_none() {
            page_generation == PageGeneration::FIRST && retention_floor_csn == commit_csn
        } else if generation_changed {
            self.base.page_generation().checked_next() == Some(page_generation)
                && retention_floor_csn == commit_csn
        } else {
            base_floor == Some(retention_floor_csn)
        };
        if !valid_transition {
            return Err(MvccError::InvalidStorageTransition);
        }
        let root = RootSet::committed_with_storage(
            commit_csn,
            catalog_version,
            wal_anchor,
            self.roots,
            self.blob_generation.unwrap_or(self.base.blob_generation()),
            page_generation,
            retention_floor_csn,
        )?;
        let published = Arc::new(root);
        *self
            .coordinator
            .current
            .write()
            .map_err(|_| MvccError::Poisoned)? = Arc::clone(&published);
        self.writer.next_csn = next_csn;
        Ok(published)
    }
}

/// Serialized private root chain for one durability cohort.
///
/// Staging advances only the chain owned by this value. Dropping it before
/// publication leaves the coordinator's visible root and next CSN unchanged.
#[derive(Debug)]
pub struct RootGroupTransaction<'coordinator> {
    coordinator: &'coordinator CommitCoordinator,
    writer: MutexGuard<'coordinator, WriterState>,
    initial: Arc<RootSet>,
    staged: Vec<Arc<RootSet>>,
}

impl RootGroupTransaction<'_> {
    /// Returns the latest privately staged root, or the admitted visible root.
    pub fn base_roots(&self) -> &RootSet {
        self.staged.last().map_or(&self.initial, AsRef::as_ref)
    }

    /// Captures the latest private group root as a transaction snapshot.
    pub fn base_snapshot(&self, logical_time_micros: i64) -> Snapshot {
        let roots = self
            .staged
            .last()
            .map_or_else(|| Arc::clone(&self.initial), Arc::clone);
        Snapshot {
            visible_csn: roots.visible_csn(),
            catalog_version: roots.catalog_version(),
            logical_time_micros,
            roots,
        }
    }

    /// Returns the CSN for a future accepted offset in this cohort.
    ///
    /// # Errors
    ///
    /// Returns an error when the CSN domain cannot represent the offset.
    pub fn commit_csn_at_offset(&self, offset: usize) -> Result<Csn, MvccError> {
        let offset = u64::try_from(offset).map_err(|_| MvccError::CsnExhausted)?;
        let sequence = self
            .writer
            .next_csn
            .checked_add(offset)
            .ok_or(MvccError::CsnExhausted)?;
        Csn::new(sequence).map_err(|_| MvccError::CsnExhausted)
    }

    /// Returns the CSN assigned to the next privately staged commit.
    ///
    /// # Errors
    ///
    /// Returns an error when sequence space is exhausted.
    pub fn next_commit_csn(&self) -> Result<Csn, MvccError> {
        self.commit_csn_at_offset(self.staged.len())
    }

    /// Adds one WAL-anchored root to the private cohort chain.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid storage state, WAL authority, or exhausted
    /// sequence space.
    pub fn stage_commit(
        &mut self,
        catalog_version: CatalogVersion,
        wal_anchor: WalAnchor,
        roots: BTreeMap<RootSlot, PageId>,
        blob_generation: u64,
    ) -> Result<Arc<RootSet>, MvccError> {
        let base = self.base_roots();
        let commit_csn = self.next_commit_csn()?;
        let retention_floor_csn = base.retention_floor_csn().unwrap_or(commit_csn);
        let root = RootSet::committed_with_storage(
            commit_csn,
            catalog_version,
            wal_anchor,
            roots,
            blob_generation,
            base.page_generation(),
            retention_floor_csn,
        )?;
        let staged = Arc::new(root);
        self.staged.push(Arc::clone(&staged));
        Ok(staged)
    }

    /// Atomically installs the final staged root and consumes the cohort.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty cohort, poisoned publication, or exhausted
    /// CSN space.
    pub fn publish(mut self) -> Result<Arc<RootSet>, MvccError> {
        let published = self.staged.pop().ok_or(MvccError::EmptyGroup)?;
        let next_csn = published
            .visible_csn()
            .ok_or(MvccError::UncommittedRootSet)?
            .get()
            .checked_add(1)
            .ok_or(MvccError::CsnExhausted)?;
        *self
            .coordinator
            .current
            .write()
            .map_err(|_| MvccError::Poisoned)? = Arc::clone(&published);
        self.writer.next_csn = next_csn;
        Ok(published)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hyphae_native_types::{
        CatalogVersion, Csn, EngineKind, Lsn, ObjectId, PageGeneration, PageId,
    };

    use super::{
        CommitCoordinator, ConflictTable, MvccError, RootSlot, VersionWindow, WalAnchor, WriteKey,
    };

    fn wal_anchor(lsn: u64, marker: u8) -> Result<WalAnchor, Box<dyn std::error::Error>> {
        WalAnchor::new(Lsn::new(lsn)?, [marker; 32]).map_err(Into::into)
    }

    #[test]
    fn historical_snapshot_retains_its_root_set() -> Result<(), Box<dyn std::error::Error>> {
        let coordinator = CommitCoordinator::new(CatalogVersion::new(1)?);
        let before = coordinator.snapshot(10)?;
        let mut transaction = coordinator.begin_write()?;
        transaction.set_root(
            RootSlot {
                engine: EngineKind::Relational,
                partition: 0,
            },
            PageId::new(1)?,
        );
        transaction.commit(CatalogVersion::new(1)?, wal_anchor(112, 1)?)?;
        let after = coordinator.snapshot(11)?;
        assert_eq!(before.visible_csn, None);
        assert_eq!(after.visible_csn.map(Csn::get), Some(1));
        assert_ne!(before.roots().digest(), after.roots().digest());
        Ok(())
    }

    #[test]
    fn one_publication_contains_all_engine_roots() -> Result<(), Box<dyn std::error::Error>> {
        let coordinator = CommitCoordinator::new(CatalogVersion::new(1)?);
        let mut transaction = coordinator.begin_write()?;
        let slots = [
            RootSlot {
                engine: EngineKind::Relational,
                partition: 0,
            },
            RootSlot {
                engine: EngineKind::Structure,
                partition: 0,
            },
            RootSlot {
                engine: EngineKind::Search,
                partition: 0,
            },
        ];
        for (index, slot) in slots.into_iter().enumerate() {
            transaction.set_root(slot, PageId::new(u64::try_from(index + 1)?)?);
        }
        let published = transaction.commit(CatalogVersion::new(1)?, wal_anchor(112, 2)?)?;
        for slot in slots {
            assert!(published.root(slot).is_some());
        }
        assert_eq!(published.visible_csn().map(Csn::get), Some(1));
        Ok(())
    }

    #[test]
    fn page_generation_transition_advances_retention_floor_with_one_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let coordinator = CommitCoordinator::new(CatalogVersion::new(1)?);
        let mut initial = coordinator.begin_write()?;
        initial.set_root(
            RootSlot {
                engine: EngineKind::Relational,
                partition: 0,
            },
            PageId::new(7)?,
        );
        initial.commit(CatalogVersion::new(1)?, wal_anchor(112, 4)?)?;

        let mut vacuum = coordinator.begin_write()?;
        let vacuum_csn = vacuum.commit_csn()?;
        vacuum.set_root(
            RootSlot {
                engine: EngineKind::Relational,
                partition: 0,
            },
            PageId::new(1)?,
        );
        vacuum.set_page_generation(PageGeneration::new(2)?);
        vacuum.set_retention_floor(vacuum_csn);
        let published = vacuum.commit(CatalogVersion::new(1)?, wal_anchor(224, 5)?)?;

        assert_eq!(published.visible_csn(), Some(vacuum_csn));
        assert_eq!(published.page_generation().get(), 2);
        assert_eq!(published.retention_floor_csn(), Some(vacuum_csn));
        Ok(())
    }

    #[test]
    fn page_generation_transition_rejects_skips_and_floor_only_changes()
    -> Result<(), Box<dyn std::error::Error>> {
        fn committed_coordinator() -> Result<CommitCoordinator, Box<dyn std::error::Error>> {
            let coordinator = CommitCoordinator::new(CatalogVersion::new(1)?);
            let mut initial = coordinator.begin_write()?;
            initial.set_root(
                RootSlot {
                    engine: EngineKind::Relational,
                    partition: 0,
                },
                PageId::new(7)?,
            );
            initial.commit(CatalogVersion::new(1)?, wal_anchor(112, 6)?)?;
            Ok(coordinator)
        }

        let skipped = committed_coordinator()?;
        let mut transaction = skipped.begin_write()?;
        let commit_csn = transaction.commit_csn()?;
        transaction.set_page_generation(PageGeneration::new(3)?);
        transaction.set_retention_floor(commit_csn);
        assert_eq!(
            transaction.commit(CatalogVersion::new(1)?, wal_anchor(224, 7)?),
            Err(MvccError::InvalidStorageTransition)
        );

        let floor_only = committed_coordinator()?;
        let mut transaction = floor_only.begin_write()?;
        transaction.set_retention_floor(transaction.commit_csn()?);
        assert_eq!(
            transaction.commit(CatalogVersion::new(1)?, wal_anchor(224, 8)?),
            Err(MvccError::InvalidStorageTransition)
        );
        Ok(())
    }

    #[test]
    fn dropped_write_transaction_never_publishes() -> Result<(), Box<dyn std::error::Error>> {
        let coordinator = CommitCoordinator::new(CatalogVersion::new(1)?);
        {
            let mut transaction = coordinator.begin_write()?;
            transaction.set_root(
                RootSlot {
                    engine: EngineKind::Structure,
                    partition: 0,
                },
                PageId::new(1)?,
            );
        }
        assert_eq!(coordinator.snapshot(0)?.visible_csn, None);
        Ok(())
    }

    #[test]
    fn restored_coordinator_continues_after_recovered_csn() -> Result<(), Box<dyn std::error::Error>>
    {
        let coordinator = CommitCoordinator::new(CatalogVersion::new(1)?);
        let transaction = coordinator.begin_write()?;
        assert_eq!(transaction.commit_csn()?.get(), 1);
        let first = transaction.commit(CatalogVersion::new(1)?, wal_anchor(112, 3)?)?;
        let restored = CommitCoordinator::restore((*first).clone())?;
        let second = restored.begin_write()?;
        assert_eq!(second.read_csn().map(Csn::get), Some(1));
        assert_eq!(second.commit_csn()?.get(), 2);
        Ok(())
    }

    #[test]
    fn group_roots_remain_private_until_final_publication() -> Result<(), Box<dyn std::error::Error>>
    {
        let coordinator = CommitCoordinator::new(CatalogVersion::new(1)?);
        let slot = RootSlot {
            engine: EngineKind::Structure,
            partition: 0,
        };
        let mut group = coordinator.begin_group_write()?;
        assert_eq!(group.commit_csn_at_offset(0)?, Csn::new(1)?);
        assert_eq!(group.commit_csn_at_offset(1)?, Csn::new(2)?);

        group.stage_commit(
            CatalogVersion::new(1)?,
            wal_anchor(112, 9)?,
            BTreeMap::from([(slot, PageId::new(1)?)]),
            0,
        )?;
        group.stage_commit(
            CatalogVersion::new(1)?,
            wal_anchor(224, 10)?,
            BTreeMap::from([(slot, PageId::new(2)?)]),
            0,
        )?;
        assert_eq!(group.base_roots().visible_csn(), Some(Csn::new(2)?));
        assert_eq!(group.base_roots().root(slot), Some(PageId::new(2)?));
        assert_eq!(coordinator.snapshot(0)?.visible_csn, None);

        let published = group.publish()?;
        assert_eq!(published.visible_csn(), Some(Csn::new(2)?));
        assert_eq!(published.root(slot), Some(PageId::new(2)?));
        assert_eq!(coordinator.snapshot(0)?.visible_csn, Some(Csn::new(2)?));
        assert_eq!(coordinator.begin_write()?.commit_csn()?, Csn::new(3)?);
        Ok(())
    }

    #[test]
    fn dropped_group_does_not_consume_sequence_space() -> Result<(), Box<dyn std::error::Error>> {
        let coordinator = CommitCoordinator::new(CatalogVersion::new(1)?);
        {
            let mut group = coordinator.begin_group_write()?;
            group.stage_commit(
                CatalogVersion::new(1)?,
                wal_anchor(112, 11)?,
                BTreeMap::from([(
                    RootSlot {
                        engine: EngineKind::Search,
                        partition: 0,
                    },
                    PageId::new(1)?,
                )]),
                0,
            )?;
        }
        assert_eq!(coordinator.snapshot(0)?.visible_csn, None);
        assert_eq!(coordinator.begin_write()?.commit_csn()?, Csn::new(1)?);
        Ok(())
    }

    #[test]
    fn version_windows_are_half_open() -> Result<(), Box<dyn std::error::Error>> {
        let window = VersionWindow {
            begin: Csn::new(2)?,
            end: Some(Csn::new(4)?),
        };
        assert!(!window.is_visible_at(Some(Csn::new(1)?)));
        assert!(window.is_visible_at(Some(Csn::new(2)?)));
        assert!(window.is_visible_at(Some(Csn::new(3)?)));
        assert!(!window.is_visible_at(Some(Csn::new(4)?)));
        assert!(!window.is_visible_at(None));
        Ok(())
    }

    #[test]
    fn conflict_table_rejects_stale_same_key_and_accepts_disjoint_keys()
    -> Result<(), Box<dyn std::error::Error>> {
        let row = WriteKey::new(
            EngineKind::Relational,
            Some(ObjectId::new(7)?),
            b"account-1",
        );
        let other_row = WriteKey::new(
            EngineKind::Relational,
            Some(ObjectId::new(7)?),
            b"account-2",
        );
        let mut table = ConflictTable::default();
        table.publish_committed(Csn::new(2)?, [row.clone()]);

        let conflict = match table.validate(Some(Csn::new(1)?), std::slice::from_ref(&row)) {
            Ok(()) => return Err("stale writer was accepted".into()),
            Err(conflict) => conflict,
        };
        assert_eq!(conflict.key, row);
        assert_eq!(conflict.latest_commit.get(), 2);
        assert_eq!(conflict.read_csn.map(Csn::get), Some(1));
        table.validate(Some(Csn::new(1)?), &[other_row])?;
        table.validate(Some(Csn::new(2)?), std::slice::from_ref(&row))?;
        Ok(())
    }

    #[test]
    fn conflict_table_replay_is_monotonic_and_idempotent() -> Result<(), Box<dyn std::error::Error>>
    {
        let key = WriteKey::new(EngineKind::Structure, None, b"session");
        let mut table = ConflictTable::default();
        table.publish_committed(Csn::new(3)?, [key.clone(), key.clone()]);
        table.publish_committed(Csn::new(2)?, [key.clone()]);

        assert_eq!(table.len(), 1);
        assert_eq!(table.latest_commit(&key).map(Csn::get), Some(3));
        assert!(table.validate(Some(Csn::new(2)?), &[key]).is_err());
        Ok(())
    }
}
