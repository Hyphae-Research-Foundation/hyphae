// SPDX-License-Identifier: Apache-2.0

//! Bounded mixed-family physical-amplification evidence for G3.
//!
//! Resident-memory amplification is measured by the hosted workflow around
//! this isolated test process. This test owns deterministic page/WAL metrics.

use hyphae_native_pages::PAGE_SIZE;
use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

// Initial regression ceilings reflect the append-only COW implementation.
// They are deliberately recorded as debt, not production targets.
const MAX_RETAINED_MILLI_X: u64 = 2_048_000;
const MAX_APPEND_MILLI_X: u64 = 1_536_000;

#[test]
fn mixed_structure_workload_has_bounded_physical_amplification()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-g3-amplification-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let baseline = database.physical_observation()?;
    let mut logical_live_bytes = 0_u64;
    let mut logical_mutation_bytes = 0_u64;

    let mut create = database.begin_optimistic(0, DurabilityClass::Strict)?;
    create.create_hash(b"hash".to_vec())?;
    create.create_set(b"set".to_vec())?;
    create.create_list(b"list".to_vec())?;
    create.create_sorted_set(b"sorted".to_vec())?;
    create.create_stream(b"stream".to_vec())?;
    database.commit_optimistic(create)?;

    for batch_id in 0..8_u64 {
        let mut batch =
            database.begin_optimistic(i64::try_from(batch_id + 1)?, DurabilityClass::Strict)?;
        for item in 0..16_u64 {
            let id = batch_id * 16 + item;
            let identity = format!("{id:03}").into_bytes();
            let scalar_key = [b"scalar-".as_slice(), identity.as_slice()].concat();
            let value = vec![u8::try_from(id % 251)?; 64];
            batch.set(scalar_key.clone(), value.clone(), None)?;
            batch.hset(b"hash".to_vec(), identity.clone(), value.clone())?;
            batch.sadd(b"set".to_vec(), identity.clone())?;
            batch.rpush(b"list".to_vec(), value.clone())?;
            batch.zadd(
                b"sorted".to_vec(),
                f64::from(u32::try_from(id)?),
                identity.clone(),
            )?;
            batch.xadd(b"stream".to_vec(), &[(identity.clone(), value.clone())])?;
            let item_logical_bytes = u64::try_from(
                scalar_key.len()
                    + value.len()
                    + identity.len()
                    + value.len()
                    + identity.len()
                    + value.len()
                    + identity.len()
                    + 8
                    + identity.len()
                    + value.len()
                    + 8,
            )?;
            logical_live_bytes += item_logical_bytes;
            logical_mutation_bytes += item_logical_bytes;
        }
        database.commit_optimistic(batch)?;
    }
    let loaded = database.physical_observation()?;

    for batch_id in 0..4_u64 {
        let mut churn =
            database.begin_optimistic(i64::try_from(100 + batch_id)?, DurabilityClass::Strict)?;
        for item in 0..16_u64 {
            let id = batch_id * 16 + item;
            let identity = format!("{id:03}").into_bytes();
            let scalar_key = [b"scalar-".as_slice(), identity.as_slice()].concat();
            churn.delete_structure(scalar_key)?;
            churn.hdelete(b"hash".to_vec(), identity.clone())?;
            churn.srem(b"set".to_vec(), identity.clone())?;
            churn.lpop(b"list".to_vec())?;
            churn.zrem(b"sorted".to_vec(), identity)?;
        }
        database.commit_optimistic(churn)?;
    }
    let churned = database.physical_observation()?;
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

    assert!(logical_live_bytes > 0);
    assert!(churned.page_count >= loaded.page_count);
    println!(
        "G3_AMPLIFICATION: retained_milli_x={retained_milli_x} append_milli_x={append_milli_x} logical_live_bytes={logical_live_bytes} page_bytes={retained_page_bytes} wal_bytes={appended_wal_bytes} families=6 rss=hosted-external"
    );
    assert!(retained_milli_x <= MAX_RETAINED_MILLI_X);
    assert!(append_milli_x <= MAX_APPEND_MILLI_X);

    drop(database);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
