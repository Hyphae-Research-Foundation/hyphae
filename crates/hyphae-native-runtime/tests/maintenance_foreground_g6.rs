// SPDX-License-Identifier: GPL-3.0-only

//! Bounded foreground-interference acceptance for every G6 maintenance family.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use hyphae_native_runtime::{HnswConfig, NativeDatabase, Vector, VectorMetric};
use hyphae_native_types::{DurabilityClass, ObjectId};

type TestError = Box<dyn std::error::Error>;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(std::env::temp_dir().join(format!(
            "hyphae-maintenance-foreground-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn setup(path: &Path) -> Result<(NativeDatabase, ObjectId, ObjectId), TestError> {
    let lexical = ObjectId::new(100)?;
    let ann = ObjectId::new(101)?;
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(0, DurabilityClass::Strict)?;
    seed.set(b"expiring".to_vec(), b"old".to_vec(), Some(1))?;
    seed.set(b"large".to_vec(), vec![7; 10_000], None)?;
    seed.create_search_index(lexical, "documents")?;
    seed.index_document(lexical, b"document".to_vec(), "old searchable value")?;
    seed.create_vector_index(
        ann,
        "vectors",
        2,
        VectorMetric::SquaredL2,
        HnswConfig::new(4, 16, 8, 32, 7)?,
    )?;
    seed.upsert_vectors(
        ann,
        [
            (ObjectId::new(1)?, Vector::new([0.0, 0.0])?),
            (ObjectId::new(2)?, Vector::new([2.0, 0.0])?),
        ],
    )?;
    seed.commit()?;
    let mut tombstones = database.begin(2, DurabilityClass::Strict)?;
    tombstones.delete_structure(b"large".to_vec())?;
    tombstones.delete_document(lexical, b"document".to_vec())?;
    tombstones.upsert_vector(ann, ObjectId::new(2)?, Vector::new([3.0, 0.0])?)?;
    tombstones.commit()?;
    Ok((database, lexical, ann))
}

fn assert_old_or_new(snapshot: &hyphae_native_runtime::NativeSnapshot) {
    let state = (
        snapshot.get(b"foreground-a").map(<[u8]>::to_vec),
        snapshot.get(b"foreground-b").map(<[u8]>::to_vec),
    );
    assert!(
        state == (None, None) || state == (Some(b"new-a".to_vec()), Some(b"new-b".to_vec())),
        "reader observed mixed foreground state: {state:?}"
    );
}

#[test]
fn all_maintenance_families_preserve_concurrent_snapshots_and_foreground_progress()
-> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let (mut database, _lexical, ann) = setup(&temporary.0)?;

    let old = Arc::new(database.snapshot(2)?);
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let reader = {
        let old = Arc::clone(&old);
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                assert_old_or_new(&old);
                reads.fetch_add(1, Ordering::Relaxed);
                thread::yield_now();
            }
        })
    };

    let mut foreground = database.begin_optimistic(3, DurabilityClass::Strict)?;
    foreground.set(b"foreground-a".to_vec(), b"new-a".to_vec(), None)?;
    foreground.set(b"foreground-b".to_vec(), b"new-b".to_vec(), None)?;

    database.checkpoint()?;
    database.expire_due_structures(2, 1, DurabilityClass::Strict)?;
    database.compact_structure(DurabilityClass::Strict)?;
    database.compact_search(DurabilityClass::Strict)?;
    let plan = database.plan_ann_consolidation(ann, 16, 16)?;
    database.consolidate_ann(plan, DurabilityClass::Strict)?;
    database.commit_optimistic(foreground)?;
    assert_old_or_new(&database.snapshot(3)?);
    database.vacuum_pages()?;
    database.checkpoint()?;
    database.truncate_wal_at_retention_checkpoint()?;
    database.collect_blobs()?;
    database.collect_retired_page_generations()?;

    stop.store(true, Ordering::Release);
    reader.join().map_err(|_| "foreground reader panicked")?;
    assert!(reads.load(Ordering::Relaxed) > 0, "reader made no progress");
    assert_eq!(old.get(b"foreground-a"), None);
    assert_eq!(old.get(b"foreground-b"), None);
    let current = database.snapshot(3)?;
    assert_eq!(current.get(b"foreground-a"), Some(b"new-a".as_slice()));
    assert_eq!(current.get(b"foreground-b"), Some(b"new-b".as_slice()));

    drop(current);
    drop(old);
    drop(database);
    let reopened = NativeDatabase::open(&temporary.0)?;
    assert_old_or_new(&reopened.snapshot(3)?);
    Ok(())
}

#[test]
fn maintenance_progress_is_bounded_and_stale_ann_publication_cancels_without_mixed_state()
-> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let (mut database, _lexical, ann) = setup(&temporary.0)?;
    let stale = database.plan_ann_consolidation(ann, 16, 16)?;
    let fresh = database.plan_ann_consolidation(ann, 16, 16)?;
    database.consolidate_ann(fresh, DurabilityClass::Strict)?;
    let before = database.observe_ann_index(ann)?;
    assert!(matches!(
        database.consolidate_ann(stale, DurabilityClass::Strict),
        Err(hyphae_native_runtime::NativeRuntimeError::AnnConsolidationStale)
    ));
    assert_eq!(database.observe_ann_index(ann)?, before);

    for sequence in 0..8_u8 {
        let key = vec![b'e', sequence];
        let mut transaction = database.begin(3, DurabilityClass::Memory)?;
        transaction.set(key, vec![sequence], Some(4))?;
        transaction.commit()?;
    }
    let first = database.expire_due_structures(4, 3, DurabilityClass::Memory)?;
    assert_eq!(first.expired_keys, 3);
    assert!(first.more_due);
    let mut attempts = 1;
    let mut expired = first.expired_keys;
    while expired < 9 {
        let receipt = database.expire_due_structures(4, 3, DurabilityClass::Memory)?;
        assert!(receipt.expired_keys <= 3);
        expired += receipt.expired_keys;
        attempts += 1;
        assert!(attempts <= 3, "bounded expiry failed to make progress");
    }
    assert_eq!(expired, 9);
    Ok(())
}
