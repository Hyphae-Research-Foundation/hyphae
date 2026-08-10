// SPDX-License-Identifier: GPL-3.0-only

//! Deterministic list model equivalence for G3.

use std::collections::VecDeque;

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, Ttl};
use hyphae_native_types::DurabilityClass;

#[derive(Clone, Debug, Default)]
struct ListModel {
    exists: bool,
    values: VecDeque<Vec<u8>>,
    expires_at_micros: Option<i64>,
}

impl ListModel {
    fn is_visible(&self, logical_time_micros: i64) -> bool {
        self.exists
            && self
                .expires_at_micros
                .is_none_or(|expiry| expiry > logical_time_micros)
    }

    fn create(&mut self) {
        self.exists = true;
        self.values.clear();
        self.expires_at_micros = None;
    }

    fn delete(&mut self, logical_time_micros: i64) -> bool {
        if !self.is_visible(logical_time_micros) {
            return false;
        }
        self.exists = false;
        self.values.clear();
        self.expires_at_micros = None;
        true
    }

    fn expire(&mut self, expires_at_micros: i64, logical_time_micros: i64) -> bool {
        if !self.is_visible(logical_time_micros) {
            return false;
        }
        self.expires_at_micros = Some(expires_at_micros);
        true
    }

    fn ttl(&self, logical_time_micros: i64) -> Ttl {
        if !self.is_visible(logical_time_micros) {
            return Ttl::Missing;
        }
        self.expires_at_micros.map_or(Ttl::Persistent, |expiry| {
            Ttl::RemainingMicros(expiry - logical_time_micros)
        })
    }

    fn range(&self, start: i64, stop: i64) -> Vec<Vec<u8>> {
        let length = self.values.len() as i128;
        if length == 0 {
            return Vec::new();
        }
        let normalize = |index: i64| {
            let index = i128::from(index);
            if index < 0 { length + index } else { index }
        };
        let start = normalize(start).max(0);
        let stop = normalize(stop).min(length - 1);
        if start >= length || stop < 0 || start > stop {
            return Vec::new();
        }
        let start = usize::try_from(start).unwrap_or(0);
        let count = usize::try_from(stop - i128::try_from(start).unwrap_or(0) + 1).unwrap_or(0);
        self.values
            .iter()
            .skip(start)
            .take(count)
            .cloned()
            .collect()
    }
}

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

fn audit_model(
    database: &NativeDatabase,
    model: &ListModel,
    logical_time_micros: i64,
    context: &str,
) -> Result<(), String> {
    let key = b"items";
    let expected_ttl = model.ttl(logical_time_micros);
    let physical_ttl = database
        .ttl_latest_list(key, logical_time_micros)
        .map_err(|error| format!("{context}: physical TTL failed: {error:?}"))?;
    if physical_ttl != expected_ttl {
        return Err(format!(
            "{context}: physical TTL mismatch: expected {expected_ttl:?}, got {physical_ttl:?}"
        ));
    }

    let snapshot = database
        .snapshot(logical_time_micros)
        .map_err(|error| format!("{context}: snapshot failed: {error:?}"))?;
    let snapshot_ttl = snapshot.ttl_list(key);
    if snapshot_ttl != expected_ttl {
        return Err(format!(
            "{context}: snapshot TTL mismatch: expected {expected_ttl:?}, got {snapshot_ttl:?}"
        ));
    }

    if !model.is_visible(logical_time_micros) {
        if !matches!(
            database.llen_latest_list_at(key, logical_time_micros),
            Err(NativeRuntimeError::UnknownStructureList)
        ) {
            return Err(format!("{context}: missing list had a physical length"));
        }
        if !matches!(
            snapshot.lrange(key, 0, -1),
            Err(NativeRuntimeError::UnknownStructureList)
        ) {
            return Err(format!("{context}: missing list had a snapshot range"));
        }
        return Ok(());
    }

    let expected_length = model.values.len();
    let physical_length = database
        .llen_latest_list_at(key, logical_time_micros)
        .map_err(|error| format!("{context}: physical length failed: {error:?}"))?;
    let snapshot_length = snapshot
        .llen(key)
        .map_err(|error| format!("{context}: snapshot length failed: {error:?}"))?;
    if physical_length != expected_length || snapshot_length != expected_length {
        return Err(format!(
            "{context}: length mismatch: expected {expected_length}, physical {physical_length}, snapshot {snapshot_length}"
        ));
    }

    for (start, stop) in [
        (0, -1),
        (0, 0),
        (-1, -1),
        (-3, -2),
        (1, 3),
        (5, 2),
        (i64::MIN, i64::MAX),
    ] {
        let expected = model.range(start, stop);
        let physical = database
            .lrange_latest_list_at(key, start, stop, logical_time_micros)
            .map_err(|error| {
                format!("{context}: physical range {start}..={stop} failed: {error:?}")
            })?;
        let retained = snapshot.lrange(key, start, stop).map_err(|error| {
            format!("{context}: snapshot range {start}..={stop} failed: {error:?}")
        })?;
        if physical != expected || retained != expected {
            return Err(format!(
                "{context}: range {start}..={stop} mismatch: expected {expected:?}, physical {physical:?}, snapshot {retained:?}"
            ));
        }
    }
    Ok(())
}

#[test]
fn perturbed_list_oracle_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-list-model-negative-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_optimistic(1, DurabilityClass::Strict)?;
    seed.create_list(b"items".to_vec())?;
    seed.rpush(b"items".to_vec(), b"a".to_vec())?;
    seed.rpush(b"items".to_vec(), b"b".to_vec())?;
    database.commit_optimistic(seed)?;

    let mut model = ListModel::default();
    model.create();
    model.values.extend([b"a".to_vec(), b"b".to_vec()]);
    audit_model(&database, &model, 2, "unperturbed control")?;
    model.values[0] = b"perturbed".to_vec();
    let rejection = audit_model(&database, &model, 2, "perturbed control")
        .err()
        .ok_or("a perturbed independent oracle passed the audit")?;
    assert!(rejection.contains("range 0..=-1 mismatch"), "{rejection}");

    drop(database);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn seeded_list_trace_matches_model_across_reopen() -> Result<(), Box<dyn std::error::Error>> {
    for seed in 1..=4_u64 {
        let temporary =
            std::env::temp_dir().join(format!("hyphae-list-model-{}-{seed}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temporary);
        let mut database = NativeDatabase::create(&temporary)?;
        let mut create = database.begin_optimistic(0, DurabilityClass::Strict)?;
        create.create_list(b"items".to_vec())?;
        database.commit_optimistic(create)?;
        let mut model = ListModel::default();
        model.create();

        let mut no_op = database.begin_optimistic(1, DurabilityClass::Strict)?;
        assert_eq!(no_op.lpop(b"items".to_vec())?, None);
        assert_eq!(no_op.rpop(b"items".to_vec())?, None);
        assert!(!no_op.delete_list(b"missing".to_vec())?);
        assert!(!no_op.expire_list(b"missing".to_vec(), 10)?);
        drop(no_op);
        audit_model(&database, &model, 1, &format!("seed {seed}, no-op prelude"))?;

        let mut random = seed;
        for step in 2..=96_i64 {
            let action = next_u64(&mut random) % 8;
            let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
            let should_commit = match action {
                0 | 1 => {
                    if !model.is_visible(step) {
                        batch.create_list(b"items".to_vec())?;
                        model.create();
                    }
                    let value = next_u64(&mut random).to_be_bytes().to_vec();
                    let expected_length = model.values.len() + 1;
                    if action == 0 {
                        assert_eq!(
                            batch.lpush(b"items".to_vec(), value.clone())?,
                            expected_length
                        );
                        model.values.push_front(value);
                    } else {
                        assert_eq!(
                            batch.rpush(b"items".to_vec(), value.clone())?,
                            expected_length
                        );
                        model.values.push_back(value);
                    }
                    true
                }
                2 if model.is_visible(step) => {
                    let expected = model.values.pop_front();
                    assert_eq!(batch.lpop(b"items".to_vec())?, expected);
                    expected.is_some()
                }
                3 if model.is_visible(step) => {
                    let expected = model.values.pop_back();
                    assert_eq!(batch.rpop(b"items".to_vec())?, expected);
                    expected.is_some()
                }
                4 => {
                    let expected = model.delete(step);
                    assert_eq!(batch.delete_list(b"items".to_vec())?, expected);
                    expected
                }
                5 => {
                    let expiry = step + i64::try_from(next_u64(&mut random) % 5 + 1)?;
                    let expected = model.expire(expiry, step);
                    assert_eq!(batch.expire_list(b"items".to_vec(), expiry)?, expected);
                    expected
                }
                6 => {
                    if model.is_visible(step) {
                        assert!(batch.delete_list(b"items".to_vec())?);
                        assert!(model.delete(step));
                    } else {
                        assert!(!batch.delete_list(b"items".to_vec())?);
                    }
                    batch.create_list(b"items".to_vec())?;
                    model.create();
                    assert_eq!(batch.lpop(b"items".to_vec())?, None);
                    true
                }
                _ => {
                    let start = i64::try_from(next_u64(&mut random) % 15)? - 7;
                    let stop = i64::try_from(next_u64(&mut random) % 15)? - 7;
                    if model.is_visible(step) {
                        assert_eq!(
                            batch.lrange(b"items", start, stop)?,
                            model.range(start, stop)
                        );
                    } else {
                        assert!(matches!(
                            batch.lrange(b"items", start, stop),
                            Err(NativeRuntimeError::UnknownStructureList)
                        ));
                    }
                    false
                }
            };
            if should_commit {
                database.commit_optimistic(batch)?;
            }
            audit_model(
                &database,
                &model,
                step,
                &format!("seed {seed}, step {step}, action {action}"),
            )?;
            if step % 16 == 0 {
                drop(database);
                database = NativeDatabase::open(&temporary)?;
                audit_model(
                    &database,
                    &model,
                    step,
                    &format!("seed {seed}, reopen at step {step}"),
                )?;
            }
        }
        drop(database);
        std::fs::remove_dir_all(&temporary)?;
    }
    Ok(())
}
