// SPDX-License-Identifier: GPL-3.0-only

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_btree::BTree;
use hyphae_native_mvcc::{CommitCoordinator, RootSet};
use hyphae_native_types::{DurabilityClass, ObjectId, PageId};

use crate::{
    AnnSearchOptions, CommitBoundary, HnswConfig, NativeDatabase, NativeRuntimeError,
    SEARCH_DOCUMENT_TOMBSTONE, SEARCH_FORMAT_KEY, SEARCH_FORMAT_VALUE_V1,
    SEARCH_INLINE_VALUE_LIMIT, SEARCH_POSTING_PREFIX, SLOT_SEARCH, SearchFormat, Vector,
    VectorMetric, WAL_FILE, ann_store, plan_search_compaction, search_document_key,
    search_posting_key,
};

type TestError = Box<dyn Error>;
type PhysicalEntries = Vec<(Vec<u8>, Vec<u8>)>;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-search-compaction-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        );
        Self(std::env::temp_dir().join(unique))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn search_root(database: &NativeDatabase) -> Result<PageId, NativeRuntimeError> {
    database
        .coordinator
        .snapshot(0)?
        .roots()
        .root(SLOT_SEARCH)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)
}

fn search_entries(database: &NativeDatabase) -> Result<PhysicalEntries, NativeRuntimeError> {
    Ok(BTree::from_root(search_root(database)?).scan(&database.pages)?)
}

fn wal_bytes(database: &NativeDatabase) -> Result<u64, std::io::Error> {
    Ok(fs::metadata(database.data_directory.join(WAL_FILE))?.len())
}

fn ann_config() -> Result<HnswConfig, NativeRuntimeError> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x4859_5048_4145)?)
}

fn ann_options() -> Result<AnnSearchOptions, NativeRuntimeError> {
    Ok(AnnSearchOptions::new(2, 8, Some(4))?)
}

#[test]
fn compaction_preserves_ann_bytes_history_and_stale_writer_revalidation() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let lexical = ObjectId::new(100)?;
    let vectors = ObjectId::new(200)?;
    let first_vector = ObjectId::new(201)?;
    let second_vector = ObjectId::new(202)?;
    let query = Vector::new([1.0, 0.0, 0.0])?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(lexical, "documents")?;
    seed.index_document(lexical, b"doc-a".to_vec(), "alpha beta")?;
    seed.index_document(lexical, b"doc-b".to_vec(), "beta gamma")?;
    seed.create_vector_index(vectors, "vectors", 3, VectorMetric::Cosine, ann_config()?)?;
    seed.upsert_vectors(
        vectors,
        [
            (first_vector, Vector::new([1.0, 0.0, 0.0])?),
            (second_vector, Vector::new([0.0, 1.0, 0.0])?),
        ],
    )?;
    seed.commit()?;
    let historical = database.snapshot(1)?;
    let mut lifecycle = database.begin(2, DurabilityClass::Strict)?;
    lifecycle.replace_document(lexical, b"doc-a".to_vec(), "delta beta")?;
    lifecycle.delete_document(lexical, b"doc-b".to_vec())?;
    lifecycle.commit()?;

    let root = search_root(&database)?;
    let plan = plan_search_compaction(&database.pages, &database.blobs, root)?;
    let before_entries = search_entries(&database)?;
    let before_ann_entries = before_entries
        .iter()
        .filter(|(key, _)| ann_store::is_ann_physical_key(key))
        .cloned()
        .collect::<Vec<_>>();
    let before_ann = database.search_ann_latest(vectors, &query, ann_options()?)?;
    let before_exact = database.search_vector_exact_latest(vectors, &query, 2)?;
    let mut stale = database.begin_optimistic_delta(3, DurabilityClass::Memory)?;
    database.stage_delta_replace_document(
        &mut stale,
        lexical,
        b"doc-a".to_vec(),
        "epsilon beta".to_owned(),
    )?;

    let receipt = database.compact_search(DurabilityClass::Strict)?;
    assert_eq!(receipt.dropped_tombstones, plan.dropped_tombstones);
    assert_eq!(search_entries(&database)?, plan.retained_entries);
    let after_ann_entries = search_entries(&database)?
        .into_iter()
        .filter(|(key, _)| ann_store::is_ann_physical_key(key))
        .collect::<Vec<_>>();
    assert_eq!(after_ann_entries, before_ann_entries);
    let after_ann = database.search_ann_latest(vectors, &query, ann_options()?)?;
    assert_eq!(after_ann.hits, before_ann.hits);
    assert_eq!(after_ann.build_identity, before_ann.build_identity);
    assert_eq!(
        database.search_vector_exact_latest(vectors, &query, 2)?,
        before_exact
    );
    assert_eq!(historical.match_text(lexical, "alpha gamma", 10)?.len(), 2);

    database.commit_optimistic(stale)?;
    assert_eq!(
        database
            .match_latest_text(lexical, "epsilon", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc-a".to_vec()]
    );
    Ok(())
}

fn assert_crash_boundary(boundary: CommitBoundary) -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc-a".to_vec(), "alpha beta")?;
    seed.index_document(index, b"doc-b".to_vec(), "beta gamma")?;
    seed.commit()?;
    let mut lifecycle = database.begin(2, DurabilityClass::Strict)?;
    lifecycle.replace_document(index, b"doc-a".to_vec(), "delta beta")?;
    lifecycle.delete_document(index, b"doc-b".to_vec())?;
    lifecycle.commit()?;
    let prior_entries = search_entries(&database)?;
    let complete_entries =
        plan_search_compaction(&database.pages, &database.blobs, search_root(&database)?)?
            .retained_entries;

    let result = database.compact_search_at(DurabilityClass::Strict, Some(boundary));
    assert!(matches!(
        result,
        Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
    ));
    drop(database);

    let mut reopened = NativeDatabase::open(temporary.path())?;
    let recovered_entries = search_entries(&reopened)?;
    assert!(recovered_entries == prior_entries || recovered_entries == complete_entries);
    assert!(
        reopened
            .match_latest_text(index, "alpha gamma", 10)?
            .is_empty()
    );
    let retry = reopened.compact_search(DurabilityClass::Strict)?;
    if recovered_entries == prior_entries {
        assert!(retry.commit.is_some());
        assert!(retry.dropped_tombstones > 0);
    } else {
        assert!(retry.commit.is_none());
        assert_eq!(retry.dropped_tombstones, 0);
    }
    Ok(())
}

#[test]
fn every_search_compaction_boundary_recovers_prior_or_complete_root() -> Result<(), TestError> {
    for boundary in [
        CommitBoundary::BlobStaged,
        CommitBoundary::BlobPromoted,
        CommitBoundary::PageAppended,
        CommitBoundary::PageSynchronized,
        CommitBoundary::WalAppended,
        CommitBoundary::WalSynchronized,
        CommitBoundary::RootPublished,
    ] {
        assert_crash_boundary(boundary)?;
    }
    Ok(())
}

fn forged_roots(roots: &RootSet, search_root: PageId) -> Result<RootSet, NativeRuntimeError> {
    let mut entries = roots
        .iter_roots()
        .collect::<std::collections::BTreeMap<_, _>>();
    entries.insert(SLOT_SEARCH, search_root);
    Ok(RootSet::committed(
        roots
            .visible_csn()
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
        roots.catalog_version(),
        roots
            .wal_anchor()
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
        entries,
        roots.blob_generation(),
    )?)
}

#[derive(Clone, Copy)]
enum ExpectedCorruption {
    Search,
    Ann,
}

fn assert_forgery_rejected(
    database: &mut NativeDatabase,
    roots: &RootSet,
    key: Vec<u8>,
    value: Vec<u8>,
    expected: ExpectedCorruption,
) -> Result<(), TestError> {
    database.coordinator = CommitCoordinator::restore(roots.clone())?;
    let visible_csn = roots
        .visible_csn()
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let root = roots
        .root(SLOT_SEARCH)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let forged_tree = BTree::from_root(root)
        .upsert(&mut database.pages, visible_csn, key, value)?
        .tree;
    database.coordinator = CommitCoordinator::restore(forged_roots(
        roots,
        forged_tree
            .root()
            .ok_or(NativeRuntimeError::InvalidSearchTree)?,
    )?)?;
    let pages_before = database.pages.page_count();
    let wal_before = wal_bytes(database)?;
    let result = database.compact_search(DurabilityClass::Strict);
    match expected {
        ExpectedCorruption::Search => {
            assert!(matches!(result, Err(NativeRuntimeError::InvalidSearchTree)));
        }
        ExpectedCorruption::Ann => {
            assert!(matches!(result, Err(NativeRuntimeError::InvalidAnnTree)));
        }
    }
    assert_eq!(database.pages.page_count(), pages_before);
    assert_eq!(wal_bytes(database)?, wal_before);
    Ok(())
}

#[test]
fn malformed_v2_roots_are_rejected_before_compaction_append() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc-live".to_vec(), "shared live")?;
    seed.index_document(index, b"doc-deleted".to_vec(), "shared deleted")?;
    seed.commit()?;
    let mut lifecycle = database.begin(2, DurabilityClass::Strict)?;
    lifecycle.delete_document(index, b"doc-deleted".to_vec())?;
    lifecycle.commit()?;
    let roots = database.coordinator.snapshot(0)?.roots().clone();
    let entries = search_entries(&database)?;
    let posting_value = entries
        .iter()
        .find(|(key, _)| key.first() == Some(&SEARCH_POSTING_PREFIX))
        .map(|(_, value)| value.clone())
        .ok_or("missing live posting")?;
    let mut malformed_tombstone = SEARCH_DOCUMENT_TOMBSTONE.to_vec();
    malformed_tombstone.push(0);

    assert_forgery_rejected(
        &mut database,
        &roots,
        search_document_key(index, b"doc-deleted")?,
        malformed_tombstone,
        ExpectedCorruption::Search,
    )?;
    assert_forgery_rejected(
        &mut database,
        &roots,
        vec![0xff, 0x01],
        vec![0],
        ExpectedCorruption::Search,
    )?;
    assert_forgery_rejected(
        &mut database,
        &roots,
        search_posting_key(index, b"orphan", b"doc-live")?,
        posting_value,
        ExpectedCorruption::Search,
    )?;
    assert_forgery_rejected(
        &mut database,
        &roots,
        vec![ann_store::ANN_INDEX_META_PREFIX],
        vec![0],
        ExpectedCorruption::Ann,
    )?;
    database.coordinator = CommitCoordinator::restore(roots)?;
    Ok(())
}

#[test]
fn v1_and_inline_search_roots_do_not_advance_storage() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc".to_vec(), "alpha beta")?;
    seed.commit()?;
    let root = search_root(&database)?;
    assert_eq!(
        BTree::from_root(root).get(&database.pages, SEARCH_FORMAT_KEY)?,
        Some(SEARCH_FORMAT_VALUE_V1.to_vec())
    );
    let pages_before = database.pages.page_count();
    let wal_before = wal_bytes(&database)?;
    let receipt = database.compact_search(DurabilityClass::Strict)?;
    assert_eq!(receipt.dropped_tombstones, 0);
    assert!(receipt.commit.is_none());
    assert_eq!(database.pages.page_count(), pages_before);
    assert_eq!(wal_bytes(&database)?, wal_before);

    database.search_format = SearchFormat::InlineStateV1;
    assert!(matches!(
        database.compact_search(DurabilityClass::Strict),
        Err(NativeRuntimeError::SearchCompactionUnsupported)
    ));
    assert_eq!(database.pages.page_count(), pages_before);
    assert_eq!(wal_bytes(&database)?, wal_before);
    Ok(())
}

#[test]
fn missing_search_blob_is_rejected_before_compaction_append() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let index = ObjectId::new(100)?;
    let text = format!("blobtoken {}", "x ".repeat(SEARCH_INLINE_VALUE_LIMIT));
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc".to_vec(), text)?;
    seed.commit()?;
    let blob = fs::read_dir(temporary.path().join("blobs"))?
        .next()
        .ok_or("missing source blob")??
        .path();
    fs::remove_file(blob)?;
    let pages_before = database.pages.page_count();
    let wal_before = wal_bytes(&database)?;

    assert!(matches!(
        database.compact_search(DurabilityClass::Strict),
        Err(NativeRuntimeError::Blob(_))
    ));
    assert_eq!(database.pages.page_count(), pages_before);
    assert_eq!(wal_bytes(&database)?, wal_before);
    Ok(())
}

#[test]
fn compaction_enables_blob_collection_without_document_resurrection() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let index = ObjectId::new(100)?;
    let old_text = format!("oldtoken {}", "x ".repeat(SEARCH_INLINE_VALUE_LIMIT));
    let new_text = format!("newtoken {}", "y ".repeat(SEARCH_INLINE_VALUE_LIMIT));
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc".to_vec(), &old_text)?;
    seed.commit()?;
    let historical = database.snapshot(1)?;
    let mut replacement = database.begin(2, DurabilityClass::Strict)?;
    replacement.replace_document(index, b"doc".to_vec(), &new_text)?;
    replacement.commit()?;
    let mut deletion = database.begin(3, DurabilityClass::Strict)?;
    deletion.delete_document(index, b"doc".to_vec())?;
    deletion.commit()?;

    assert!(
        database
            .compact_search(DurabilityClass::Strict)?
            .commit
            .is_some()
    );
    assert!(
        database
            .match_latest_text(index, "oldtoken newtoken", 10)?
            .is_empty()
    );
    assert_eq!(historical.match_text(index, "oldtoken", 10)?.len(), 1);
    let blobs_before = database.blobs.recovery()?.blob_count;
    assert!(blobs_before >= 2);
    assert!(database.vacuum_pages()?.applied);
    database.checkpoint()?;
    database.truncate_wal_at_retention_checkpoint()?;
    let collection = database.collect_blobs()?;
    assert!(collection.removed_files >= 2);
    assert!(
        database
            .match_latest_text(index, "oldtoken newtoken", 10)?
            .is_empty()
    );
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    assert!(
        reopened
            .match_latest_text(index, "oldtoken newtoken", 10)?
            .is_empty()
    );
    assert!(reopened.recovery_report().blob_count < blobs_before);
    Ok(())
}
