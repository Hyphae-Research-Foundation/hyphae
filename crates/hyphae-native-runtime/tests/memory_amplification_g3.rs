// SPDX-License-Identifier: Apache-2.0

//! Bounded physical-amplification receipt for the G3 structure engine.

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

#[test]
fn bounded_structure_workload_has_finite_declared_amplification()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-g3-amplification-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let before = database.physical_observation()?;
    let mut logical_bytes = 0_u64;
    let mut batch = database.begin_optimistic(0, DurabilityClass::Strict)?;
    for batch_id in 0..16_u64 {
        for item in 0..16_u64 {
            let key = format!("key-{batch_id:02}-{item:02}").into_bytes();
            let byte = u8::try_from(item)?;
            let value = vec![byte; 64];
            logical_bytes += u64::try_from(key.len() + value.len())?;
            batch.set(key, value, None)?;
        }
    }
    database.commit_optimistic(batch)?;
    let after = database.physical_observation()?;
    let page_bytes = after.page_count.saturating_sub(before.page_count) * 16_384;
    let wal_bytes = after.wal_bytes.saturating_sub(before.wal_bytes);
    let physical_bytes = page_bytes.saturating_add(wal_bytes);
    assert!(logical_bytes > 0);
    assert!(physical_bytes >= logical_bytes);
    let amplification_milli = physical_bytes.saturating_mul(1_000) / logical_bytes;
    assert!(
        amplification_milli <= 400_000,
        "bounded COW append amplification was {amplification_milli} milli-x"
    );
    drop(database);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
