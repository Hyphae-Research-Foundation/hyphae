// SPDX-License-Identifier: Apache-2.0

//! Bounded mixed-family physical-amplification evidence for G3.
//!
//! Resident-memory amplification is measured by the hosted workflow around
//! this isolated test process. This test owns deterministic page/WAL metrics.

use hyphae_native_pages::PAGE_SIZE;
use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

// Retained and compaction ceilings cover the maintained current generation.
// Append amplification remains explicit debt of the current COW mutation path.
const MAX_RETAINED_MILLI_X: u64 = 6_000;
const MAX_COMPACTION_MILLI_X: u64 = 4_000;
const MAX_APPEND_MILLI_X: u64 = 1_536_000;
const VALUE_BYTES: usize = 64;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

fn load_mixed_families(database: &mut NativeDatabase) -> TestResult<(u64, u64)> {
    let mut create = database.begin_optimistic(0, DurabilityClass::Strict)?;
    create.create_hash(b"hash".to_vec())?;
    create.create_set(b"set".to_vec())?;
    create.create_list(b"list".to_vec())?;
    create.create_sorted_set(b"sorted".to_vec())?;
    create.create_stream(b"stream".to_vec())?;
    database.commit_optimistic(create)?;

    let mut logical_bytes = 0_u64;
    for batch_id in 0..8_u64 {
        let mut batch =
            database.begin_optimistic(i64::try_from(batch_id + 1)?, DurabilityClass::Strict)?;
        for item in 0..16_u64 {
            let id = batch_id * 16 + item;
            let identity = format!("{id:03}").into_bytes();
            let scalar_key = [b"scalar-".as_slice(), identity.as_slice()].concat();
            let value = vec![u8::try_from(id % 251)?; VALUE_BYTES];
            batch.set(scalar_key.clone(), value.clone(), None)?;
            batch.hset(b"hash".to_vec(), identity.clone(), value.clone())?;
            batch.sadd(b"set".to_vec(), identity.clone())?;
            batch.rpush(b"list".to_vec(), value.clone())?;
            batch.zadd(
                b"sorted".to_vec(),
                f64::from(u32::try_from(id)?),
                identity.clone(),
            )?;
            batch.xadd(b"stream".to_vec(), &[(identity.clone(), value)])?;
            logical_bytes +=
                u64::try_from(scalar_key.len() + VALUE_BYTES * 4 + identity.len() * 4 + 16)?;
        }
        database.commit_optimistic(batch)?;
    }
    Ok((logical_bytes, logical_bytes))
}

fn churn_five_families(
    database: &mut NativeDatabase,
    logical_live_bytes: &mut u64,
) -> TestResult<()> {
    for batch_id in 0..4_u64 {
        let mut churn =
            database.begin_optimistic(i64::try_from(100 + batch_id)?, DurabilityClass::Strict)?;
        for item in 0..16_u64 {
            let id = batch_id * 16 + item;
            let identity = format!("{id:03}").into_bytes();
            let scalar_key = [b"scalar-".as_slice(), identity.as_slice()].concat();
            churn.delete_structure(scalar_key.clone())?;
            churn.hdelete(b"hash".to_vec(), identity.clone())?;
            churn.srem(b"set".to_vec(), identity.clone())?;
            churn.lpop(b"list".to_vec())?;
            churn.zrem(b"sorted".to_vec(), identity.clone())?;
            let removed =
                u64::try_from(scalar_key.len() + VALUE_BYTES * 3 + identity.len() * 3 + 8)?;
            *logical_live_bytes = logical_live_bytes
                .checked_sub(removed)
                .ok_or("logical live byte accounting underflowed")?;
        }
        database.commit_optimistic(churn)?;
    }
    Ok(())
}

#[test]
fn mixed_structure_workload_has_bounded_physical_amplification()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-g3-amplification-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let baseline = database.physical_observation()?;
    let (mut logical_live_bytes, logical_mutation_bytes) = load_mixed_families(&mut database)?;
    let loaded = database.physical_observation()?;
    churn_five_families(&mut database, &mut logical_live_bytes)?;
    let churned = database.physical_observation()?;
    database
        .snapshot(i64::MAX)
        .map_err(|error| format!("post-churn state validation failed: {error}"))?;
    let compaction = database
        .compact_structure(DurabilityClass::Memory)
        .map_err(|error| format!("structure compaction failed: {error}"))?;
    let vacuum = database
        .vacuum_pages()
        .map_err(|error| format!("page vacuum failed: {error}"))?;
    let maintained = database.physical_observation()?;

    let baseline_page_bytes = baseline.page_count * u64::try_from(PAGE_SIZE)?;
    let retained_page_bytes = maintained
        .page_count
        .saturating_mul(u64::try_from(PAGE_SIZE)?)
        .saturating_sub(baseline_page_bytes);
    let appended_page_bytes = loaded
        .page_count
        .saturating_sub(baseline.page_count)
        .saturating_mul(u64::try_from(PAGE_SIZE)?);
    let appended_wal_bytes = loaded.wal_bytes.saturating_sub(baseline.wal_bytes);
    let retained_milli_x = retained_page_bytes.saturating_mul(1_000) / logical_live_bytes;
    let append_milli_x = appended_page_bytes
        .saturating_add(appended_wal_bytes)
        .saturating_mul(1_000)
        / logical_mutation_bytes.max(1);
    let compaction_milli_x = compaction
        .pages_appended
        .saturating_mul(u64::try_from(PAGE_SIZE)?)
        .saturating_mul(1_000)
        / logical_live_bytes.max(1);

    assert!(logical_live_bytes > 0);
    assert!(churned.page_count >= loaded.page_count);
    println!(
        "G3_AMPLIFICATION: retained_milli_x={retained_milli_x} compaction_milli_x={compaction_milli_x} append_milli_x={append_milli_x} logical_live_bytes={logical_live_bytes} page_bytes={retained_page_bytes} wal_bytes={appended_wal_bytes} compacted_entries={} vacuum_before_pages={} vacuum_after_pages={} families=6 rss=hosted-external",
        compaction.retained_entries, vacuum.previous_page_count, vacuum.active_page_count,
    );
    assert!(retained_milli_x <= MAX_RETAINED_MILLI_X);
    assert!(compaction_milli_x <= MAX_COMPACTION_MILLI_X);
    assert!(append_milli_x <= MAX_APPEND_MILLI_X);

    drop(database);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
