// SPDX-License-Identifier: GPL-3.0-only

//! Contract tests for native lexical document replacement and deletion.

use std::{
    fs,
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    CommitBoundary, LOCAL_TRANSACTION_SEARCH_DELETE_HEADER_SIZE,
    LOCAL_TRANSACTION_SEARCH_DOCUMENT_HEADER_SIZE, LocalSearchCodecError,
    LocalTransactionDeleteDocumentRequest, LocalTransactionReplaceDocumentRequest, NativeDatabase,
    NativeRuntimeError, decode_local_transaction_delete_document,
    decode_local_transaction_replace_document, encode_local_transaction_delete_document,
    encode_local_transaction_replace_document,
};
use hyphae_native_types::{DurabilityClass, ObjectId};

type TestError = Box<dyn std::error::Error>;

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hy-search-lifecycle-{}-{timestamp}-{sequence}",
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
fn local_lifecycle_codecs_have_stable_bytes_and_fail_closed() -> Result<(), TestError> {
    let mut buffer = Vec::new();
    let handle = NonZeroU64::new(1).ok_or("nonzero handle")?;
    let index = ObjectId::new(2)?;
    let replace = LocalTransactionReplaceDocumentRequest {
        handle,
        index,
        document_id: b"d",
        text: "x",
    };
    let encoded_replace =
        encode_local_transaction_replace_document(&mut buffer, replace, usize::MAX)?.to_vec();
    assert_eq!(
        encoded_replace,
        [
            1, 3, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, b'd', b'x',
        ]
    );
    assert_eq!(
        decode_local_transaction_replace_document(&encoded_replace)?,
        replace
    );
    for length in 0..encoded_replace.len() {
        assert!(matches!(
            decode_local_transaction_replace_document(&encoded_replace[..length]),
            Err(LocalSearchCodecError::Truncated | LocalSearchCodecError::LengthMismatch)
        ));
    }
    assert!(matches!(
        encode_local_transaction_replace_document(
            &mut buffer,
            replace,
            LOCAL_TRANSACTION_SEARCH_DOCUMENT_HEADER_SIZE + 1,
        ),
        Err(LocalSearchCodecError::PayloadTooLarge)
    ));

    let delete = LocalTransactionDeleteDocumentRequest {
        handle,
        index,
        document_id: b"d",
    };
    let encoded_delete =
        encode_local_transaction_delete_document(&mut buffer, delete, usize::MAX)?.to_vec();
    assert_eq!(
        encoded_delete,
        [
            1, 4, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            0, 0, 0, 0, 0, 0, 0, b'd',
        ]
    );
    assert_eq!(
        decode_local_transaction_delete_document(&encoded_delete)?,
        delete
    );
    for length in 0..encoded_delete.len() {
        assert!(matches!(
            decode_local_transaction_delete_document(&encoded_delete[..length]),
            Err(LocalSearchCodecError::Truncated | LocalSearchCodecError::LengthMismatch)
        ));
    }
    assert!(matches!(
        encode_local_transaction_delete_document(
            &mut buffer,
            delete,
            LOCAL_TRANSACTION_SEARCH_DELETE_HEADER_SIZE,
        ),
        Err(LocalSearchCodecError::PayloadTooLarge)
    ));
    let mut invalid = encoded_delete;
    invalid[32] = 1;
    assert!(matches!(
        decode_local_transaction_delete_document(&invalid),
        Err(LocalSearchCodecError::ReservedBytes)
    ));
    Ok(())
}

#[test]
fn embedded_lifecycle_preserves_history_and_reinsert_semantics() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;

    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc-a".to_vec(), "rust native alpha")?;
    seed.index_document(index, b"doc-b".to_vec(), "rust native beta")?;
    let seeded = seed.commit()?;
    let historical = database.snapshot(1)?;

    let mut lifecycle = database.begin(2, DurabilityClass::Strict)?;
    lifecycle.replace_document(index, b"doc-a".to_vec(), "hyphae gamma")?;
    lifecycle.delete_document(index, b"doc-b".to_vec())?;
    lifecycle.index_document(index, b"doc-b".to_vec(), "hyphae delta")?;
    let committed = lifecycle.commit()?;
    assert_eq!(committed.commit_csn.get(), seeded.commit_csn.get() + 1);

    assert_eq!(
        historical
            .match_text(index, "rust", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc-a".to_vec(), b"doc-b".to_vec()]
    );
    let latest = database.snapshot(2)?;
    assert_eq!(
        database.match_latest_text(index, "hyphae", 10)?,
        latest.match_text(index, "hyphae", 10)?
    );
    assert!(database.match_latest_text(index, "rust", 10)?.is_empty());
    drop(database);

    let reopened = NativeDatabase::open(temporary.path().join("data"))?;
    assert_eq!(
        reopened
            .match_latest_text(index, "hyphae", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc-a".to_vec(), b"doc-b".to_vec()]
    );
    Ok(())
}

#[test]
fn point_resolved_lifecycle_sequences_use_transaction_private_state() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;

    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"existing".to_vec(), "first")?;
    seed.commit()?;

    let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
    database.stage_delta_replace_document(
        &mut delta,
        index,
        b"existing".to_vec(),
        "second".to_owned(),
    )?;
    database.stage_delta_replace_document(
        &mut delta,
        index,
        b"existing".to_vec(),
        "third".to_owned(),
    )?;
    database.stage_delta_delete_document(&mut delta, index, b"existing".to_vec())?;
    database.stage_delta_index_document(
        &mut delta,
        index,
        b"existing".to_vec(),
        "fourth".to_owned(),
    )?;
    database.stage_delta_index_document(
        &mut delta,
        index,
        b"transient".to_vec(),
        "temporary".to_owned(),
    )?;
    database.stage_delta_delete_document(&mut delta, index, b"transient".to_vec())?;
    database.commit_optimistic(delta)?;

    assert_eq!(
        database
            .match_latest_text(index, "fourth", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"existing".to_vec()]
    );
    assert!(
        database
            .match_latest_text(index, "temporary", 10)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn lifecycle_conflicts_per_document_and_rebases_disjoint_documents() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path().join("data"))?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc-a".to_vec(), "alpha")?;
    seed.index_document(index, b"doc-b".to_vec(), "beta")?;
    seed.commit()?;

    let mut first = database.begin_optimistic(2, DurabilityClass::Memory)?;
    let mut conflicting = database.begin_optimistic(2, DurabilityClass::Memory)?;
    first.replace_document(index, b"doc-a".to_vec(), "first")?;
    conflicting.replace_document(index, b"doc-a".to_vec(), "conflict")?;
    database.commit_optimistic(first)?;
    assert!(matches!(
        database.commit_optimistic(conflicting),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut replace = database.begin_optimistic(3, DurabilityClass::Memory)?;
    let mut delete = database.begin_optimistic(3, DurabilityClass::Memory)?;
    replace.replace_document(index, b"doc-a".to_vec(), "current")?;
    delete.delete_document(index, b"doc-b".to_vec())?;
    database.commit_optimistic(replace)?;
    database.commit_optimistic(delete)?;

    assert_eq!(
        database
            .match_latest_text(index, "current", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc-a".to_vec()]
    );
    assert!(database.match_latest_text(index, "beta", 10)?.is_empty());
    Ok(())
}

#[test]
fn lifecycle_crash_boundaries_recover_prior_or_complete_projection() -> Result<(), TestError> {
    const BOUNDARIES: [CommitBoundary; 7] = [
        CommitBoundary::BlobStaged,
        CommitBoundary::BlobPromoted,
        CommitBoundary::PageAppended,
        CommitBoundary::PageSynchronized,
        CommitBoundary::WalAppended,
        CommitBoundary::WalSynchronized,
        CommitBoundary::RootPublished,
    ];
    for (operation, boundary) in ["replace", "delete"]
        .into_iter()
        .flat_map(|operation| BOUNDARIES.map(|boundary| (operation, boundary)))
    {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(format!("{operation}-{boundary:?}"));
        let index = ObjectId::new(100)?;
        let mut database = NativeDatabase::create(&data)?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_search_index(index, "documents")?;
        seed.index_document(index, b"doc".to_vec(), "old token")?;
        seed.commit()?;

        let mut lifecycle = database.begin(2, DurabilityClass::Strict)?;
        match operation {
            "replace" => {
                lifecycle.replace_document(index, b"doc".to_vec(), "new token")?;
            }
            "delete" => lifecycle.delete_document(index, b"doc".to_vec())?,
            _ => return Err("unknown lifecycle operation".into()),
        }
        assert!(matches!(
            lifecycle.commit_with_interruption(boundary),
            Err(NativeRuntimeError::InjectedCrash(actual)) if actual == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        let old = reopened.match_latest_text(index, "old", 10)?;
        let new = reopened.match_latest_text(index, "new", 10)?;
        match operation {
            "replace" => {
                assert!(
                    (old.len() == 1 && new.is_empty()) || (old.is_empty() && new.len() == 1),
                    "mixed replacement projection at {boundary:?}"
                );
            }
            "delete" => {
                assert!(new.is_empty());
                assert!(old.len() <= 1);
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

#[test]
fn large_document_replacement_and_deletion_reopen_without_resurrection() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let index = ObjectId::new(100)?;
    let old = format!("old {}", "a ".repeat(4_501));
    let new = format!("new {}", "b ".repeat(4_501));
    let mut database = NativeDatabase::create(&data)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"large".to_vec(), old)?;
    seed.commit()?;

    let mut replace = database.begin(2, DurabilityClass::Strict)?;
    replace.replace_document(index, b"large".to_vec(), new)?;
    replace.commit()?;
    drop(database);
    let mut reopened = NativeDatabase::open(&data)?;
    assert_eq!(
        reopened
            .match_latest_text(index, "new", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"large".to_vec()]
    );
    assert!(reopened.match_latest_text(index, "old", 10)?.is_empty());

    let mut delete = reopened.begin(3, DurabilityClass::Strict)?;
    delete.delete_document(index, b"large".to_vec())?;
    delete.commit()?;
    let vacuum = reopened.vacuum_pages()?;
    assert!(vacuum.applied);
    reopened.checkpoint()?;
    reopened.truncate_wal_at_retention_checkpoint()?;
    let collection = reopened.collect_blobs()?;
    assert_eq!(collection.live_files, 0);
    assert_eq!(collection.candidate_files, 2);
    assert_eq!(collection.removed_files, 2);
    drop(reopened);
    let reopened = NativeDatabase::open(&data)?;
    assert!(reopened.match_latest_text(index, "new", 10)?.is_empty());
    assert_eq!(reopened.recovery_report().blob_count, 0);
    Ok(())
}
