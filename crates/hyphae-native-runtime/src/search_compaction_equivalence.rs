// SPDX-License-Identifier: Apache-2.0

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_btree::BTree;
use hyphae_native_mvcc::{CommitCoordinator, RootSet};
use hyphae_native_pages::PageStore;
use hyphae_native_types::{DurabilityClass, ObjectId, PageId};

use crate::{
    AnnSearchOptions, CommitBoundary, HnswConfig, NativeDatabase, NativeRuntimeError,
    SEARCH_DOCUMENT_TOMBSTONE, SEARCH_FORMAT_KEY, SEARCH_FORMAT_VALUE_V1,
    SEARCH_INLINE_VALUE_LIMIT, SEARCH_POSTING_PREFIX, SLOT_SEARCH, SearchFormat, Vector,
    VectorIndexDefinition, VectorMetric, WAL_FILE, ann_store, compact_search_tree,
    load_catalog_state, load_search_state, load_search_state_with_retained, load_state,
    plan_search_compaction, rebuild_btree_root, search_document_key, search_posting_key,
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
    assert!(before_ann_entries.iter().all(|(key, value)| {
        key.first().is_some_and(|prefix| *prefix <= 8)
            && ![
                b"HYANNM05".as_slice(),
                b"HYANNO01".as_slice(),
                b"HYANND02".as_slice(),
                b"HYANNN01".as_slice(),
                b"HYANNA02".as_slice(),
            ]
            .iter()
            .any(|magic| value.windows(8).any(|window| window == *magic))
    }));
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

#[test]
#[allow(clippy::too_many_lines)]
fn test_installed_m05_root_loads_compacts_vacuums_and_rejects_corruption() -> Result<(), TestError>
{
    let temporary = TestDirectory::new();
    let lexical = ObjectId::new(100)?;
    let vectors = ObjectId::new(200)?;
    let first = ObjectId::new(201)?;
    let second = ObjectId::new(202)?;
    let inserted = ObjectId::new(203)?;
    let config = ann_config()?;
    let definition = VectorIndexDefinition::new(vectors, 3, VectorMetric::SquaredL2, config)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(lexical, "documents")?;
    seed.index_document(lexical, b"live".to_vec(), "shared live")?;
    seed.index_document(lexical, b"deleted".to_vec(), "shared deleted")?;
    seed.create_vector_index(vectors, "vectors", 3, VectorMetric::SquaredL2, config)?;
    seed.upsert_vectors(
        vectors,
        [
            (first, Vector::new([1.0, 0.0, 0.0])?),
            (second, Vector::new([2.0, 0.0, 0.0])?),
        ],
    )?;
    seed.commit()?;
    let mut legacy = database.begin(2, DurabilityClass::Strict)?;
    legacy.upsert_vector(vectors, first, Vector::new([1.5, 0.0, 0.0])?)?;
    legacy.delete_document(lexical, b"deleted".to_vec())?;
    legacy.commit()?;

    let committed = database.coordinator.snapshot(0)?.roots().clone();
    let visible_csn = committed
        .visible_csn()
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let search_root = committed
        .root(SLOT_SEARCH)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let installed = ann_store::install_test_m05_tree(
        &mut database.pages,
        &database.buffer_pool,
        search_root,
        definition,
        visible_csn,
        &[
            ann_store::TestOverlayMutation::Tombstone(first),
            ann_store::TestOverlayMutation::Upsert(inserted, Vector::new([0.5, 0.0, 0.0])?),
        ],
    )
    .map_err(|error| format!("install M05: {error}"))?;
    assert!(installed.node_count >= 32);
    let installed_root = installed
        .tree
        .root()
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let installed_roots = forged_roots(&committed, installed_root)?;

    // This root is test-installed as already committed authority. No WAL path
    // can legitimately synthesize M05 in the passive-reader slice.
    let (lexical_state, lexical_retained_bytes) =
        load_search_state_with_retained(&database.pages, &database.blobs, &installed_roots)?;
    assert!(lexical_retained_bytes > 0);
    assert!(lexical_retained_bytes < crate::RECOVERY_MEMORY_BYTES);
    let installed_catalog = load_catalog_state(&database.pages, &database.blobs, &installed_roots)?;
    ann_store::reset_full_stream_observation_for_test();
    assert!(matches!(
        ann_store::load_with_memory_limit(
            &database.pages,
            Some(installed_root),
            &installed_catalog,
            1_024 * 1_024,
        ),
        Err(NativeRuntimeError::InvalidAnnTree)
    ));
    assert_eq!(ann_store::full_stream_observation_for_test().0, 0);
    load_state(&database.pages, &database.blobs, &installed_roots)?;
    load_state(&database.pages, &database.blobs, &installed_roots)?;
    database.coordinator = CommitCoordinator::restore(installed_roots.clone())?;
    let query = Vector::new([0.5, 0.0, 0.0])?;
    let exact = database.search_vector_exact_latest(vectors, &query, 2)?;
    let approximate = database.search_ann_latest(vectors, &query, ann_options()?)?;
    assert_eq!(approximate.hits, exact);
    assert_eq!(approximate.build_identity, installed.view_identity);
    assert_eq!(
        exact.iter().map(|hit| hit.object_id).collect::<Vec<_>>(),
        [inserted, second]
    );

    let before_ann = BTree::from_root(installed_root)
        .scan(&database.pages)?
        .into_iter()
        .filter(|(key, _)| ann_store::is_ann_physical_key(key))
        .collect::<Vec<_>>();
    let compacted = compact_search_tree(
        &mut database.pages,
        &database.blobs,
        Some(installed_root),
        visible_csn,
    )?;
    let compacted_root = compacted
        .root()
        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    let after_ann = BTree::from_root(compacted_root)
        .scan(&database.pages)?
        .into_iter()
        .filter(|(key, _)| ann_store::is_ann_physical_key(key))
        .collect::<Vec<_>>();
    assert_eq!(after_ann, before_ann);
    let compacted_roots = forged_roots(&installed_roots, compacted_root)?;
    load_state(&database.pages, &database.blobs, &compacted_roots)?;

    let candidate_path = temporary.path().join("test-m05-vacuum.pages");
    let mut candidate = PageStore::create(&candidate_path)?;
    let vacuum_root =
        rebuild_btree_root(&database.pages, &mut candidate, compacted_root, visible_csn)?;
    let vacuum_ann = BTree::from_root(vacuum_root)
        .scan(&candidate)?
        .into_iter()
        .filter(|(key, _)| ann_store::is_ann_physical_key(key))
        .collect::<Vec<_>>();
    assert_eq!(vacuum_ann, before_ann);
    let catalog = load_catalog_state(&database.pages, &database.blobs, &compacted_roots)?;
    let vacuum_state = ann_store::load(&candidate, Some(vacuum_root), &catalog)?;
    assert_eq!(vacuum_state.search_exact(vectors, &query, 2)?, exact);

    let overlay_leaf = before_ann
        .iter()
        .find(|(key, _)| key.first() == Some(&10))
        .cloned()
        .ok_or("missing M05 overlay leaf")?;
    let mut oversized_leaf = overlay_leaf.1;
    oversized_leaf.extend_from_slice(&[0; 7_000]);
    let oversized_leaf_root = BTree::from_root(installed_root)
        .upsert(
            &mut database.pages,
            visible_csn,
            overlay_leaf.0,
            oversized_leaf,
        )?
        .tree
        .root()
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let oversized_leaf_roots = forged_roots(&installed_roots, oversized_leaf_root)?;
    assert_eq!(
        load_search_state(&database.pages, &database.blobs, &oversized_leaf_roots)?,
        lexical_state
    );
    ann_store::reset_full_stream_observation_for_test();
    assert!(matches!(
        load_state(&database.pages, &database.blobs, &oversized_leaf_roots,),
        Err(NativeRuntimeError::InvalidAnnTree)
    ));
    assert_eq!(ann_store::full_stream_overlay_decodes_for_test(), 0);

    let manifest = before_ann
        .iter()
        .find(|(key, _)| key.first() == Some(&9))
        .cloned()
        .ok_or("missing M05 manifest")?;
    let mut corrupted_manifest = manifest.1;
    corrupted_manifest[40] ^= 1;
    let corrupted = BTree::from_root(installed_root)
        .upsert(
            &mut database.pages,
            visible_csn,
            manifest.0,
            corrupted_manifest,
        )?
        .tree
        .root()
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let corrupted_roots = forged_roots(&installed_roots, corrupted)?;
    assert!(matches!(
        load_state(&database.pages, &database.blobs, &corrupted_roots),
        Err(NativeRuntimeError::InvalidAnnTree)
    ));
    assert_eq!(
        database.search_vector_exact_latest(vectors, &query, 2)?,
        exact
    );
    drop(candidate);
    fs::remove_file(candidate_path)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn maximum_m05_nodes_stream_once_and_excess_fails_before_decode() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let lexical = ObjectId::new(299)?;
    let vectors = ObjectId::new(300)?;
    let config = ann_config()?;
    let definition = VectorIndexDefinition::new(vectors, 3, VectorMetric::SquaredL2, config)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(lexical, "mixed-large-ann")?;
    seed.index_document(lexical, b"doc".to_vec(), "lexical authority")?;
    seed.create_vector_index(
        vectors,
        "maximum-overlay",
        3,
        VectorMetric::SquaredL2,
        config,
    )?;
    seed.commit()?;
    let committed = database.coordinator.snapshot(0)?.roots().clone();
    let visible_csn = committed
        .visible_csn()
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let search_root = committed
        .root(SLOT_SEARCH)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;

    let mut identities = std::collections::BTreeSet::new();
    let mut random = 0x9e37_79b9_7f4a_7c15_d1b5_4a32_d192_ed03_u128;
    while identities.len() < ann_store::MAX_ANN_DELTA_RECORDS {
        random ^= random << 17;
        random ^= random >> 29;
        random ^= random << 41;
        identities.insert(ObjectId::new(random.max(1))?);
    }
    let mutations = identities
        .iter()
        .enumerate()
        .map(|(position, object_id)| {
            Ok(ann_store::TestOverlayMutation::Upsert(
                *object_id,
                Vector::new([
                    f32::from(u16::try_from(position)?),
                    f32::from(u16::try_from(position % 251)?),
                    1.0,
                ])?,
            ))
        })
        .collect::<Result<Vec<_>, TestError>>()?;
    let installed = ann_store::install_test_m05_tree(
        &mut database.pages,
        &database.buffer_pool,
        search_root,
        definition,
        visible_csn,
        &mutations,
    )?;
    assert!(installed.node_count > 100_000);
    assert!(installed.node_count <= ann_store::MAX_ANN_DELTA_RECORDS * 32);
    let installed_root = installed
        .tree
        .root()
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let installed_roots = forged_roots(&committed, installed_root)?;
    let mut physical_entries = 0_usize;
    BTree::from_root(installed_root).visit_range_borrowed_with_control(
        &database.pages,
        std::ops::Bound::Included(&[6]),
        std::ops::Bound::Excluded(&[12]),
        hyphae_native_btree::BorrowedVisitLimits {
            maximum_entries: usize::MAX,
            maximum_bytes: u64::MAX,
        },
        || std::ops::ControlFlow::Continue(()),
        |_, _| {
            physical_entries = physical_entries.saturating_add(1);
            std::ops::ControlFlow::Continue(())
        },
    )?;
    assert!(load_search_state(&database.pages, &database.blobs, &installed_roots).is_ok());
    ann_store::reset_full_stream_observation_for_test();
    load_state(&database.pages, &database.blobs, &installed_roots)?;
    let (visits, node_decodes, peak_frontier) = ann_store::full_stream_observation_for_test();
    assert_eq!(visits, physical_entries);
    assert_eq!(node_decodes, installed.node_count);
    assert_eq!(peak_frontier, ann_store::MAX_ANN_DELTA_RECORDS);

    let metadata_key = ann_store::meta_key(vectors);
    let metadata = BTree::from_root(installed_root)
        .get(&database.pages, &metadata_key)?
        .ok_or("missing M05 metadata")?;
    let mut understated = metadata.clone();
    understated[240..248].copy_from_slice(
        &u64::try_from(installed.node_count - 1)
            .map_err(|_| NativeRuntimeError::InvalidAnnTree)?
            .to_le_bytes(),
    );
    let understated_root = BTree::from_root(installed_root)
        .upsert(
            &mut database.pages,
            visible_csn,
            metadata_key.clone(),
            understated,
        )?
        .tree
        .root()
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    ann_store::reset_full_stream_observation_for_test();
    assert!(matches!(
        load_state(
            &database.pages,
            &database.blobs,
            &forged_roots(&installed_roots, understated_root)?,
        ),
        Err(NativeRuntimeError::InvalidAnnTree)
    ));
    let (_, excess_node_decodes, _) = ann_store::full_stream_observation_for_test();
    assert_eq!(excess_node_decodes, installed.node_count - 1);

    let mut over_recovery_memory = metadata;
    over_recovery_memory[120..128].copy_from_slice(&1_u64.to_le_bytes());
    over_recovery_memory[128..136].copy_from_slice(&(32_u64 * 1_024 * 1_024).to_le_bytes());
    let oversized_root = BTree::from_root(installed_root)
        .upsert(
            &mut database.pages,
            visible_csn,
            metadata_key,
            over_recovery_memory,
        )?
        .tree
        .root()
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    ann_store::reset_full_stream_observation_for_test();
    assert!(matches!(
        load_state(
            &database.pages,
            &database.blobs,
            &forged_roots(&installed_roots, oversized_root)?,
        ),
        Err(NativeRuntimeError::InvalidAnnTree)
    ));
    assert_eq!(ann_store::full_stream_observation_for_test().0, 0);
    Ok(())
}

#[test]
fn empty_m05_rejects_initial_bulk_capture_and_publication_before_pages() -> Result<(), TestError> {
    let temporary = TestDirectory::new();
    let vectors = ObjectId::new(400)?;
    let config = ann_config()?;
    let definition = VectorIndexDefinition::new(vectors, 3, VectorMetric::SquaredL2, config)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_vector_index(vectors, "empty-overlay", 3, VectorMetric::SquaredL2, config)?;
    seed.commit()?;
    let committed = database.coordinator.snapshot(0)?.roots().clone();
    let visible_csn = committed
        .visible_csn()
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let search_root = committed
        .root(SLOT_SEARCH)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let installed = ann_store::install_test_m05_tree(
        &mut database.pages,
        &database.buffer_pool,
        search_root,
        definition,
        visible_csn,
        &[],
    )
    .map_err(|error| format!("install empty M05: {error}"))?;
    assert_eq!(installed.node_count, 0);
    let installed_root = installed
        .tree
        .root()
        .ok_or(NativeRuntimeError::InvalidAnnTree)?;
    let catalog = load_catalog_state(&database.pages, &database.blobs, &committed)?;
    ann_store::verify_test_m05_rejects_initial_bulk(
        &mut database.pages,
        installed_root,
        &catalog,
        definition,
        visible_csn,
    )
    .map_err(|error| format!("reject initial bulk: {error}"))?;
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
