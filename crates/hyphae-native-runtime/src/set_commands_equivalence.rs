// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeSet,
    error::Error,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_types::{Csn, DurabilityClass};

use crate::{CommitBoundary, MAX_SET_MEMBER_BATCH_SIZE, NativeDatabase, NativeRuntimeError, Ttl};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-set-commands-{}-{}",
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
fn set_member_commands_match_private_snapshot_physical_and_reopen() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    assert_eq!(
        seed.sadd_many(
            b"members".to_vec(),
            vec![b"b".to_vec(), Vec::new(), b"a".to_vec(), vec![0xff]],
        )?,
        4
    );
    assert_eq!(
        seed.smismember(
            b"members",
            &[b"a".to_vec(), b"missing".to_vec(), b"a".to_vec()],
        )?,
        vec![true, false, true]
    );
    assert_eq!(
        seed.sscan(b"members", None, 3)?,
        vec![Vec::new(), b"a".to_vec(), b"b".to_vec()]
    );
    assert_eq!(
        seed.srem_many(
            b"members".to_vec(),
            vec![b"missing".to_vec(), b"b".to_vec()],
        )?,
        1
    );
    seed.commit()?;

    let snapshot = database.snapshot(11)?;
    assert_eq!(
        snapshot.smismember(
            b"members",
            &[Vec::new(), b"b".to_vec(), vec![0xff], Vec::new()],
        )?,
        vec![true, false, true, true]
    );
    assert_eq!(
        snapshot.sscan(b"members", Some(b"".as_slice()), 8)?,
        vec![b"a".to_vec(), vec![0xff]]
    );
    assert_eq!(
        database.smismember_latest_set_at(
            b"members",
            &[Vec::new(), b"a".to_vec(), b"b".to_vec(), vec![0xff]],
            11,
        )?,
        vec![true, true, false, true]
    );
    assert_eq!(
        database.sscan_latest_set_at(b"members", Some(b"a".as_slice()), 8, 11)?,
        vec![vec![0xff]]
    );
    drop(snapshot);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.sscan_latest_set_at(b"members", None, 8, 11)?,
        vec![Vec::new(), b"a".to_vec(), vec![0xff]]
    );
    Ok(())
}

#[test]
fn set_member_inputs_are_bounded_canonical_and_failure_atomic() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd_many(
        b"members".to_vec(),
        vec![b"present".to_vec(), b"retire".to_vec()],
    )?;
    seed.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    seed.commit()?;

    let mut update = database.begin_optimistic(11, DurabilityClass::Strict)?;
    assert!(update.mutations.is_empty());
    assert_eq!(update.sadd_many(b"members".to_vec(), Vec::new())?, 0);
    assert_eq!(update.srem_many(b"members".to_vec(), Vec::new())?, 0);
    assert!(update.smismember(b"members", &[])?.is_empty());
    assert!(update.mutations.is_empty());

    assert!(matches!(
        update.sadd_many(
            b"members".to_vec(),
            vec![b"duplicate".to_vec(), b"duplicate".to_vec()],
        ),
        Err(NativeRuntimeError::DuplicateSetMember)
    ));
    assert!(matches!(
        update.srem_many(
            b"members".to_vec(),
            vec![b"present".to_vec(), b"present".to_vec()],
        ),
        Err(NativeRuntimeError::DuplicateSetMember)
    ));
    let too_many = vec![Vec::new(); MAX_SET_MEMBER_BATCH_SIZE + 1];
    assert!(matches!(
        update.smismember(b"members", &too_many),
        Err(NativeRuntimeError::SetMemberBatchTooLarge { .. })
    ));
    assert!(matches!(
        update.sadd_many(b"members".to_vec(), too_many),
        Err(NativeRuntimeError::SetMemberBatchTooLarge { .. })
    ));

    let oversized = vec![b'x'; hyphae_native_btree::BTREE_MAX_KEY_SIZE];
    assert!(matches!(
        update.sadd_many(b"members".to_vec(), vec![oversized.clone()]),
        Err(NativeRuntimeError::StructureIdentityTooLarge)
    ));
    assert!(matches!(
        update.srem_many(b"members".to_vec(), vec![oversized.clone()]),
        Err(NativeRuntimeError::StructureIdentityTooLarge)
    ));
    assert!(matches!(
        update.sadd(b"members".to_vec(), oversized.clone()),
        Err(NativeRuntimeError::StructureIdentityTooLarge)
    ));
    assert!(matches!(
        update.srem(b"members".to_vec(), oversized),
        Err(NativeRuntimeError::StructureIdentityTooLarge)
    ));
    assert!(matches!(
        update.sadd_many(b"scalar".to_vec(), Vec::new()),
        Err(NativeRuntimeError::StructureKindMismatch)
    ));
    assert!(matches!(
        update.srem_many(b"missing".to_vec(), Vec::new()),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    assert!(update.mutations.is_empty());
    assert_eq!(update.scard(b"members")?, 2);

    assert_eq!(
        update.sadd_many(
            b"members".to_vec(),
            vec![b"zeta".to_vec(), b"beta".to_vec(), b"middle".to_vec()],
        )?,
        3
    );
    let prepared = update
        .mutations
        .iter()
        .map(|mutation| {
            let (_, member) = crate::decode_set_member_identity(&mutation.key)?;
            Ok(member.to_vec())
        })
        .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
    assert_eq!(
        prepared,
        [b"beta".to_vec(), b"middle".to_vec(), b"zeta".to_vec()]
    );
    assert_eq!(
        update.smismember(
            b"members",
            &[
                b"present".to_vec(),
                b"missing".to_vec(),
                b"present".to_vec()
            ],
        )?,
        [true, false, true]
    );
    update.rollback();
    Ok(())
}

#[test]
fn set_member_commands_preserve_ttl_and_validate_zero_limit() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"expiring".to_vec())?;
    seed.sadd_many(b"expiring".to_vec(), vec![b"a".to_vec(), b"b".to_vec()])?;
    assert!(seed.expire_set(b"expiring".to_vec(), 100)?);
    seed.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    seed.commit()?;

    let mut update = database.begin(20, DurabilityClass::Strict)?;
    assert_eq!(
        update.sadd_many(b"expiring".to_vec(), vec![b"b".to_vec(), b"c".to_vec()],)?,
        1
    );
    assert_eq!(
        update.srem_many(
            b"expiring".to_vec(),
            vec![b"a".to_vec(), b"missing".to_vec()],
        )?,
        1
    );
    assert_eq!(update.ttl_set(b"expiring"), Ttl::RemainingMicros(80));
    assert_eq!(
        update.sscan(b"expiring", None, 10)?,
        [b"b".to_vec(), b"c".to_vec()]
    );
    update.commit()?;

    assert!(
        database
            .sscan_latest_set_at(b"expiring", None, 0, 99)?
            .is_empty()
    );
    assert_eq!(
        database.sscan_latest_set_at(b"expiring", None, 10, 99)?,
        [b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        database.ttl_latest_set(b"expiring", 99)?,
        Ttl::RemainingMicros(1)
    );
    assert!(matches!(
        database.smismember_latest_set_at(b"expiring", &[b"b".to_vec()], 100),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    assert!(matches!(
        database.sscan_latest_set_at(b"expiring", None, 0, 100),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    assert!(matches!(
        database.sscan_latest_set_at(b"missing", None, 0, 99),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    assert!(matches!(
        database.sscan_latest_set_at(b"scalar", None, 0, 99),
        Err(NativeRuntimeError::StructureKindMismatch)
    ));
    Ok(())
}

#[test]
fn set_member_batches_rebase_disjoint_writes_and_conflict_atomically() -> Result<(), Box<dyn Error>>
{
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd_many(
        b"members".to_vec(),
        vec![b"alpha".to_vec(), b"retire".to_vec()],
    )?;
    seed.commit()?;

    let mut left = database.begin_optimistic(11, DurabilityClass::Strict)?;
    let mut right = database.begin_optimistic(11, DurabilityClass::Strict)?;
    left.sadd_many(
        b"members".to_vec(),
        vec![b"beta".to_vec(), b"gamma".to_vec()],
    )?;
    right.sadd_many(
        b"members".to_vec(),
        vec![b"delta".to_vec(), b"epsilon".to_vec()],
    )?;
    database.commit_optimistic(left)?;
    database.commit_optimistic(right)?;

    let mut winner = database.begin_optimistic(12, DurabilityClass::Strict)?;
    let mut loser = database.begin_optimistic(12, DurabilityClass::Strict)?;
    winner.srem_many(b"members".to_vec(), vec![b"alpha".to_vec()])?;
    loser.sadd_many(b"members".to_vec(), vec![b"not-published".to_vec()])?;
    loser.srem_many(b"members".to_vec(), vec![b"alpha".to_vec()])?;
    database.commit_optimistic(winner)?;
    assert!(matches!(
        database.commit_optimistic(loser),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    assert!(!database.sismember_latest_set(b"members", b"not-published")?);

    let mut stale = database.begin_optimistic(13, DurabilityClass::Strict)?;
    stale.sadd_many(b"members".to_vec(), vec![b"stale".to_vec()])?;
    let mut expiry = database.begin_optimistic(13, DurabilityClass::Strict)?;
    assert!(expiry.expire_set(b"members".to_vec(), 100)?);
    database.commit_optimistic(expiry)?;
    assert!(matches!(
        database.commit_optimistic(stale),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    assert!(!database.sismember_latest_set_at(b"members", b"stale", 14)?);
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
fn every_set_member_batch_boundary_recovers_prior_or_complete_state() -> Result<(), Box<dyn Error>>
{
    for boundary in commit_boundaries() {
        let temporary = TestDirectory::new();
        let mut database = NativeDatabase::create(temporary.path())?;
        let mut seed = database.begin(10, DurabilityClass::Strict)?;
        seed.create_set(b"members".to_vec())?;
        seed.sadd_many(
            b"members".to_vec(),
            vec![b"keep".to_vec(), b"retire".to_vec()],
        )?;
        assert!(seed.expire_set(b"members".to_vec(), 1_000)?);
        seed.commit()?;

        let mut update = database.begin(20, DurabilityClass::Strict)?;
        update.sadd_many(
            b"members".to_vec(),
            vec![b"alpha".to_vec(), b"beta".to_vec()],
        )?;
        update.srem_many(b"members".to_vec(), vec![b"retire".to_vec()])?;
        assert!(matches!(
            update.commit_with_interruption(boundary),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(temporary.path())?;
        let members = reopened.sscan_latest_set_at(b"members", None, 10, 21)?;
        match reopened
            .recovery_report()
            .visible_csn
            .map(hyphae_native_types::Csn::get)
        {
            Some(1) => {
                assert_eq!(members, [b"keep".to_vec(), b"retire".to_vec()]);
            }
            Some(2) => {
                assert_eq!(
                    members,
                    [b"alpha".to_vec(), b"beta".to_vec(), b"keep".to_vec()]
                );
            }
            found => {
                return Err(format!(
                    "unexpected recovered set member batch CSN {found:?} at {boundary:?}"
                )
                .into());
            }
        }
        assert_eq!(
            reopened.ttl_latest_set(b"members", 21)?,
            Ttl::RemainingMicros(979)
        );
    }
    Ok(())
}

#[test]
fn multilevel_set_scan_prunes_tombstones_and_fails_closed() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(100, DurabilityClass::Memory)?;
    seed.create_set(b"large".to_vec())?;
    seed.sadd_many(
        b"large".to_vec(),
        (0..2_048_u32)
            .map(|index| index.to_be_bytes().to_vec())
            .collect(),
    )?;
    seed.commit()?;
    assert!(database.latest_structure_tree_height()? >= 2);

    let mut remove = database.begin(102, DurabilityClass::Strict)?;
    assert_eq!(
        remove.srem_many(b"large".to_vec(), vec![1_025_u32.to_be_bytes().to_vec()],)?,
        1
    );
    remove.commit()?;
    let expected = [1_024_u32, 1_026, 1_027, 1_028]
        .into_iter()
        .map(|index| index.to_be_bytes().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(
        database.sscan_latest_set_at(b"large", Some(1_023_u32.to_be_bytes().as_slice()), 4, 103,)?,
        expected
    );

    let root = database
        .coordinator
        .snapshot(103)?
        .roots()
        .root(crate::SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let invalid_value = hyphae_native_btree::BTree::from_root(root)
        .upsert(
            &mut database.pages,
            Csn::new(3)?,
            crate::structure_set_member_key(b"large", &1_024_u32.to_be_bytes())?,
            vec![0xff],
        )?
        .tree;
    assert!(matches!(
        database.set_scan_in_tree_at(
            invalid_value,
            b"large",
            Some(1_023_u32.to_be_bytes().as_slice()),
            1,
            103,
        ),
        Err(NativeRuntimeError::InvalidStructureTree)
    ));

    let invalid_count = hyphae_native_btree::BTree::from_root(root)
        .upsert(
            &mut database.pages,
            Csn::new(3)?,
            crate::structure_set_meta_key(b"large"),
            crate::encode_set_metadata(0),
        )?
        .tree;
    assert!(matches!(
        database.set_scan_in_tree_at(invalid_count, b"large", None, 1, 103),
        Err(NativeRuntimeError::InvalidStructureTree)
    ));
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    assert_eq!(
        reopened.sscan_latest_set_at(b"large", Some(1_023_u32.to_be_bytes().as_slice()), 4, 103,)?,
        expected
    );
    Ok(())
}

#[test]
fn randomized_set_member_commands_match_independent_ordered_model() -> Result<(), Box<dyn Error>> {
    fn next_random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn member(value: u64) -> Vec<u8> {
        if value == 0 {
            Vec::new()
        } else {
            u16::try_from(value)
                .unwrap_or_default()
                .to_be_bytes()
                .to_vec()
        }
    }

    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut create = database.begin(1, DurabilityClass::Strict)?;
    create.create_set(b"model".to_vec())?;
    create.commit()?;

    let mut oracle = BTreeSet::new();
    let mut random = 0x4d59_5048_4145_u64;
    for step in 0..96_i64 {
        let mut requested = BTreeSet::new();
        let width = usize::try_from(next_random(&mut random) % 8 + 1)?;
        while requested.len() < width {
            requested.insert(member(next_random(&mut random) % 64));
        }
        let requested = requested.into_iter().collect::<Vec<_>>();
        let mut batch = database.begin(step + 2, DurabilityClass::Memory)?;
        if next_random(&mut random) & 1 == 0 {
            let expected = requested
                .iter()
                .filter(|value| oracle.insert((*value).clone()))
                .count();
            assert_eq!(
                batch.sadd_many(b"model".to_vec(), requested.clone())?,
                expected
            );
        } else {
            let expected = requested
                .iter()
                .filter(|value| oracle.remove(value.as_slice()))
                .count();
            assert_eq!(
                batch.srem_many(b"model".to_vec(), requested.clone())?,
                expected
            );
        }
        let expected_members = oracle.iter().cloned().collect::<Vec<_>>();
        assert_eq!(batch.sscan(b"model", None, 4_096)?, expected_members);
        if batch.mutations.is_empty() {
            batch.rollback();
        } else {
            batch.commit()?;
        }

        if step % 8 == 0 {
            let mut probes = (0..20_u64).map(member).collect::<Vec<_>>();
            probes.push(member(3));
            probes.push(member(3));
            let expected_membership = probes
                .iter()
                .map(|value| oracle.contains(value))
                .collect::<Vec<_>>();
            let snapshot = database.snapshot(step + 2)?;
            assert_eq!(snapshot.smismember(b"model", &probes)?, expected_membership);
            assert_eq!(
                database.smismember_latest_set_at(b"model", &probes, step + 2)?,
                expected_membership
            );

            let mut paged = Vec::new();
            let mut cursor: Option<Vec<u8>> = None;
            loop {
                let page =
                    database.sscan_latest_set_at(b"model", cursor.as_deref(), 7, step + 2)?;
                if page.is_empty() {
                    break;
                }
                cursor = page.last().cloned();
                paged.extend(page);
            }
            assert_eq!(paged, oracle.iter().cloned().collect::<Vec<_>>());
        }
    }
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.sscan_latest_set_at(b"model", None, 4_096, 200)?,
        oracle.into_iter().collect::<Vec<_>>()
    );
    Ok(())
}
