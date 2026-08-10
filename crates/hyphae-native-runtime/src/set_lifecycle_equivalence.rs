// SPDX-License-Identifier: GPL-3.0-only

use std::{
    error::Error,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_types::{Csn, DurabilityClass, EngineKind};

use crate::{
    CommitBoundary, NativeDatabase, NativeRuntimeError, Ttl,
    wal_codec::{Mutation, Opcode},
};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-set-lifecycle-{}-{}",
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
fn whole_set_delete_recreates_without_retired_members_and_preserves_history()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd_many(
        b"members".to_vec(),
        vec![b"retired-a".to_vec(), b"retired-b".to_vec()],
    )?;
    assert!(seed.expire_set(b"members".to_vec(), 1_000)?);
    seed.commit()?;
    let historical = database.snapshot(11)?;

    let mut replace = database.begin(20, DurabilityClass::Strict)?;
    assert!(replace.delete_set(b"members".to_vec())?);
    assert!(matches!(
        replace.scard(b"members"),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    replace.create_set(b"members".to_vec())?;
    replace.sadd_many(b"members".to_vec(), vec![b"current".to_vec()])?;
    replace.commit()?;

    assert_eq!(
        historical.sscan(b"members", None, 10)?,
        [b"retired-a".to_vec(), b"retired-b".to_vec()]
    );
    assert_eq!(
        database.sscan_latest_set_at(b"members", None, 10, 21)?,
        [b"current".to_vec()]
    );
    assert_eq!(database.ttl_latest_set(b"members", 21)?, Ttl::Persistent);
    let roots = database.coordinator.snapshot(21)?;
    let root = roots
        .roots()
        .root(crate::SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let tree = hyphae_native_btree::BTree::from_root(root);
    assert!(crate::is_structure_tombstone(
        &tree
            .get(
                &database.pages,
                &crate::structure_set_member_key(b"members", b"retired-a")?,
            )?
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
    ));
    assert_eq!(
        tree.get(
            &database.pages,
            &crate::structure_expiry_key(1_000, b"members")?,
        )?,
        Some(vec![crate::STRUCTURE_EXPIRY_TOMBSTONE])
    );
    drop(historical);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.sscan_latest_set_at(b"members", None, 10, 21)?,
        [b"current".to_vec()]
    );
    Ok(())
}

#[test]
fn whole_set_delete_handles_missing_due_empty_and_wrong_families_without_side_effects()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"empty".to_vec())?;
    seed.create_set(b"due".to_vec())?;
    seed.sadd(b"due".to_vec(), b"retired".to_vec())?;
    assert!(seed.expire_set(b"due".to_vec(), 20)?);
    seed.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    seed.create_hash(b"hash".to_vec())?;
    seed.create_list(b"list".to_vec())?;
    seed.create_sorted_set(b"sorted".to_vec())?;
    seed.commit()?;

    let mut delete = database.begin_optimistic(20, DurabilityClass::Strict)?;
    assert!(delete.mutations.is_empty());
    assert!(!delete.delete_set(b"missing".to_vec())?);
    assert!(!delete.delete_set(b"due".to_vec())?);
    assert!(delete.mutations.is_empty());
    assert!(delete.delete_set(b"empty".to_vec())?);
    assert!(!delete.delete_set(b"empty".to_vec())?);
    assert_eq!(delete.mutations.len(), 1);
    for key in [
        b"scalar".as_slice(),
        b"hash".as_slice(),
        b"list".as_slice(),
        b"sorted".as_slice(),
    ] {
        assert!(matches!(
            delete.delete_set(key.to_vec()),
            Err(NativeRuntimeError::StructureKindMismatch)
        ));
    }
    assert_eq!(delete.mutations.len(), 1);
    database.commit_optimistic(delete)?;

    assert!(matches!(
        database.scard_latest_set_at(b"empty", 21),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    assert_eq!(database.ttl_latest_set(b"due", 21)?, Ttl::Missing);
    assert_eq!(
        database.get_latest_structure(b"scalar", 21)?,
        Some(b"value".to_vec())
    );
    assert_eq!(database.hlen_latest_hash_at(b"hash", 21)?, 0);
    assert_eq!(database.llen_latest_list(b"list")?, 0);
    assert_eq!(database.zcard_latest_sorted_set(b"sorted")?, 0);
    Ok(())
}

#[test]
fn member_mutations_prepared_before_delete_cannot_survive_retirement() -> Result<(), Box<dyn Error>>
{
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd_many(
        b"members".to_vec(),
        vec![b"keep".to_vec(), b"remove".to_vec()],
    )?;
    seed.commit()?;

    let mut delete = database.begin(11, DurabilityClass::Strict)?;
    assert_eq!(
        delete.sadd_many(
            b"members".to_vec(),
            vec![b"temporary-a".to_vec(), b"temporary-b".to_vec()],
        )?,
        2
    );
    assert_eq!(
        delete.srem_many(b"members".to_vec(), vec![b"remove".to_vec()])?,
        1
    );
    assert!(delete.delete_set(b"members".to_vec())?);
    assert!(matches!(
        delete.scard(b"members"),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    delete.commit()?;

    assert!(matches!(
        database.sscan_latest_set_at(b"members", None, 10, 12),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    drop(database);
    let reopened = NativeDatabase::open(&path)?;
    assert!(matches!(
        reopened.scard_latest_set_at(b"members", 12),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    Ok(())
}

#[test]
fn deleted_sets_recreate_as_every_native_structure_family_without_resurrection()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let keys: [&[u8]; 5] = [b"scalar", b"hash", b"set", b"list", b"sorted"];
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    for key in keys {
        seed.create_set(key.to_vec())?;
        seed.sadd(key.to_vec(), b"retired".to_vec())?;
        assert!(seed.expire_set(key.to_vec(), 1_000)?);
    }
    seed.commit()?;
    let historical = database.snapshot(11)?;

    let mut recreate = database.begin(12, DurabilityClass::Strict)?;
    for key in keys {
        assert!(recreate.delete_set(key.to_vec())?);
    }
    recreate.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    recreate.create_hash(b"hash".to_vec())?;
    recreate.hset(b"hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    recreate.create_set(b"set".to_vec())?;
    recreate.sadd(b"set".to_vec(), b"current".to_vec())?;
    recreate.create_list(b"list".to_vec())?;
    recreate.rpush(b"list".to_vec(), b"value".to_vec())?;
    recreate.create_sorted_set(b"sorted".to_vec())?;
    recreate.zadd(b"sorted".to_vec(), 1.5, b"member".to_vec())?;
    recreate.commit()?;

    for key in keys {
        assert!(historical.sismember(key, b"retired")?);
    }
    let current = database.snapshot(13)?;
    assert_eq!(current.get(b"scalar"), Some(b"value".as_slice()));
    assert_eq!(current.hget(b"hash", b"field")?, Some(b"value".as_slice()));
    assert!(!current.sismember(b"set", b"retired")?);
    assert!(current.sismember(b"set", b"current")?);
    assert_eq!(current.lrange(b"list", 0, -1)?, [b"value".to_vec()]);
    assert_eq!(current.zscore(b"sorted", b"member")?, Some(1.5));
    drop(current);
    drop(historical);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.get_latest_structure(b"scalar", 13)?,
        Some(b"value".to_vec())
    );
    assert_eq!(
        reopened.hget_latest_hash_at(b"hash", b"field", 13)?,
        Some(b"value".to_vec())
    );
    assert!(!reopened.sismember_latest_set_at(b"set", b"retired", 13)?);
    assert!(reopened.sismember_latest_set_at(b"set", b"current", 13)?);
    assert_eq!(
        reopened.snapshot(13)?.lrange(b"list", 0, -1)?,
        [b"value".to_vec()]
    );
    assert_eq!(
        reopened.snapshot(13)?.zscore(b"sorted", b"member")?,
        Some(1.5)
    );
    Ok(())
}

#[test]
fn set_lifecycle_fence_rejects_stale_members_rebases_and_serializes_retirement()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.commit()?;

    let mut stale_member = database.begin_optimistic(11, DurabilityClass::Strict)?;
    stale_member.sadd(b"members".to_vec(), b"stale".to_vec())?;
    let mut deletion = database.begin_optimistic(11, DurabilityClass::Strict)?;
    assert!(deletion.delete_set(b"members".to_vec())?);
    database.commit_optimistic(deletion)?;
    assert!(matches!(
        database.commit_optimistic(stale_member),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut recreate = database.begin(12, DurabilityClass::Strict)?;
    recreate.create_set(b"members".to_vec())?;
    recreate.commit()?;
    let mut admitted_member = database.begin_optimistic(13, DurabilityClass::Strict)?;
    admitted_member.sadd(b"members".to_vec(), b"admitted".to_vec())?;
    let mut later_delete = database.begin_optimistic(13, DurabilityClass::Strict)?;
    assert!(later_delete.delete_set(b"members".to_vec())?);
    database.commit_optimistic(admitted_member)?;
    database.commit_optimistic(later_delete)?;
    assert!(matches!(
        database.scard_latest_set(b"members"),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));

    let mut recreate = database.begin(14, DurabilityClass::Strict)?;
    recreate.create_set(b"members".to_vec())?;
    recreate.commit()?;
    let mut first_delete = database.begin_optimistic(15, DurabilityClass::Strict)?;
    let mut second_delete = database.begin_optimistic(15, DurabilityClass::Strict)?;
    assert!(first_delete.delete_set(b"members".to_vec())?);
    assert!(second_delete.delete_set(b"members".to_vec())?);
    database.commit_optimistic(first_delete)?;
    assert!(matches!(
        database.commit_optimistic(second_delete),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut recreate = database.begin(16, DurabilityClass::Strict)?;
    recreate.create_set(b"members".to_vec())?;
    recreate.commit()?;
    let mut prior_incarnation = database.begin_optimistic(17, DurabilityClass::Strict)?;
    prior_incarnation.sadd(b"members".to_vec(), b"prior".to_vec())?;
    let mut replace = database.begin_optimistic(17, DurabilityClass::Strict)?;
    assert!(replace.delete_set(b"members".to_vec())?);
    replace.create_set(b"members".to_vec())?;
    replace.sadd(b"members".to_vec(), b"replacement".to_vec())?;
    database.commit_optimistic(replace)?;
    assert!(matches!(
        database.commit_optimistic(prior_incarnation),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    assert_eq!(
        database.sscan_latest_set_at(b"members", None, 10, 18)?,
        [b"replacement".to_vec()]
    );
    Ok(())
}

fn commit_boundaries() -> [CommitBoundary; 7] {
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
fn every_set_delete_boundary_recovers_prior_or_complete_retirement() -> Result<(), Box<dyn Error>> {
    for boundary in commit_boundaries() {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_set(b"members".to_vec())?;
        seed.sadd_many(
            b"members".to_vec(),
            vec![b"alpha".to_vec(), b"beta".to_vec()],
        )?;
        assert!(seed.expire_set(b"members".to_vec(), 1_000)?);
        seed.commit()?;

        let mut delete = database.begin(20, DurabilityClass::Strict)?;
        assert!(delete.delete_set(b"members".to_vec())?);
        assert!(matches!(
            delete.commit_with_interruption(boundary),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        match reopened
            .recovery_report()
            .visible_csn
            .map(hyphae_native_types::Csn::get)
        {
            Some(1) => {
                assert_eq!(
                    reopened.sscan_latest_set_at(b"members", None, 10, 21)?,
                    [b"alpha".to_vec(), b"beta".to_vec()]
                );
                assert_eq!(
                    reopened.ttl_latest_set(b"members", 21)?,
                    Ttl::RemainingMicros(979)
                );
            }
            Some(2) => {
                assert!(matches!(
                    reopened.scard_latest_set_at(b"members", 21),
                    Err(NativeRuntimeError::UnknownStructureSet)
                ));
                assert_eq!(reopened.ttl_latest_set(b"members", 21)?, Ttl::Missing);
            }
            found => {
                return Err(format!(
                    "unexpected recovered set deletion CSN {found:?} at {boundary:?}"
                )
                .into());
            }
        }
    }
    Ok(())
}

#[test]
fn every_set_replacement_boundary_recovers_prior_or_complete_incarnation()
-> Result<(), Box<dyn Error>> {
    for boundary in commit_boundaries() {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_set(b"members".to_vec())?;
        seed.sadd(b"members".to_vec(), b"prior".to_vec())?;
        seed.commit()?;

        let mut replace = database.begin(20, DurabilityClass::Strict)?;
        assert!(replace.delete_set(b"members".to_vec())?);
        replace.create_set(b"members".to_vec())?;
        replace.sadd(b"members".to_vec(), b"replacement".to_vec())?;
        assert!(matches!(
            replace.commit_with_interruption(boundary),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        match reopened
            .recovery_report()
            .visible_csn
            .map(hyphae_native_types::Csn::get)
        {
            Some(1) => assert_eq!(
                reopened.sscan_latest_set_at(b"members", None, 10, 21)?,
                [b"prior".to_vec()]
            ),
            Some(2) => assert_eq!(
                reopened.sscan_latest_set_at(b"members", None, 10, 21)?,
                [b"replacement".to_vec()]
            ),
            found => {
                return Err(format!(
                    "unexpected recovered set replacement CSN {found:?} at {boundary:?}"
                )
                .into());
            }
        }
    }
    Ok(())
}

#[test]
fn deleted_set_compacts_and_vacuums_without_resurrection() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd_many(
        b"members".to_vec(),
        vec![b"alpha".to_vec(), b"beta".to_vec()],
    )?;
    assert!(seed.expire_set(b"members".to_vec(), 1_000)?);
    seed.commit()?;
    let historical = database.snapshot(11)?;

    let mut delete = database.begin(20, DurabilityClass::Strict)?;
    assert!(delete.delete_set(b"members".to_vec())?);
    delete.commit()?;
    let compaction = database.compact_structure(DurabilityClass::Strict)?;
    assert_eq!(compaction.dropped_tombstones, 4);
    assert!(compaction.commit.is_some());
    assert_eq!(
        historical.sscan(b"members", None, 10)?,
        [b"alpha".to_vec(), b"beta".to_vec()]
    );
    assert!(matches!(
        database.scard_latest_set_at(b"members", 21),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    drop(historical);

    let vacuum = database.vacuum_pages()?;
    assert!(vacuum.applied);
    assert!(matches!(
        database.scard_latest_set_at(b"members", 21),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert!(matches!(
        reopened.scard_latest_set_at(b"members", 21),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    assert_eq!(reopened.ttl_latest_set(b"members", 21)?, Ttl::Missing);
    Ok(())
}

fn delete_set_mutation(key: &[u8]) -> Mutation {
    Mutation {
        engine: EngineKind::Structure,
        opcode: Opcode::DeleteSet,
        target: None,
        key: key.to_vec(),
        value: Vec::new(),
        expires_at_micros: None,
    }
}

#[test]
fn physical_set_delete_rejects_metadata_member_count_envelope_and_expiry_corruption()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd(b"members".to_vec(), b"one".to_vec())?;
    assert!(seed.expire_set(b"members".to_vec(), 1_000)?);
    seed.commit()?;

    let roots = database.coordinator.snapshot(11)?;
    let root = roots
        .roots()
        .root(crate::SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let metadata_key = crate::structure_set_meta_key(b"members");
    let member_key = crate::structure_set_member_key(b"members", b"one")?;
    let expiry_key = crate::structure_expiry_key(1_000, b"members")?;
    let forgeries = [
        (metadata_key.clone(), vec![0xff]),
        (metadata_key.clone(), crate::structure_tombstone_value()),
        (
            metadata_key,
            crate::encode_set_metadata_state(crate::SetMetadata {
                member_count: 2,
                expires_at_micros: Some(1_000),
            }),
        ),
        (member_key, vec![0xff]),
        (expiry_key.clone(), vec![crate::STRUCTURE_HASH_EXPIRY_LIVE]),
        (expiry_key, vec![crate::STRUCTURE_EXPIRY_TOMBSTONE]),
    ];
    let mutation = delete_set_mutation(b"members");
    for (key, value) in forgeries {
        let forged = hyphae_native_btree::BTree::from_root(root)
            .upsert(&mut database.pages, Csn::new(1)?, key, value)?
            .tree;
        assert!(matches!(
            crate::delete_set_in_tree(&mut database.pages, forged, Csn::new(2)?, &mutation),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
    }

    assert!(matches!(
        crate::decode_set_scan_member_identity(
            &[crate::STRUCTURE_SET_MEMBER_PREFIX, 0, 0],
            b"members"
        ),
        Err(NativeRuntimeError::InvalidStructureTree)
    ));
    let wrong_set = crate::structure_set_member_key(b"other", b"one")?;
    assert!(matches!(
        crate::decode_set_scan_member_identity(&wrong_set, b"members"),
        Err(NativeRuntimeError::InvalidStructureTree)
    ));
    Ok(())
}

#[test]
fn physical_set_delete_rejects_reached_page_corruption() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd(b"members".to_vec(), b"one".to_vec())?;
    seed.commit()?;
    let root = database
        .coordinator
        .snapshot(11)?
        .roots()
        .root(crate::SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;

    let offset = root
        .get()
        .checked_sub(1)
        .and_then(|slot| slot.checked_mul(u64::try_from(hyphae_native_pages::PAGE_SIZE).ok()?))
        .and_then(|page| page.checked_add(100))
        .ok_or("page corruption offset overflow")?;
    let mut pages = OpenOptions::new()
        .read(true)
        .write(true)
        .open(temporary.path().join("pages.hydb"))?;
    pages.seek(SeekFrom::Start(offset))?;
    let mut byte = [0_u8; 1];
    pages.read_exact(&mut byte)?;
    byte[0] ^= 1;
    pages.seek(SeekFrom::Start(offset))?;
    pages.write_all(&byte)?;
    pages.sync_all()?;

    assert!(matches!(
        crate::delete_set_in_tree(
            &mut database.pages,
            hyphae_native_btree::BTree::from_root(root),
            Csn::new(2)?,
            &delete_set_mutation(b"members"),
        ),
        Err(NativeRuntimeError::BTree(_))
    ));
    Ok(())
}
