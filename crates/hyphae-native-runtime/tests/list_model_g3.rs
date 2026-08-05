// SPDX-License-Identifier: Apache-2.0

//! Deterministic list model equivalence for G3.

use std::collections::VecDeque;

use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

#[test]
fn seeded_list_trace_matches_model_across_reopen() -> Result<(), Box<dyn std::error::Error>> {
    for seed in 1..=4_u64 {
        let temporary =
            std::env::temp_dir().join(format!("hyphae-list-model-{}-{seed}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temporary);
        let mut database = NativeDatabase::create(&temporary)?;
        let mut create = database.begin_optimistic(0, DurabilityClass::Strict)?;
        create.create_list(b"items".to_vec())?;
        database.commit_optimistic(create)?;
        let mut model = VecDeque::<Vec<u8>>::new();
        let mut random = seed;
        for step in 1..=96_i64 {
            let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
            match next_u64(&mut random) % 4 {
                0 => {
                    let value = random.to_be_bytes().to_vec();
                    assert_eq!(
                        batch.lpush(b"items".to_vec(), value.clone())?,
                        model.len() + 1
                    );
                    model.push_front(value);
                }
                1 => {
                    let value = random.to_be_bytes().to_vec();
                    assert_eq!(
                        batch.rpush(b"items".to_vec(), value.clone())?,
                        model.len() + 1
                    );
                    model.push_back(value);
                }
                2 if !model.is_empty() => {
                    assert_eq!(batch.lpop(b"items".to_vec())?, model.pop_front());
                }
                3 if !model.is_empty() => {
                    assert_eq!(batch.rpop(b"items".to_vec())?, model.pop_back());
                }
                _ => continue,
            }
            database.commit_optimistic(batch)?;
            assert_eq!(
                database.lrange_latest_list(b"items", 0, -1)?,
                model.iter().cloned().collect::<Vec<_>>()
            );
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
