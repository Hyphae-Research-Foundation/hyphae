// SPDX-License-Identifier: Apache-2.0

//! Cross-family G3 atomicity, controlled-expiry, and restart matrices.

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError};
use hyphae_native_types::DurabilityClass;

fn temporary(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hyphae-g3-{name}-{}", std::process::id()))
}

fn seed_all_families(
    database: &mut NativeDatabase,
    expiry: Option<i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut batch = database.begin_optimistic(1, DurabilityClass::Strict)?;
    batch.set(b"scalar".to_vec(), b"one".to_vec(), expiry)?;
    batch.create_hash(b"hash".to_vec())?;
    batch.hset(b"hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    batch.create_set(b"set".to_vec())?;
    batch.sadd(b"set".to_vec(), b"member".to_vec())?;
    batch.create_list(b"list".to_vec())?;
    batch.rpush(b"list".to_vec(), b"item".to_vec())?;
    batch.create_sorted_set(b"sorted".to_vec())?;
    batch.zadd(b"sorted".to_vec(), 1.5, b"ranked".to_vec())?;
    batch.create_stream(b"stream".to_vec())?;
    batch.xadd(
        b"stream".to_vec(),
        &[(b"field".to_vec(), b"event".to_vec())],
    )?;
    database.commit_optimistic(batch)?;
    if let Some(expires_at) = expiry {
        let mut batch = database.begin_optimistic(2, DurabilityClass::Strict)?;
        batch.expire_structure(b"scalar".to_vec(), expires_at)?;
        batch.expire_hash(b"hash".to_vec(), expires_at)?;
        batch.expire_set(b"set".to_vec(), expires_at)?;
        batch.expire_list(b"list".to_vec(), expires_at)?;
        batch.expire_sorted_set(b"sorted".to_vec(), expires_at)?;
        batch.expire_stream(b"stream".to_vec(), expires_at)?;
        database.commit_optimistic(batch)?;
    }
    Ok(())
}

fn assert_seeded(database: &NativeDatabase) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        database.get_latest_structure(b"scalar", 0)?,
        Some(b"one".to_vec())
    );
    assert_eq!(
        database.hget_latest_hash(b"hash", b"field")?,
        Some(b"value".to_vec())
    );
    assert_eq!(database.scard_latest_set(b"set")?, 1);
    assert_eq!(
        database.lrange_latest_list(b"list", 0, -1)?,
        vec![b"item".to_vec()]
    );
    let sorted = database.zrange_latest_sorted_set(b"sorted", 0, -1)?;
    assert_eq!(sorted.len(), 1);
    assert_eq!(
        (sorted[0].member(), sorted[0].score()),
        (b"ranked".as_slice(), 1.5)
    );
    assert_eq!(
        database.xrange_latest_stream(b"stream", 0, u64::MAX, 8)?,
        vec![(1, vec![(b"field".to_vec(), b"event".to_vec())])]
    );
    Ok(())
}

#[test]
fn all_structure_families_match_after_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("restart-matrix");
    let _ = std::fs::remove_dir_all(&path);
    let mut database = NativeDatabase::create(&path)?;
    seed_all_families(&mut database, None)?;
    assert_seeded(&database)?;
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_seeded(&reopened)?;
    drop(reopened);
    std::fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn all_family_batch_is_atomic_on_conflict() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("atomic-matrix");
    let _ = std::fs::remove_dir_all(&path);
    let mut database = NativeDatabase::create(&path)?;
    seed_all_families(&mut database, None)?;

    let mut winner = database.begin_optimistic(2, DurabilityClass::Strict)?;
    let mut loser = database.begin_optimistic(2, DurabilityClass::Strict)?;
    winner.set(b"scalar".to_vec(), b"winner".to_vec(), None)?;
    loser.set(b"scalar".to_vec(), b"loser".to_vec(), None)?;
    loser.hset(b"hash".to_vec(), b"loser".to_vec(), b"value".to_vec())?;
    loser.sadd(b"set".to_vec(), b"loser".to_vec())?;
    loser.rpush(b"list".to_vec(), b"loser".to_vec())?;
    loser.zadd(b"sorted".to_vec(), 9.0, b"loser".to_vec())?;
    loser.xadd(
        b"stream".to_vec(),
        &[(b"field".to_vec(), b"loser".to_vec())],
    )?;
    database.commit_optimistic(winner)?;
    assert!(matches!(
        database.commit_optimistic(loser),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    assert_eq!(
        database.get_latest_structure(b"scalar", 0)?,
        Some(b"winner".to_vec())
    );
    assert_eq!(database.hget_latest_hash(b"hash", b"loser")?, None);
    assert_eq!(database.scard_latest_set(b"set")?, 1);
    assert_eq!(database.lrange_latest_list(b"list", 0, -1)?.len(), 1);
    assert_eq!(database.zcard_latest_sorted_set(b"sorted")?, 1);
    assert_eq!(
        database
            .xrange_latest_stream(b"stream", 0, u64::MAX, 8)?
            .len(),
        1
    );
    drop(database);
    std::fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn controlled_expiry_covers_every_structure_family() -> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("ttl-matrix");
    let _ = std::fs::remove_dir_all(&path);
    let mut database = NativeDatabase::create(&path)?;
    seed_all_families(&mut database, Some(10))?;

    assert_eq!(database.get_latest_structure(b"scalar", 10)?, None);
    assert!(matches!(
        database.hget_latest_hash_at(b"hash", b"field", 10),
        Err(NativeRuntimeError::UnknownStructureHash)
    ));
    assert!(matches!(
        database.sscan_latest_set_at(b"set", None, 8, 10),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    assert!(matches!(
        database.lrange_latest_list_at(b"list", 0, -1, 10),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    assert!(matches!(
        database.zcard_latest_sorted_set_at(b"sorted", 10),
        Err(NativeRuntimeError::UnknownStructureSortedSet)
    ));
    assert!(matches!(
        database.xrange_latest_stream_at(b"stream", 0, u64::MAX, 8, 10),
        Err(NativeRuntimeError::UnknownStructureStream)
    ));
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(reopened.get_latest_structure(b"scalar", 10)?, None);
    assert!(matches!(
        reopened.xrange_latest_stream_at(b"stream", 0, u64::MAX, 8, 10),
        Err(NativeRuntimeError::UnknownStructureStream)
    ));
    drop(reopened);
    std::fs::remove_dir_all(path)?;
    Ok(())
}
