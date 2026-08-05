// SPDX-License-Identifier: Apache-2.0

//! Deterministic scalar/counter and set model equivalence for G3.

use std::collections::BTreeSet;

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

#[test]
fn seeded_scalar_counter_trace_matches_model() -> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-scalar-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut model = 0_i64;
    for step in 1..=128_i64 {
        let delta = (step % 11) - 5;
        let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
        model = model
            .checked_add(delta)
            .ok_or("bounded counter model overflow")?;
        assert_eq!(batch.increment_i64(b"counter".to_vec(), delta)?, model);
        database.commit_optimistic(batch)?;
        assert_eq!(
            database.get_latest_structure(b"counter", i64::MIN)?,
            Some(model.to_string().into_bytes())
        );
        if step % 16 == 0 {
            drop(database);
            database = NativeDatabase::open(&temporary)?;
        }
    }
    drop(database);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn seeded_set_trace_matches_model_across_reopen() -> Result<(), Box<dyn std::error::Error>> {
    for seed in 1..=4_u64 {
        let temporary =
            std::env::temp_dir().join(format!("hyphae-set-model-{}-{seed}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temporary);
        let mut database = NativeDatabase::create(&temporary)?;
        let mut create = database.begin_optimistic(0, DurabilityClass::Strict)?;
        create.create_set(b"members".to_vec())?;
        database.commit_optimistic(create)?;
        let mut model = BTreeSet::<Vec<u8>>::new();
        let mut random = seed;
        for step in 1..=96_i64 {
            let member = (next_u64(&mut random) % 32).to_be_bytes().to_vec();
            let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
            let changed = if random % 2 == 0 {
                let expected = model.insert(member.clone());
                assert_eq!(batch.sadd(b"members".to_vec(), member)?, expected);
                expected
            } else {
                let expected = model.remove(&member);
                assert_eq!(batch.srem(b"members".to_vec(), member)?, expected);
                expected
            };
            if changed {
                database.commit_optimistic(batch)?;
            }
            assert_eq!(database.scard_latest_set(b"members")?, model.len());
            if step % 16 == 0 {
                drop(database);
                database = NativeDatabase::open(&temporary)?;
            }
        }
        drop(database);
        std::fs::remove_dir_all(&temporary)?;
    }
    Ok(())
}
