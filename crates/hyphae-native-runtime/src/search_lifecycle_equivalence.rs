// SPDX-License-Identifier: GPL-3.0-only

use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_btree::BTree;
use hyphae_native_mvcc::RootSet;
use hyphae_native_types::{Csn, DurabilityClass, ObjectId, PageId};

use crate::{
    FAIL_FULL_CATALOG_STATE_LOAD, FAIL_FULL_STATE_LOAD, NativeDatabase, NativeRuntimeError,
    SEARCH_DOCUMENT_TOMBSTONE, SEARCH_FORMAT_KEY, SEARCH_FORMAT_VALUE_V1, SEARCH_FORMAT_VALUE_V2,
    SEARCH_POSTING_TOMBSTONE, SEARCH_TERM_META_TOMBSTONE, SLOT_SEARCH, decode_search_term_metadata,
    load_search_state, search_document_key, search_posting_key, search_term_meta_key,
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-search-lifecycle-{}-{}",
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
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

fn search_root(database: &NativeDatabase, logical_time: i64) -> Result<PageId, NativeRuntimeError> {
    database
        .coordinator
        .snapshot(logical_time)?
        .roots()
        .root(SLOT_SEARCH)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)
}

fn forged_search_roots(
    roots: &RootSet,
    search_root: PageId,
) -> Result<RootSet, NativeRuntimeError> {
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

#[test]
fn lifecycle_upgrades_v1_tombstones_exactly_and_revives_one_identity() -> Result<(), Box<dyn Error>>
{
    let temporary = TestDirectory::new();
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc-a".to_vec(), "shared old")?;
    seed.index_document(index, b"doc-b".to_vec(), "shared keep")?;
    seed.commit()?;
    let historical = database.snapshot(1)?;
    let historical_tree = BTree::from_root(search_root(&database, 1)?);
    assert_eq!(
        historical_tree.get(&database.pages, SEARCH_FORMAT_KEY)?,
        Some(SEARCH_FORMAT_VALUE_V1.to_vec())
    );

    let mut lifecycle = database.begin(2, DurabilityClass::Strict)?;
    lifecycle.replace_document(index, b"doc-a".to_vec(), "shared new")?;
    lifecycle.delete_document(index, b"doc-b".to_vec())?;
    lifecycle.commit()?;

    let current_tree = BTree::from_root(search_root(&database, 2)?);
    assert_eq!(
        current_tree.get(&database.pages, SEARCH_FORMAT_KEY)?,
        Some(SEARCH_FORMAT_VALUE_V2.to_vec())
    );
    assert_eq!(
        current_tree.get(&database.pages, &search_document_key(index, b"doc-b")?)?,
        Some(SEARCH_DOCUMENT_TOMBSTONE.to_vec())
    );
    assert_eq!(
        current_tree.get(&database.pages, &search_term_meta_key(index, b"old")?)?,
        Some(SEARCH_TERM_META_TOMBSTONE.to_vec())
    );
    assert_eq!(
        current_tree.get(
            &database.pages,
            &search_posting_key(index, b"old", b"doc-a")?
        )?,
        Some(SEARCH_POSTING_TOMBSTONE.to_vec())
    );
    assert_eq!(
        current_tree.get(&database.pages, &search_term_meta_key(index, b"keep")?)?,
        Some(SEARCH_TERM_META_TOMBSTONE.to_vec())
    );
    let shared = current_tree
        .get(&database.pages, &search_term_meta_key(index, b"shared")?)?
        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    assert_eq!(decode_search_term_metadata(&shared)?, 1);
    assert_eq!(
        historical
            .match_text(index, "old keep", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc-a".to_vec(), b"doc-b".to_vec()]
    );
    assert!(
        database
            .match_latest_text(index, "old keep", 10)?
            .is_empty()
    );

    let mut revive = database.begin(3, DurabilityClass::Strict)?;
    revive.index_document(index, b"doc-b".to_vec(), "keep revived")?;
    revive.commit()?;
    assert_eq!(
        database
            .match_latest_text(index, "keep", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc-b".to_vec()]
    );
    Ok(())
}

#[test]
fn v1_and_v2_reject_noncanonical_document_tombstones() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc".to_vec(), "source")?;
    seed.commit()?;

    let v1_roots = database.coordinator.snapshot(1)?.roots().clone();
    let v1_tree = BTree::from_root(
        v1_roots
            .root(SLOT_SEARCH)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
    )
    .upsert(
        &mut database.pages,
        Csn::new(1)?,
        search_document_key(index, b"doc")?,
        SEARCH_DOCUMENT_TOMBSTONE.to_vec(),
    )?
    .tree;
    let forged_v1 = forged_search_roots(
        &v1_roots,
        v1_tree
            .root()
            .ok_or(NativeRuntimeError::InvalidSearchTree)?,
    )?;
    assert!(matches!(
        load_search_state(&database.pages, &database.blobs, &forged_v1),
        Err(NativeRuntimeError::InvalidSearchTree)
    ));

    let mut delete = database.begin(2, DurabilityClass::Strict)?;
    delete.delete_document(index, b"doc".to_vec())?;
    delete.commit()?;
    let v2_roots = database.coordinator.snapshot(2)?.roots().clone();
    let mut malformed = SEARCH_DOCUMENT_TOMBSTONE.to_vec();
    malformed.push(0);
    let v2_tree = BTree::from_root(
        v2_roots
            .root(SLOT_SEARCH)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
    )
    .upsert(
        &mut database.pages,
        Csn::new(2)?,
        search_document_key(index, b"doc")?,
        malformed,
    )?
    .tree;
    let forged_v2 = forged_search_roots(
        &v2_roots,
        v2_tree
            .root()
            .ok_or(NativeRuntimeError::InvalidSearchTree)?,
    )?;
    assert!(matches!(
        load_search_state(&database.pages, &database.blobs, &forged_v2),
        Err(NativeRuntimeError::InvalidSearchTree)
    ));
    Ok(())
}

#[test]
fn delta_lifecycle_never_materializes_complete_state_or_catalog() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let index = ObjectId::new(100)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(index, "documents")?;
    seed.index_document(index, b"doc".to_vec(), "old")?;
    for sequence in 1_000_u128..1_256 {
        seed.create_relation(
            ObjectId::new(sequence)?,
            &format!("unrelated_relation_{sequence}"),
        )?;
    }
    seed.commit()?;

    FAIL_FULL_STATE_LOAD.set(true);
    FAIL_FULL_CATALOG_STATE_LOAD.set(true);
    let result = (|| -> Result<(), NativeRuntimeError> {
        let mut delta = database.begin_optimistic_delta(2, DurabilityClass::Memory)?;
        database.stage_delta_replace_document(
            &mut delta,
            index,
            b"doc".to_vec(),
            "replacement".to_owned(),
        )?;
        database.stage_delta_delete_document(&mut delta, index, b"doc".to_vec())?;
        database.stage_delta_index_document(
            &mut delta,
            index,
            b"doc".to_vec(),
            "current".to_owned(),
        )?;
        database.commit_optimistic(delta)?;
        Ok(())
    })();
    FAIL_FULL_STATE_LOAD.set(false);
    FAIL_FULL_CATALOG_STATE_LOAD.set(false);
    result?;
    assert_eq!(
        database
            .match_latest_text(index, "current", 10)?
            .into_iter()
            .map(|hit| hit.document_id)
            .collect::<Vec<_>>(),
        [b"doc".to_vec()]
    );
    Ok(())
}
