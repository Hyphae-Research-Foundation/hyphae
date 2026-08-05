// SPDX-License-Identifier: Apache-2.0

//! Deterministic model-based stream equivalence for G3.

use std::collections::BTreeMap;

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError};
use hyphae_native_types::DurabilityClass;

type Fields = Vec<(Vec<u8>, Vec<u8>)>;

#[derive(Default)]
struct Model {
    entries: BTreeMap<u64, Fields>,
    next: u64,
    expiry: Option<i64>,
}

impl Model {
    fn append(&mut self, fields: Fields) -> u64 {
        self.next += 1;
        self.entries.insert(self.next, fields);
        self.next
    }

    fn visible(&self, now: i64) -> bool {
        self.expiry.is_none_or(|expiry| expiry > now)
    }
}

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

#[test]
fn seeded_stream_trace_matches_model_across_reopen() -> Result<(), Box<dyn std::error::Error>> {
    for seed in 1..=8_u64 {
        let temporary =
            std::env::temp_dir().join(format!("hyphae-stream-model-{}-{seed}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temporary);
        let mut database = NativeDatabase::create(&temporary)?;
        let mut create = database.begin_optimistic(0, DurabilityClass::Strict)?;
        create.create_stream(b"events".to_vec())?;
        database.commit_optimistic(create)?;
        let mut model = Model::default();
        let mut random = seed;
        for step in 1..=128_i64 {
            match next_u64(&mut random) % 3 {
                0 | 1 => {
                    let fields = vec![(b"value".to_vec(), random.to_be_bytes().to_vec())];
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    let actual = batch.xadd(b"events".to_vec(), &fields)?;
                    let expected = model.append(fields);
                    assert_eq!(actual, expected);
                    database.commit_optimistic(batch)?;
                }
                _ => {
                    let expiry = step + 3;
                    let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
                    assert!(batch.expire_stream(b"events".to_vec(), expiry)?);
                    database.commit_optimistic(batch)?;
                    model.expiry = Some(expiry);
                }
            }
            let actual = database.xrange_latest_stream_at(b"events", 1, u64::MAX, 256, step);
            if model.visible(step) {
                let actual = actual?;
                assert_eq!(actual.len(), model.entries.len());
                assert_eq!(
                    actual,
                    model
                        .entries
                        .iter()
                        .map(|(id, fields)| (*id, fields.clone()))
                        .collect::<Vec<_>>()
                );
            } else {
                assert!(matches!(
                    actual,
                    Err(NativeRuntimeError::UnknownStructureStream)
                ));
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
