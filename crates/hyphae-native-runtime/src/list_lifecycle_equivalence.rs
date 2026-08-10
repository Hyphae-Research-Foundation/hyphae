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
    CommitBoundary, NativeDatabase, NativeRuntimeError,
    wal_codec::{Mutation, Opcode},
};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-list-lifecycle-{}-{}",
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
fn whole_list_delete_recreates_without_retired_elements_and_preserves_history()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"retired-a".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"retired-b".to_vec())?;
    seed.commit()?;
    let historical = database.snapshot(11)?;

    let mut replace = database.begin(20, DurabilityClass::Strict)?;
    assert!(replace.delete_list(b"queue".to_vec())?);
    assert!(matches!(
        replace.llen(b"queue"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    replace.create_list(b"queue".to_vec())?;
    replace.rpush(b"queue".to_vec(), b"current".to_vec())?;
    replace.commit()?;

    assert_eq!(
        historical.lrange(b"queue", 0, -1)?,
        [b"retired-a".to_vec(), b"retired-b".to_vec()]
    );
    assert_eq!(
        database.snapshot(21)?.lrange(b"queue", 0, -1)?,
        [b"current".to_vec()]
    );
    drop(historical);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.snapshot(21)?.lrange(b"queue", 0, -1)?,
        [b"current".to_vec()]
    );
    Ok(())
}

#[test]
fn whole_list_delete_handles_missing_empty_and_wrong_families_without_side_effects()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"empty".to_vec())?;
    seed.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    seed.create_hash(b"hash".to_vec())?;
    seed.create_set(b"set".to_vec())?;
    seed.create_sorted_set(b"sorted".to_vec())?;
    seed.commit()?;

    let mut delete = database.begin_optimistic(11, DurabilityClass::Strict)?;
    assert!(delete.mutations.is_empty());
    assert!(!delete.delete_list(b"missing".to_vec())?);
    assert!(delete.mutations.is_empty());
    assert!(delete.delete_list(b"empty".to_vec())?);
    assert!(!delete.delete_list(b"empty".to_vec())?);
    assert_eq!(delete.mutations.len(), 1);
    for key in [
        b"scalar".as_slice(),
        b"hash".as_slice(),
        b"set".as_slice(),
        b"sorted".as_slice(),
    ] {
        assert!(matches!(
            delete.delete_list(key.to_vec()),
            Err(NativeRuntimeError::StructureKindMismatch)
        ));
    }
    assert_eq!(delete.mutations.len(), 1);
    database.commit_optimistic(delete)?;

    assert!(matches!(
        database.llen_latest_list(b"empty"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    assert_eq!(
        database.get_latest_structure(b"scalar", 12)?,
        Some(b"value".to_vec())
    );
    assert_eq!(database.hlen_latest_hash(b"hash")?, 0);
    assert_eq!(database.scard_latest_set(b"set")?, 0);
    assert_eq!(database.zcard_latest_sorted_set(b"sorted")?, 0);
    Ok(())
}

#[test]
fn list_mutations_prepared_before_delete_cannot_survive_retirement() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"keep".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"remove".to_vec())?;
    seed.commit()?;

    let mut delete = database.begin(11, DurabilityClass::Strict)?;
    assert_eq!(
        delete.lpush(b"queue".to_vec(), b"temporary-head".to_vec())?,
        3
    );
    assert_eq!(delete.rpop(b"queue".to_vec())?, Some(b"remove".to_vec()));
    assert!(delete.delete_list(b"queue".to_vec())?);
    assert!(matches!(
        delete.llen(b"queue"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    delete.commit()?;

    assert!(matches!(
        database.llen_latest_list(b"queue"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    drop(database);
    let reopened = NativeDatabase::open(&path)?;
    assert!(matches!(
        reopened.llen_latest_list(b"queue"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    Ok(())
}

#[test]
fn deleted_lists_recreate_as_every_native_structure_family_without_resurrection()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let keys: [&[u8]; 5] = [b"scalar", b"hash", b"set", b"list", b"sorted"];
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    for key in keys {
        seed.create_list(key.to_vec())?;
        seed.rpush(key.to_vec(), b"retired".to_vec())?;
    }
    seed.commit()?;
    let historical = database.snapshot(11)?;

    let mut recreate = database.begin(12, DurabilityClass::Strict)?;
    for key in keys {
        assert!(recreate.delete_list(key.to_vec())?);
    }
    recreate.set(b"scalar".to_vec(), b"value".to_vec(), None)?;
    recreate.create_hash(b"hash".to_vec())?;
    recreate.hset(b"hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    recreate.create_set(b"set".to_vec())?;
    recreate.sadd(b"set".to_vec(), b"member".to_vec())?;
    recreate.create_list(b"list".to_vec())?;
    recreate.rpush(b"list".to_vec(), b"current".to_vec())?;
    recreate.create_sorted_set(b"sorted".to_vec())?;
    recreate.zadd(b"sorted".to_vec(), 1.5, b"member".to_vec())?;
    recreate.commit()?;

    for key in keys {
        assert_eq!(historical.lrange(key, 0, -1)?, [b"retired".to_vec()]);
    }
    let current = database.snapshot(13)?;
    assert_eq!(current.get(b"scalar"), Some(b"value".as_slice()));
    assert_eq!(current.hget(b"hash", b"field")?, Some(b"value".as_slice()));
    assert!(current.sismember(b"set", b"member")?);
    assert_eq!(current.lrange(b"list", 0, -1)?, [b"current".to_vec()]);
    assert_eq!(current.zscore(b"sorted", b"member")?, Some(1.5));
    drop(current);
    drop(historical);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.snapshot(13)?.lrange(b"list", 0, -1)?,
        [b"current".to_vec()]
    );
    Ok(())
}

#[test]
fn whole_list_identity_serializes_writers_deletes_and_recreation() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"seed".to_vec())?;
    seed.commit()?;

    let mut stale_push = database.begin_optimistic(11, DurabilityClass::Strict)?;
    stale_push.rpush(b"queue".to_vec(), b"stale".to_vec())?;
    let mut deletion = database.begin_optimistic(11, DurabilityClass::Strict)?;
    assert!(deletion.delete_list(b"queue".to_vec())?);
    database.commit_optimistic(deletion)?;
    assert!(matches!(
        database.commit_optimistic(stale_push),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut recreate = database.begin(12, DurabilityClass::Strict)?;
    recreate.create_list(b"queue".to_vec())?;
    recreate.commit()?;
    let mut admitted_push = database.begin_optimistic(13, DurabilityClass::Strict)?;
    admitted_push.rpush(b"queue".to_vec(), b"admitted".to_vec())?;
    let mut stale_delete = database.begin_optimistic(13, DurabilityClass::Strict)?;
    assert!(stale_delete.delete_list(b"queue".to_vec())?);
    database.commit_optimistic(admitted_push)?;
    assert!(matches!(
        database.commit_optimistic(stale_delete),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut first_delete = database.begin_optimistic(14, DurabilityClass::Strict)?;
    let mut second_delete = database.begin_optimistic(14, DurabilityClass::Strict)?;
    assert!(first_delete.delete_list(b"queue".to_vec())?);
    assert!(second_delete.delete_list(b"queue".to_vec())?);
    database.commit_optimistic(first_delete)?;
    assert!(matches!(
        database.commit_optimistic(second_delete),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut recreate = database.begin(15, DurabilityClass::Strict)?;
    recreate.create_list(b"queue".to_vec())?;
    recreate.commit()?;
    let mut prior = database.begin_optimistic(16, DurabilityClass::Strict)?;
    prior.rpush(b"queue".to_vec(), b"prior".to_vec())?;
    let mut replacement = database.begin_optimistic(16, DurabilityClass::Strict)?;
    assert!(replacement.delete_list(b"queue".to_vec())?);
    replacement.create_list(b"queue".to_vec())?;
    replacement.rpush(b"queue".to_vec(), b"replacement".to_vec())?;
    database.commit_optimistic(replacement)?;
    assert!(matches!(
        database.commit_optimistic(prior),
        Err(NativeRuntimeError::WriteConflict(_))
    ));
    assert_eq!(
        database.snapshot(17)?.lrange(b"queue", 0, -1)?,
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
fn every_list_delete_and_replacement_boundary_recovers_one_complete_incarnation()
-> Result<(), Box<dyn Error>> {
    for replace in [false, true] {
        for boundary in commit_boundaries() {
            let temporary = TestDirectory::new();
            let mut database = NativeDatabase::create(temporary.path())?;
            let mut seed = database.begin(10, DurabilityClass::Strict)?;
            seed.create_list(b"queue".to_vec())?;
            seed.rpush(b"queue".to_vec(), b"prior".to_vec())?;
            seed.commit()?;

            let mut delete = database.begin(20, DurabilityClass::Strict)?;
            assert!(delete.delete_list(b"queue".to_vec())?);
            if replace {
                delete.create_list(b"queue".to_vec())?;
                delete.rpush(b"queue".to_vec(), b"replacement".to_vec())?;
            }
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
                Some(1) => assert_eq!(
                    reopened.snapshot(21)?.lrange(b"queue", 0, -1)?,
                    [b"prior".to_vec()]
                ),
                Some(2) if replace => assert_eq!(
                    reopened.snapshot(21)?.lrange(b"queue", 0, -1)?,
                    [b"replacement".to_vec()]
                ),
                Some(2) => assert!(matches!(
                    reopened.llen_latest_list(b"queue"),
                    Err(NativeRuntimeError::UnknownStructureList)
                )),
                found => {
                    return Err(format!(
                        "unexpected recovered list lifecycle CSN {found:?} at {boundary:?}"
                    )
                    .into());
                }
            }
        }
    }
    Ok(())
}

#[test]
fn deleted_multichunk_blob_list_compacts_vacuums_and_collects_without_resurrection()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let blob = vec![0x41; 9_000];
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), blob.clone())?;
    for index in 0..1_024_u32 {
        seed.rpush(b"queue".to_vec(), index.to_be_bytes().to_vec())?;
    }
    seed.commit()?;
    let historical = database.snapshot(11)?;

    let mut delete = database.begin(20, DurabilityClass::Strict)?;
    assert!(delete.delete_list(b"queue".to_vec())?);
    delete.commit()?;
    let compaction = database.compact_structure(DurabilityClass::Strict)?;
    assert!(compaction.dropped_tombstones >= 2);
    assert_eq!(historical.llen(b"queue")?, 1_025);
    assert_eq!(historical.lrange(b"queue", 0, 0)?, [blob]);
    drop(historical);

    let vacuum = database.vacuum_pages()?;
    assert!(vacuum.applied);
    database.checkpoint()?;
    database.truncate_wal_at_retention_checkpoint()?;
    let collection = database.collect_blobs()?;
    assert!(collection.removed_files >= 1);
    assert!(matches!(
        database.llen_latest_list(b"queue"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert!(matches!(
        reopened.llen_latest_list(b"queue"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    Ok(())
}

fn delete_list_mutation(key: &[u8]) -> Mutation {
    Mutation {
        engine: EngineKind::Structure,
        opcode: Opcode::DeleteList,
        target: None,
        key: key.to_vec(),
        value: Vec::new(),
        expires_at_micros: None,
    }
}

#[test]
fn physical_list_delete_rejects_metadata_chunk_gap_envelope_and_identity_corruption()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    for index in 0..130_u32 {
        seed.rpush(b"queue".to_vec(), index.to_be_bytes().to_vec())?;
    }
    seed.commit()?;

    let root = database
        .coordinator
        .snapshot(11)?
        .roots()
        .root(crate::SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;
    let tree = hyphae_native_btree::BTree::from_root(root);
    let metadata_key = crate::structure_list_meta_key(b"queue")?;
    let encoded_metadata = tree
        .get(&database.pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let metadata = crate::decode_list_metadata(&encoded_metadata)?;
    assert!(metadata.head_chunk < metadata.tail_chunk);
    let forgeries = [
        (metadata_key.clone(), vec![0xff]),
        (metadata_key.clone(), crate::structure_tombstone_value()),
        (
            metadata_key,
            crate::encode_list_metadata(crate::ListMetadata {
                length: metadata.length + 1,
                ..metadata
            }),
        ),
        (
            crate::structure_list_chunk_key(b"queue", metadata.head_chunk)?,
            crate::structure_tombstone_value(),
        ),
        (
            crate::structure_list_chunk_key(b"queue", metadata.tail_chunk)?,
            vec![0xff],
        ),
    ];
    let mutation = delete_list_mutation(b"queue");
    for (key, value) in forgeries {
        let forged = tree
            .upsert(&mut database.pages, Csn::new(1)?, key, value)?
            .tree;
        assert!(matches!(
            crate::delete_list_in_tree(&mut database.pages, forged, Csn::new(2)?, &mutation),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
    }

    assert!(matches!(
        crate::decode_list_chunk_identity(&[0, 0, 0]),
        Err(NativeRuntimeError::InvalidStructureTree)
    ));
    let wrong_list = crate::structure_list_chunk_key(b"other", 0)?;
    let identity = wrong_list
        .get(1..)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let (list, _) = crate::decode_list_chunk_identity(identity)?;
    assert_ne!(list, b"queue");
    Ok(())
}

#[test]
fn physical_list_delete_rejects_reached_page_corruption() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"value".to_vec())?;
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
        crate::delete_list_in_tree(
            &mut database.pages,
            hyphae_native_btree::BTree::from_root(root),
            Csn::new(2)?,
            &delete_list_mutation(b"queue"),
        ),
        Err(NativeRuntimeError::BTree(_))
    ));
    Ok(())
}
