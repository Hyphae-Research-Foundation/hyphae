// SPDX-License-Identifier: AGPL-3.0-only

//! Deterministic model-based stream equivalence for G3.

use std::collections::BTreeMap;

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError};
use hyphae_native_types::DurabilityClass;

type Fields = Vec<(Vec<u8>, Vec<u8>)>;

#[derive(Clone, Default)]
struct Model {
    exists: bool,
    entries: BTreeMap<u64, Fields>,
    next: u64,
    expiry: Option<i64>,
}

impl Model {
    fn create(&mut self) {
        self.exists = true;
        self.entries.clear();
        self.next = 0;
        self.expiry = None;
    }

    fn append(&mut self, fields: Fields) -> u64 {
        self.next += 1;
        self.entries.insert(self.next, fields);
        self.next
    }

    fn delete(&mut self) -> bool {
        if !self.exists {
            return false;
        }
        self.exists = false;
        self.entries.clear();
        self.expiry = None;
        true
    }

    fn expire(&mut self, expiry: i64) -> bool {
        if !self.exists {
            return false;
        }
        self.expiry = Some(expiry);
        true
    }

    fn visible(&self, now: i64) -> bool {
        self.exists && self.expiry.is_none_or(|expiry| expiry > now)
    }

    fn range(&self, start: u64, end: u64, limit: usize) -> Vec<(u64, Fields)> {
        if start > end {
            return Vec::new();
        }
        self.entries
            .range(start..=end)
            .take(limit)
            .map(|(id, fields)| (*id, fields.clone()))
            .collect()
    }
}

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

fn fields_for(step: i64, random: &mut u64) -> Fields {
    let count = usize::try_from(step.rem_euclid(3) + 1).unwrap_or(1);
    (0..count)
        .map(|index| {
            let value = next_u64(random);
            (
                format!("field-{index}").into_bytes(),
                [
                    step.to_be_bytes().as_slice(),
                    value.to_be_bytes().as_slice(),
                ]
                .concat(),
            )
        })
        .collect()
}

fn assert_range_matches(
    database: &NativeDatabase,
    key: &[u8],
    model: &Model,
    now: i64,
    start: u64,
    end: u64,
    limit: usize,
) -> Result<(), NativeRuntimeError> {
    let actual = database.xrange_latest_stream_at(key, start, end, limit, now);
    if model.visible(now) {
        assert_eq!(actual?, model.range(start, end, limit));
    } else {
        assert!(matches!(
            actual,
            Err(NativeRuntimeError::UnknownStructureStream)
        ));
    }
    Ok(())
}

fn stream_key(generation: u64) -> Vec<u8> {
    format!("events-{generation}").into_bytes()
}

#[test]
fn seeded_stream_trace_matches_model_across_reopen() -> Result<(), Box<dyn std::error::Error>> {
    for seed in 1..=8_u64 {
        let temporary =
            std::env::temp_dir().join(format!("hyphae-stream-model-{}-{seed}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temporary);
        let mut database = NativeDatabase::create(&temporary)?;
        let mut generation = 0;
        let mut key = stream_key(generation);
        let mut create = database.begin_optimistic(0, DurabilityClass::Strict)?;
        create.create_stream(key.clone())?;
        database.commit_optimistic(create)?;
        let mut model = Model::default();
        model.create();
        let mut random = seed;
        for step in 1..=128_i64 {
            match step % 16 {
                1 | 2 | 3 | 4 | 6 | 12 | 13 | 0 => {
                    let fields = fields_for(step, &mut random);
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    assert_eq!(batch.xadd(key.clone(), &fields)?, model.append(fields));
                    database.commit_optimistic(batch)?;
                }
                5 => {
                    let expiry = step + 3;
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    assert_eq!(
                        batch.expire_stream(key.clone(), expiry)?,
                        model.expire(expiry)
                    );
                    database.commit_optimistic(batch)?;
                }
                9 => {
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    assert_eq!(batch.delete_stream(key.clone())?, model.delete());
                    database.commit_optimistic(batch)?;
                }
                10 => {
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    assert!(!batch.delete_stream(key.clone())?);
                    assert!(!batch.expire_stream(key.clone(), step + 1)?);
                }
                11 => {
                    generation += 1;
                    key = stream_key(generation);
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    batch.create_stream(key.clone())?;
                    model.create();
                    let fields = fields_for(step, &mut random);
                    assert_eq!(batch.xadd(key.clone(), &fields)?, model.append(fields));
                    database.commit_optimistic(batch)?;
                }
                14 => {
                    let expiry = step + 10;
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    assert_eq!(
                        batch.expire_stream(key.clone(), expiry)?,
                        model.expire(expiry)
                    );
                    database.commit_optimistic(batch)?;
                }
                7 | 8 | 15 => {
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    assert!(!batch.delete_stream(b"missing".to_vec())?);
                    assert!(!batch.expire_stream(b"missing".to_vec(), step)?);
                }
                _ => unreachable!(),
            }

            assert_range_matches(&database, &key, &model, step, 1, u64::MAX, 256)?;
            let start = next_u64(&mut random) % (model.next + 3);
            let end = next_u64(&mut random) % (model.next + 3);
            let limit = usize::try_from(next_u64(&mut random) % 6)?;
            assert_range_matches(&database, &key, &model, step, start, end, limit)?;
            if step % 16 == 0 {
                drop(database);
                database = NativeDatabase::open(&temporary)?;
                assert_range_matches(&database, &key, &model, step, 0, u64::MAX, 256)?;
            }
        }
        drop(database);
        std::fs::remove_dir_all(&temporary)?;
    }
    Ok(())
}

#[test]
fn negative_control_rejects_a_perturbed_stream_oracle() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-stream-model-negative-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut batch = database.begin_optimistic(1, DurabilityClass::Strict)?;
    batch.create_stream(b"events".to_vec())?;
    let fields = vec![
        (b"kind".to_vec(), b"created".to_vec()),
        (b"source".to_vec(), b"control".to_vec()),
    ];
    batch.xadd(b"events".to_vec(), &fields)?;
    database.commit_optimistic(batch)?;

    let mut perturbed = Model::default();
    perturbed.create();
    perturbed.append(fields);
    let entry = perturbed
        .entries
        .get_mut(&1)
        .ok_or("missing seeded entry")?;
    entry[0].1 = b"deleted".to_vec();
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(assert_range_matches(&database, b"events", &perturbed, 1, 1, u64::MAX, 8).is_ok());
    }));
    assert!(rejected.is_err(), "the perturbed oracle was not rejected");

    drop(database);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
