// SPDX-License-Identifier: AGPL-3.0-only

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    CommitBoundary, DurabilityClass, NativeDatabase, NativeRuntimeError, SetAlgebraOperation,
    SetAlgebraRequest, Ttl,
};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-set-ttl-red-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        Self { path }
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
fn whole_set_ttl_is_visible_on_every_execution_surface() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd(b"members".to_vec(), b"one".to_vec())?;
    assert_eq!(seed.ttl_set(b"members"), Ttl::Persistent);
    assert!(seed.expire_set(b"members".to_vec(), 20)?);
    assert_eq!(seed.ttl_set(b"members"), Ttl::RemainingMicros(10));
    seed.commit()?;

    let before = database.snapshot(19)?;
    assert_eq!(before.ttl_set(b"members"), Ttl::RemainingMicros(1));
    assert!(database.sismember_latest_set_at(b"members", b"one", 19)?);
    assert_eq!(database.scard_latest_set_at(b"members", 19)?, 1);
    assert_eq!(
        database.ttl_latest_set(b"members", 19)?,
        Ttl::RemainingMicros(1)
    );

    let due = database.snapshot(20)?;
    assert_eq!(due.ttl_set(b"members"), Ttl::Missing);
    Ok(())
}

fn algebra_request(
    operation: SetAlgebraOperation,
    keys: &[&[u8]],
) -> Result<SetAlgebraRequest, Box<dyn std::error::Error>> {
    Ok(SetAlgebraRequest::try_new(
        operation,
        keys.iter().map(|key| key.to_vec()).collect(),
        64,
        1_024,
    )?)
}

#[test]
fn set_members_and_algebra_share_one_expiry_boundary() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    seed.create_set(b"left".to_vec())?;
    seed.create_set(b"right".to_vec())?;
    seed.sadd(b"left".to_vec(), b"a".to_vec())?;
    seed.sadd(b"left".to_vec(), b"b".to_vec())?;
    seed.sadd(b"right".to_vec(), b"b".to_vec())?;
    assert!(seed.expire_set(b"left".to_vec(), 20)?);
    assert!(seed.sadd(b"left".to_vec(), b"c".to_vec())?);
    assert!(seed.srem(b"left".to_vec(), b"a".to_vec())?);
    assert_eq!(seed.ttl_set(b"left"), Ttl::RemainingMicros(10));
    seed.commit()?;

    let union = algebra_request(SetAlgebraOperation::Union, &[b"left", b"right"])?;
    let intersection = algebra_request(SetAlgebraOperation::Intersection, &[b"left", b"right"])?;
    let difference = algebra_request(SetAlgebraOperation::Difference, &[b"left", b"right"])?;

    let before = database.snapshot(19)?;
    assert_eq!(before.scard(b"left")?, 2);
    assert_eq!(
        before.set_algebra(&union)?.members(),
        &[b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        before.set_algebra(&intersection)?.members(),
        &[b"b".to_vec()]
    );
    assert_eq!(before.set_algebra(&difference)?.members(), &[b"c".to_vec()]);

    let due = database.snapshot(20)?;
    assert_eq!(due.ttl_set(b"left"), Ttl::Missing);
    assert!(matches!(
        due.scard(b"left"),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    assert_eq!(due.set_algebra(&union)?.members(), &[b"b".to_vec()]);
    assert!(due.set_algebra(&intersection)?.members().is_empty());
    assert!(due.set_algebra(&difference)?.members().is_empty());
    assert_eq!(
        database.set_algebra_latest_at(&union, 20)?.members(),
        &[b"b".to_vec()]
    );
    assert!(matches!(
        database.sismember_latest_set_at(b"left", b"b", 20),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    Ok(())
}

#[test]
fn due_set_incarnations_reuse_every_structure_family_without_resurrection()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let keys: [&[u8]; 5] = [b"scalar", b"hash", b"set", b"list", b"sorted"];
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    for key in keys {
        seed.create_set(key.to_vec())?;
        seed.sadd(key.to_vec(), b"retired".to_vec())?;
        assert!(seed.expire_set(key.to_vec(), 20)?);
    }
    seed.commit()?;

    let historical = database.snapshot(19)?;
    for key in keys {
        assert!(historical.sismember(key, b"retired")?);
    }

    let mut reuse = database.begin(20, DurabilityClass::Strict)?;
    reuse.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    reuse.create_hash(b"hash".to_vec())?;
    reuse.hset(b"hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    reuse.create_set(b"set".to_vec())?;
    reuse.sadd(b"set".to_vec(), b"new".to_vec())?;
    reuse.create_list(b"list".to_vec())?;
    reuse.rpush(b"list".to_vec(), b"value".to_vec())?;
    reuse.create_sorted_set(b"sorted".to_vec())?;
    reuse.zadd(b"sorted".to_vec(), 1.0, b"member".to_vec())?;
    reuse.commit()?;

    assert!(historical.sismember(b"set", b"retired")?);
    let current = database.snapshot(20)?;
    assert_eq!(current.get(b"scalar"), Some(b"value".as_slice()));
    assert_eq!(current.hget(b"hash", b"field")?, Some(b"value".as_slice()));
    assert!(!current.sismember(b"set", b"retired")?);
    assert!(current.sismember(b"set", b"new")?);
    assert_eq!(current.lrange(b"list", 0, -1)?, vec![b"value".to_vec()]);
    assert_eq!(current.zscore(b"sorted", b"member")?, Some(1.0));
    drop(current);
    drop(historical);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.snapshot(20)?.get(b"scalar"),
        Some(b"value".as_slice())
    );
    assert!(!reopened.sismember_latest_set_at(b"set", b"retired", 20)?);
    assert!(reopened.sismember_latest_set_at(b"set", b"new", 20)?);
    Ok(())
}

#[test]
fn whole_set_expiry_conflicts_with_stale_members_and_rebases_after_admitted_members()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.commit()?;

    let mut stale_member = database.begin_optimistic(11, DurabilityClass::Strict)?;
    stale_member.sadd(b"members".to_vec(), b"stale".to_vec())?;
    let mut expiry = database.begin_optimistic(11, DurabilityClass::Strict)?;
    assert!(expiry.expire_set(b"members".to_vec(), 100)?);
    database.commit_optimistic(expiry)?;
    assert!(matches!(
        database.commit_optimistic(stale_member),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut admitted_member = database.begin_optimistic(12, DurabilityClass::Strict)?;
    admitted_member.sadd(b"members".to_vec(), b"admitted".to_vec())?;
    let mut later_expiry = database.begin_optimistic(12, DurabilityClass::Strict)?;
    assert!(later_expiry.expire_set(b"members".to_vec(), 200)?);
    database.commit_optimistic(admitted_member)?;
    database.commit_optimistic(later_expiry)?;

    assert_eq!(database.scard_latest_set_at(b"members", 13)?, 1);
    assert!(database.sismember_latest_set_at(b"members", b"admitted", 13)?);
    assert_eq!(
        database.ttl_latest_set(b"members", 13)?,
        Ttl::RemainingMicros(187)
    );
    Ok(())
}

#[test]
fn whole_set_expiry_commits_and_reopens_through_group_durability()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd(b"members".to_vec(), b"one".to_vec())?;
    seed.commit()?;

    let mut expiry = database.begin_optimistic(2, DurabilityClass::Group)?;
    assert!(expiry.expire_set(b"members".to_vec(), 20)?);
    let mut disjoint = database.begin_optimistic(2, DurabilityClass::Group)?;
    disjoint.set(b"cohort".to_vec(), b"value".to_vec(), None)?;
    let report = database.commit_group(vec![expiry, disjoint])?;
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
        database.ttl_latest_set(b"members", 3)?,
        Ttl::RemainingMicros(17)
    );
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.ttl_latest_set(b"members", 3)?,
        Ttl::RemainingMicros(17)
    );
    assert!(reopened.sismember_latest_set_at(b"members", b"one", 3)?);
    assert_eq!(
        reopened.get_latest_structure(b"cohort", 3)?,
        Some(b"value".to_vec())
    );
    Ok(())
}

#[test]
fn active_cleanup_retires_one_due_set_atomically_and_reopens()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd(b"members".to_vec(), b"a".to_vec())?;
    seed.sadd(b"members".to_vec(), b"b".to_vec())?;
    assert!(seed.expire_set(b"members".to_vec(), 20)?);
    seed.commit()?;

    let receipt = database.expire_due_structures(20, 1, DurabilityClass::Strict)?;
    assert_eq!(receipt.expired_keys, 1);
    assert!(!receipt.more_due);
    assert!(receipt.commit.is_some());
    assert_eq!(database.ttl_latest_set(b"members", 20)?, Ttl::Missing);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.ttl_latest_set(b"members", 20)?, Ttl::Missing);
    assert!(matches!(
        reopened.scard_latest_set_at(b"members", 20),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    Ok(())
}

#[test]
fn set_expiry_shares_the_global_cleanup_order_and_bound() -> Result<(), Box<dyn std::error::Error>>
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
    seed.create_hash(b"d-hash".to_vec())?;
    seed.hset(b"d-hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    assert!(seed.expire_hash_field(b"d-hash".to_vec(), b"field".to_vec(), 10)?);
    seed.commit()?;

    let first = database.expire_due_structures(10, 3, DurabilityClass::Memory)?;
    assert_eq!(first.expired_keys, 3);
    assert!(first.more_due);
    assert_eq!(database.get_latest_structure(b"a-scalar", i64::MIN)?, None);
    assert_eq!(database.ttl_latest_hash(b"b-hash", 10)?, Ttl::Missing);
    assert_eq!(database.ttl_latest_set(b"c-set", 10)?, Ttl::Missing);
    assert_eq!(
        database.ttl_latest_hash_field(b"d-hash", b"field", 9)?,
        Ttl::RemainingMicros(1)
    );

    let second = database.expire_due_structures(10, 1, DurabilityClass::Strict)?;
    assert_eq!(second.expired_keys, 1);
    assert!(!second.more_due);
    assert_eq!(
        database.ttl_latest_hash_field(b"d-hash", b"field", 10)?,
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
fn every_set_cleanup_boundary_recovers_due_or_complete_tombstones()
-> Result<(), Box<dyn std::error::Error>> {
    for boundary in commit_crash_boundaries() {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(1, DurabilityClass::Strict)?;
        seed.create_set(b"members".to_vec())?;
        seed.sadd(b"members".to_vec(), b"one".to_vec())?;
        assert!(seed.expire_set(b"members".to_vec(), 10)?);
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
            .get(&reopened.pages, &crate::structure_set_meta_key(b"members"))?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        let member = tree
            .get(
                &reopened.pages,
                &crate::structure_set_member_key(b"members", b"one")?,
            )?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        let marker = tree
            .get(
                &reopened.pages,
                &crate::structure_expiry_key(10, b"members")?,
            )?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        match reopened
            .recovery_report()
            .visible_csn
            .map(hyphae_native_types::Csn::get)
        {
            Some(1) => {
                let decoded = crate::decode_live_set_metadata(&metadata)?
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                assert_eq!(decoded.member_count, 1);
                assert_eq!(decoded.expires_at_micros, Some(10));
                assert!(crate::decode_set_member_value(&member)?);
                assert_eq!(marker, vec![crate::STRUCTURE_SET_EXPIRY_LIVE]);
            }
            Some(2) => {
                assert!(crate::is_structure_tombstone(&metadata));
                assert!(crate::is_structure_tombstone(&member));
                assert_eq!(marker, vec![crate::STRUCTURE_EXPIRY_TOMBSTONE]);
            }
            found => {
                return Err(format!(
                    "unexpected recovered set cleanup CSN {found:?} at {boundary:?}"
                )
                .into());
            }
        }
    }
    Ok(())
}

#[test]
fn expired_set_compacts_and_vacuums_without_resurrection() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_set(b"ephemeral".to_vec())?;
    seed.sadd(b"ephemeral".to_vec(), b"a".to_vec())?;
    seed.sadd(b"ephemeral".to_vec(), b"b".to_vec())?;
    assert!(seed.expire_set(b"ephemeral".to_vec(), 10)?);
    seed.commit()?;
    let historical = database.snapshot(9)?;

    let cleanup = database.expire_due_structures(10, 1, DurabilityClass::Strict)?;
    assert_eq!(cleanup.expired_keys, 1);
    let compaction = database.compact_structure(DurabilityClass::Strict)?;
    assert_eq!(compaction.dropped_tombstones, 4);
    assert!(compaction.commit.is_some());
    assert!(historical.sismember(b"ephemeral", b"a")?);
    assert!(historical.sismember(b"ephemeral", b"b")?);
    assert_eq!(database.ttl_latest_set(b"ephemeral", 10)?, Ttl::Missing);
    drop(historical);

    let vacuum = database.vacuum_pages()?;
    assert!(vacuum.applied);
    assert_eq!(database.ttl_latest_set(b"ephemeral", 10)?, Ttl::Missing);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.ttl_latest_set(b"ephemeral", 10)?, Ttl::Missing);
    assert!(matches!(
        reopened.scard_latest_set_at(b"ephemeral", 10),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    Ok(())
}

#[test]
fn malformed_set_expiry_metadata_and_markers_fail_complete_validation()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd(b"members".to_vec(), b"one".to_vec())?;
    assert!(seed.expire_set(b"members".to_vec(), 10)?);
    seed.commit()?;

    let root_set = database.coordinator.snapshot(2)?.roots().clone();
    let root = root_set
        .root(crate::SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let forgeries = [
        (
            crate::structure_set_meta_key(b"members"),
            crate::encode_set_metadata_state(crate::SetMetadata {
                member_count: 1,
                expires_at_micros: Some(11),
            }),
        ),
        (
            crate::structure_expiry_key(10, b"members")?,
            vec![crate::STRUCTURE_HASH_EXPIRY_LIVE],
        ),
        (
            crate::structure_expiry_key(10, b"members")?,
            vec![crate::STRUCTURE_EXPIRY_TOMBSTONE],
        ),
        (
            crate::structure_expiry_key(10, b"orphan")?,
            vec![crate::STRUCTURE_SET_EXPIRY_LIVE],
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
fn increment_reuses_a_due_set_as_a_scalar_without_resurrection()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_set(b"counter".to_vec())?;
    seed.sadd(b"counter".to_vec(), b"retired".to_vec())?;
    assert!(seed.expire_set(b"counter".to_vec(), 10)?);
    seed.commit()?;

    let mut reuse = database.begin(10, DurabilityClass::Strict)?;
    assert_eq!(reuse.increment_i64(b"counter".to_vec(), 7)?, 7);
    reuse.commit()?;

    assert_eq!(
        database.get_latest_structure(b"counter", 10)?,
        Some(b"7".to_vec())
    );
    assert!(matches!(
        database.scard_latest_set_at(b"counter", 10),
        Err(NativeRuntimeError::StructureKindMismatch)
    ));
    Ok(())
}
