// SPDX-License-Identifier: GPL-3.0-only

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{CommitBoundary, DurabilityClass, NativeDatabase, NativeRuntimeError, Ttl};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-list-ttl-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            path: std::env::temp_dir().join(unique),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn whole_list_ttl_is_visible_on_every_execution_surface() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"one".to_vec())?;
    assert_eq!(seed.ttl_list(b"queue"), Ttl::Persistent);
    assert!(seed.expire_list(b"queue".to_vec(), 20)?);
    assert_eq!(seed.ttl_list(b"queue"), Ttl::RemainingMicros(10));
    seed.commit()?;

    let before = database.snapshot(19)?;
    assert_eq!(before.ttl_list(b"queue"), Ttl::RemainingMicros(1));
    assert_eq!(database.llen_latest_list_at(b"queue", 19)?, 1);
    assert_eq!(
        database.lrange_latest_list_at(b"queue", 0, -1, 19)?,
        [b"one".to_vec()]
    );
    assert_eq!(
        database.ttl_latest_list(b"queue", 19)?,
        Ttl::RemainingMicros(1)
    );

    let due = database.snapshot(20)?;
    assert_eq!(due.ttl_list(b"queue"), Ttl::Missing);
    Ok(())
}

#[test]
fn list_commands_preserve_and_replace_one_expiry_boundary() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"a".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"b".to_vec())?;
    assert!(seed.expire_list(b"queue".to_vec(), 20)?);
    assert_eq!(seed.lpush(b"queue".to_vec(), b"head".to_vec())?, 3);
    assert_eq!(seed.lpop(b"queue".to_vec())?, Some(b"head".to_vec()));
    assert_eq!(seed.rpop(b"queue".to_vec())?, Some(b"b".to_vec()));
    assert_eq!(seed.rpush(b"queue".to_vec(), b"c".to_vec())?, 2);
    assert!(seed.expire_list(b"queue".to_vec(), 30)?);
    assert_eq!(seed.ttl_list(b"queue"), Ttl::RemainingMicros(20));
    seed.commit()?;

    let before = database.snapshot(29)?;
    assert_eq!(before.llen(b"queue")?, 2);
    assert_eq!(
        before.lrange(b"queue", 0, -1)?,
        [b"a".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        database.lrange_latest_list_at(b"queue", 0, -1, 29)?,
        [b"a".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        database.ttl_latest_list(b"queue", 29)?,
        Ttl::RemainingMicros(1)
    );

    let due = database.snapshot(30)?;
    assert_eq!(due.ttl_list(b"queue"), Ttl::Missing);
    assert!(matches!(
        due.llen(b"queue"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    assert!(matches!(
        database.lrange_latest_list_at(b"queue", 0, -1, 30),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    drop(due);
    drop(before);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.ttl_latest_list(b"queue", 30)?, Ttl::Missing);
    Ok(())
}

#[test]
fn list_expiry_handles_missing_empty_and_wrong_families_without_side_effects()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_list(b"empty".to_vec())?;
    seed.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    seed.create_hash(b"hash".to_vec())?;
    seed.create_set(b"set".to_vec())?;
    seed.create_sorted_set(b"sorted".to_vec())?;
    seed.commit()?;

    let mut expiry = database.begin_optimistic(2, DurabilityClass::Strict)?;
    assert!(expiry.mutations.is_empty());
    assert!(!expiry.expire_list(b"missing".to_vec(), 10)?);
    assert!(expiry.mutations.is_empty());
    for key in [
        b"scalar".as_slice(),
        b"hash".as_slice(),
        b"set".as_slice(),
        b"sorted".as_slice(),
    ] {
        assert!(matches!(
            expiry.expire_list(key.to_vec(), 10),
            Err(NativeRuntimeError::StructureKindMismatch)
        ));
    }
    assert!(expiry.mutations.is_empty());
    assert!(expiry.expire_list(b"empty".to_vec(), 10)?);
    assert_eq!(expiry.mutations.len(), 1);
    database.commit_optimistic(expiry)?;

    assert_eq!(
        database.ttl_latest_list(b"empty", 9)?,
        Ttl::RemainingMicros(1)
    );
    let cleanup = database.expire_due_structures(10, 1, DurabilityClass::Strict)?;
    assert_eq!(cleanup.expired_keys, 1);
    assert_eq!(database.ttl_latest_list(b"empty", 10)?, Ttl::Missing);
    Ok(())
}

#[test]
fn due_list_incarnations_reuse_every_structure_family_without_resurrection()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let keys: [&[u8]; 5] = [b"scalar", b"hash", b"set", b"list", b"sorted"];
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    for key in keys {
        seed.create_list(key.to_vec())?;
        seed.rpush(key.to_vec(), b"retired".to_vec())?;
        assert!(seed.expire_list(key.to_vec(), 20)?);
    }
    seed.commit()?;
    let historical = database.snapshot(19)?;
    for key in keys {
        assert_eq!(historical.lrange(key, 0, -1)?, [b"retired".to_vec()]);
    }

    let mut reuse = database.begin(20, DurabilityClass::Strict)?;
    reuse.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    reuse.create_hash(b"hash".to_vec())?;
    reuse.hset(b"hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    reuse.create_set(b"set".to_vec())?;
    reuse.sadd(b"set".to_vec(), b"member".to_vec())?;
    reuse.create_list(b"list".to_vec())?;
    reuse.rpush(b"list".to_vec(), b"current".to_vec())?;
    reuse.create_sorted_set(b"sorted".to_vec())?;
    reuse.zadd(b"sorted".to_vec(), 1.0, b"member".to_vec())?;
    reuse.commit()?;

    assert_eq!(historical.lrange(b"list", 0, -1)?, [b"retired".to_vec()]);
    let current = database.snapshot(20)?;
    assert_eq!(current.get(b"scalar"), Some(b"value".as_slice()));
    assert_eq!(current.hget(b"hash", b"field")?, Some(b"value".as_slice()));
    assert!(current.sismember(b"set", b"member")?);
    assert_eq!(current.lrange(b"list", 0, -1)?, [b"current".to_vec()]);
    assert_eq!(current.zscore(b"sorted", b"member")?, Some(1.0));
    drop(current);
    drop(historical);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.snapshot(20)?.lrange(b"list", 0, -1)?,
        [b"current".to_vec()]
    );
    assert_eq!(reopened.ttl_latest_list(b"list", 20)?, Ttl::Persistent);
    Ok(())
}

#[test]
fn whole_list_expiry_serializes_writers_and_commits_through_group_durability()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"seed".to_vec())?;
    seed.commit()?;

    let mut stale_push = database.begin_optimistic(2, DurabilityClass::Strict)?;
    stale_push.rpush(b"queue".to_vec(), b"stale".to_vec())?;
    let mut expiry = database.begin_optimistic(2, DurabilityClass::Strict)?;
    assert!(expiry.expire_list(b"queue".to_vec(), 100)?);
    database.commit_optimistic(expiry)?;
    assert!(matches!(
        database.commit_optimistic(stale_push),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut admitted_push = database.begin_optimistic(3, DurabilityClass::Strict)?;
    admitted_push.rpush(b"queue".to_vec(), b"admitted".to_vec())?;
    let mut stale_expiry = database.begin_optimistic(3, DurabilityClass::Strict)?;
    assert!(stale_expiry.expire_list(b"queue".to_vec(), 200)?);
    database.commit_optimistic(admitted_push)?;
    assert!(matches!(
        database.commit_optimistic(stale_expiry),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut group_expiry = database.begin_optimistic(4, DurabilityClass::Group)?;
    assert!(group_expiry.expire_list(b"queue".to_vec(), 300)?);
    let mut disjoint = database.begin_optimistic(4, DurabilityClass::Group)?;
    disjoint.set(b"cohort".to_vec(), b"value".to_vec(), None)?;
    let report = database
        .commit_group(vec![group_expiry, disjoint])
        .map_err(|error| format!("whole-list TTL group commit failed: {error:?}"))?;
    assert_eq!(report.accepted_commits, 2);
    assert_eq!(report.page_synchronizations, 1);
    assert_eq!(report.wal_synchronizations, 1);
    assert!(
        report
            .outcomes
            .iter()
            .all(|outcome| matches!(outcome, crate::GroupCommitOutcome::Committed(_)))
    );
    assert_eq!(
        database.ttl_latest_list(b"queue", 5)?,
        Ttl::RemainingMicros(295)
    );
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.lrange_latest_list_at(b"queue", 0, -1, 5)?,
        [b"seed".to_vec(), b"admitted".to_vec()]
    );
    assert_eq!(
        reopened.get_latest_structure(b"cohort", 5)?,
        Some(b"value".to_vec())
    );
    Ok(())
}

#[test]
fn list_expiry_shares_the_global_cleanup_order_and_bound() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.set(b"a-scalar".to_vec(), b"value".to_vec(), Some(10))?;
    seed.create_hash(b"b-hash".to_vec())?;
    seed.hset(b"b-hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    assert!(seed.expire_hash(b"b-hash".to_vec(), 10)?);
    seed.create_set(b"c-set".to_vec())?;
    seed.sadd(b"c-set".to_vec(), b"member".to_vec())?;
    assert!(seed.expire_set(b"c-set".to_vec(), 10)?);
    seed.create_list(b"d-list".to_vec())?;
    seed.rpush(b"d-list".to_vec(), b"value".to_vec())?;
    assert!(seed.expire_list(b"d-list".to_vec(), 10)?);
    seed.create_hash(b"e-hash".to_vec())?;
    seed.hset(b"e-hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    assert!(seed.expire_hash_field(b"e-hash".to_vec(), b"field".to_vec(), 10)?);
    seed.commit()?;

    let first = database.expire_due_structures(10, 4, DurabilityClass::Memory)?;
    assert_eq!(first.expired_keys, 4);
    assert!(first.more_due);
    assert_eq!(database.get_latest_structure(b"a-scalar", i64::MIN)?, None);
    assert_eq!(database.ttl_latest_hash(b"b-hash", 10)?, Ttl::Missing);
    assert_eq!(database.ttl_latest_set(b"c-set", 10)?, Ttl::Missing);
    assert_eq!(database.ttl_latest_list(b"d-list", 10)?, Ttl::Missing);
    assert_eq!(
        database.ttl_latest_hash_field(b"e-hash", b"field", 9)?,
        Ttl::RemainingMicros(1)
    );

    let second = database.expire_due_structures(10, 1, DurabilityClass::Strict)?;
    assert_eq!(second.expired_keys, 1);
    assert!(!second.more_due);
    assert_eq!(
        database.ttl_latest_hash_field(b"e-hash", b"field", 10)?,
        Ttl::Missing
    );
    Ok(())
}

fn commit_crash_boundaries() -> [CommitBoundary; 7] {
    [
        CommitBoundary::BlobStaged,
        CommitBoundary::BlobPromoted,
        CommitBoundary::PageAppended,
        CommitBoundary::PageSynchronized,
        CommitBoundary::WalAppended,
        CommitBoundary::WalSynchronized,
        CommitBoundary::RootPublished,
    ]
}

#[test]
fn every_list_cleanup_boundary_recovers_due_or_complete_retirement()
-> Result<(), Box<dyn std::error::Error>> {
    for boundary in commit_crash_boundaries() {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_list(b"queue".to_vec())?;
        for index in 0..130_u32 {
            seed.rpush(b"queue".to_vec(), index.to_be_bytes().to_vec())?;
        }
        assert!(seed.expire_list(b"queue".to_vec(), 10)?);
        seed.commit()?;

        assert!(matches!(
            database.expire_due_structures_at(
                10,
                1,
                DurabilityClass::Strict,
                Some(boundary),
            ),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let roots = reopened.coordinator.snapshot(10)?;
        let root = roots
            .roots()
            .root(crate::SLOT_STRUCTURE)
            .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
        let tree = hyphae_native_btree::BTree::from_root(root);
        let metadata = tree
            .get(&reopened.pages, &crate::structure_list_meta_key(b"queue")?)?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        let marker = tree
            .get(&reopened.pages, &crate::structure_expiry_key(10, b"queue")?)?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        let chunks = tree.scan_prefix(
            &reopened.pages,
            &crate::structure_list_chunk_prefix(b"queue")?,
        )?;
        match reopened
            .recovery_report()
            .visible_csn
            .map(hyphae_native_types::Csn::get)
        {
            Some(1) => {
                let decoded = crate::decode_live_list_metadata(&metadata)?
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                assert_eq!(decoded.length, 130);
                assert_eq!(decoded.expires_at_micros, Some(10));
                assert_eq!(marker, vec![crate::STRUCTURE_LIST_EXPIRY_LIVE]);
                assert_eq!(reopened.llen_latest_list_at(b"queue", 9)?, 130);
                assert!(
                    chunks
                        .iter()
                        .all(|(_, value)| !crate::is_structure_tombstone(value))
                );
            }
            Some(2) => {
                assert!(crate::is_structure_tombstone(&metadata));
                assert_eq!(marker, vec![crate::STRUCTURE_EXPIRY_TOMBSTONE]);
                assert!(
                    chunks
                        .iter()
                        .all(|(_, value)| crate::is_structure_tombstone(value))
                );
                assert_eq!(reopened.ttl_latest_list(b"queue", 10)?, Ttl::Missing);
            }
            found => {
                return Err(format!(
                    "unexpected recovered list cleanup CSN {found:?} at {boundary:?}"
                )
                .into());
            }
        }
    }
    Ok(())
}

#[test]
fn expired_blob_list_compacts_vacuums_and_collects_without_resurrection()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let blob = vec![0x41; 9_000];
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_list(b"ephemeral".to_vec())?;
    seed.rpush(b"ephemeral".to_vec(), blob.clone())?;
    for index in 0..130_u32 {
        seed.rpush(b"ephemeral".to_vec(), index.to_be_bytes().to_vec())?;
    }
    assert!(seed.expire_list(b"ephemeral".to_vec(), 10)?);
    seed.commit()?;
    let historical = database.snapshot(9)?;

    let cleanup = database.expire_due_structures(10, 1, DurabilityClass::Strict)?;
    assert_eq!(cleanup.expired_keys, 1);
    let compaction = database.compact_structure(DurabilityClass::Strict)?;
    assert!(compaction.dropped_tombstones >= 4);
    assert_eq!(historical.llen(b"ephemeral")?, 131);
    assert_eq!(historical.lrange(b"ephemeral", 0, 0)?, [blob]);
    assert_eq!(database.ttl_latest_list(b"ephemeral", 10)?, Ttl::Missing);
    drop(historical);

    let vacuum = database.vacuum_pages()?;
    assert!(vacuum.applied);
    database.checkpoint()?;
    database.truncate_wal_at_retention_checkpoint()?;
    let collection = database.collect_blobs()?;
    assert!(collection.removed_files >= 1);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.ttl_latest_list(b"ephemeral", 10)?, Ttl::Missing);
    assert!(matches!(
        reopened.llen_latest_list_at(b"ephemeral", 10),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    Ok(())
}

#[test]
fn malformed_list_expiry_metadata_markers_and_chunks_fail_complete_validation()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    for index in 0..70_u32 {
        seed.rpush(b"queue".to_vec(), index.to_be_bytes().to_vec())?;
    }
    assert!(seed.expire_list(b"queue".to_vec(), 10)?);
    seed.commit()?;

    let root_set = database.coordinator.snapshot(2)?.roots().clone();
    let root = root_set
        .root(crate::SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let metadata_key = crate::structure_list_meta_key(b"queue")?;
    let encoded_metadata = hyphae_native_btree::BTree::from_root(root)
        .get(&database.pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let metadata = crate::decode_list_metadata(&encoded_metadata)?;
    let forgeries = [
        (
            metadata_key,
            crate::encode_list_metadata(crate::ListMetadata {
                expires_at_micros: Some(11),
                ..metadata
            }),
        ),
        (
            crate::structure_expiry_key(10, b"queue")?,
            vec![crate::STRUCTURE_SET_EXPIRY_LIVE],
        ),
        (
            crate::structure_expiry_key(10, b"queue")?,
            vec![crate::STRUCTURE_EXPIRY_TOMBSTONE],
        ),
        (
            crate::structure_expiry_key(10, b"orphan")?,
            vec![crate::STRUCTURE_LIST_EXPIRY_LIVE],
        ),
        (
            crate::structure_list_chunk_key(b"queue", metadata.head_chunk)?,
            crate::structure_tombstone_value(),
        ),
    ];
    for (key, value) in forgeries {
        let bad_tree = hyphae_native_btree::BTree::from_root(root)
            .upsert(
                &mut database.pages,
                hyphae_native_types::Csn::new(1)?,
                key,
                value,
            )?
            .tree;
        let mut roots = root_set
            .iter_roots()
            .collect::<std::collections::BTreeMap<_, _>>();
        roots.insert(
            crate::SLOT_STRUCTURE,
            bad_tree
                .root()
                .ok_or(NativeRuntimeError::InvalidStructureTree)?,
        );
        let forged = hyphae_native_mvcc::RootSet::committed(
            root_set
                .visible_csn()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            root_set.catalog_version(),
            root_set
                .wal_anchor()
                .ok_or(NativeRuntimeError::InvalidCommittedRoot)?,
            roots,
            root_set.blob_generation(),
        )?;
        assert!(matches!(
            crate::load_structure_state(&database.pages, &database.blobs, &forged),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
    }
    Ok(())
}

#[test]
fn increment_reuses_a_due_list_as_a_scalar_without_resurrection()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_list(b"counter".to_vec())?;
    seed.rpush(b"counter".to_vec(), b"retired".to_vec())?;
    assert!(seed.expire_list(b"counter".to_vec(), 10)?);
    seed.commit()?;

    let mut reuse = database.begin(10, DurabilityClass::Strict)?;
    assert_eq!(reuse.increment_i64(b"counter".to_vec(), 7)?, 7);
    reuse.commit()?;

    assert_eq!(
        database.get_latest_structure(b"counter", 10)?,
        Some(b"7".to_vec())
    );
    assert!(matches!(
        database.llen_latest_list_at(b"counter", 10),
        Err(NativeRuntimeError::StructureKindMismatch)
    ));
    Ok(())
}
