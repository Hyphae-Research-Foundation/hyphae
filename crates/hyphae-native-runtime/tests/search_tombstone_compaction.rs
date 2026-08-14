// SPDX-License-Identifier: AGPL-3.0-only

//! Contract tests for native lexical tombstone compaction.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{NativeDatabase, SearchCompactionReceipt};
use hyphae_native_types::{DurabilityClass, ObjectId};

type TestError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hy-search-compaction-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn compaction_api_reports_a_true_noop() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let receipt: SearchCompactionReceipt = database.compact_search(DurabilityClass::Memory)?;
    assert_eq!(receipt.scanned_entries, 0);
    assert_eq!(receipt.retained_entries, 0);
    assert_eq!(receipt.dropped_tombstones, 0);
    assert_eq!(receipt.pages_appended, 0);
    assert!(receipt.commit.is_none());
    Ok(())
}

#[test]
fn compaction_drops_only_tombstones_and_preserves_history_and_scores() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(&data)?;

    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc-a".to_vec(), "alpha beta")?;
    seed.index_document(index, b"doc-b".to_vec(), "beta gamma")?;
    let seeded = seed.commit()?;
    let historical = database.snapshot(1)?;

    let mut lifecycle = database.begin(2, DurabilityClass::Strict)?;
    lifecycle.replace_document(index, b"doc-a".to_vec(), "delta beta")?;
    lifecycle.delete_document(index, b"doc-b".to_vec())?;
    let lifecycle_commit = lifecycle.commit()?;
    assert_eq!(
        lifecycle_commit.commit_csn.get(),
        seeded.commit_csn.get() + 1
    );
    let before_delta = database.match_latest_text(index, "delta", 10)?;
    let before_beta = database.match_latest_text(index, "beta", 10)?;

    let compacted = database.compact_search(DurabilityClass::Strict)?;
    assert_eq!(compacted.scanned_entries, 13);
    assert_eq!(compacted.retained_entries, 7);
    assert_eq!(compacted.dropped_tombstones, 6);
    assert!(compacted.reachable_pages_before >= compacted.reachable_pages_after);
    assert!(compacted.pages_appended > 0);
    let compaction_commit = compacted.commit.ok_or("missing compaction commit")?;
    assert_eq!(
        compaction_commit.commit_csn.get(),
        lifecycle_commit.commit_csn.get() + 1
    );
    assert_eq!(
        historical
            .match_text(index, "alpha", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc-a".to_vec()]
    );
    assert_eq!(
        historical
            .match_text(index, "gamma", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc-b".to_vec()]
    );
    assert_eq!(
        database.match_latest_text(index, "delta", 10)?,
        before_delta
    );
    assert_eq!(database.match_latest_text(index, "beta", 10)?, before_beta);
    assert!(database.match_latest_text(index, "alpha", 10)?.is_empty());
    assert!(database.match_latest_text(index, "gamma", 10)?.is_empty());

    let no_op = database.compact_search(DurabilityClass::Strict)?;
    assert_eq!(no_op.scanned_entries, 7);
    assert_eq!(no_op.retained_entries, 7);
    assert_eq!(no_op.dropped_tombstones, 0);
    assert_eq!(no_op.reachable_pages_before, no_op.reachable_pages_after);
    assert_eq!(no_op.pages_appended, 0);
    assert!(no_op.commit.is_none());

    let mut after_noop = database.begin(3, DurabilityClass::Memory)?;
    after_noop.set(b"proof".to_vec(), b"next".to_vec(), None)?;
    assert_eq!(
        after_noop.commit()?.commit_csn.get(),
        compaction_commit.commit_csn.get() + 1
    );
    drop(database);
    let reopened = NativeDatabase::open(&data)?;
    assert_eq!(
        reopened.match_latest_text(index, "delta", 10)?,
        before_delta
    );
    assert_eq!(reopened.match_latest_text(index, "beta", 10)?, before_beta);
    Ok(())
}
