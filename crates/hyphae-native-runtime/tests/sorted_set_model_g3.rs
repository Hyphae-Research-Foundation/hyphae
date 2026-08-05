// SPDX-License-Identifier: Apache-2.0

//! Deterministic sorted-set model equivalence for G3.

use std::collections::BTreeMap;

use hyphae_native_runtime::{NativeDatabase, ZAddOutcome};
use hyphae_native_types::DurabilityClass;

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

#[test]
fn seeded_sorted_set_trace_matches_model_across_reopen() -> Result<(), Box<dyn std::error::Error>> {
    for seed in 1..=4_u64 {
        let temporary =
            std::env::temp_dir().join(format!("hyphae-zset-model-{}-{seed}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temporary);
        let mut database = NativeDatabase::create(&temporary)?;
        let mut create = database.begin_optimistic(0, DurabilityClass::Strict)?;
        create.create_sorted_set(b"scores".to_vec())?;
        database.commit_optimistic(create)?;
        let mut model = BTreeMap::<Vec<u8>, f64>::new();
        let mut random = seed;
        for step in 1..=96_i64 {
            let member = (next_u64(&mut random) % 24).to_be_bytes().to_vec();
            let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
            let should_commit = if random % 4 == 0 {
                let expected = model.remove(&member).is_some();
                assert_eq!(batch.zrem(b"scores".to_vec(), member)?, expected);
                expected
            } else {
                let score = f64::from((next_u64(&mut random) % 1_000) as u32) / 10.0;
                let expected = match model.insert(member.clone(), score) {
                    None => ZAddOutcome::Added,
                    Some(previous) if previous.to_bits() == score.to_bits() => {
                        ZAddOutcome::Unchanged
                    }
                    Some(_) => ZAddOutcome::Updated,
                };
                assert_eq!(batch.zadd(b"scores".to_vec(), score, member)?, expected);
                expected != ZAddOutcome::Unchanged
            };
            if should_commit {
                database.commit_optimistic(batch)?;
            }
            let mut expected = model
                .iter()
                .map(|(member, score)| (member.clone(), *score))
                .collect::<Vec<_>>();
            expected.sort_by(|left, right| {
                left.1
                    .total_cmp(&right.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            let actual = database.zrange_latest_sorted_set(b"scores", 0, -1)?;
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected.iter()) {
                assert_eq!(
                    (actual.member(), actual.score()),
                    (expected.0.as_slice(), expected.1)
                );
            }
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
