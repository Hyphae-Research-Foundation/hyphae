// SPDX-License-Identifier: Apache-2.0

//! Deterministic scalar/counter and set model equivalence for G3.

use std::{collections::BTreeSet, io};

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, SetCondition, SetOutcome, Ttl};
use hyphae_native_types::DurabilityClass;

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

#[derive(Clone, Debug, Default)]
struct ScalarModel {
    value: Option<Vec<u8>>,
    expires_at_micros: Option<i64>,
}

impl ScalarModel {
    fn advance_to(&mut self, logical_time_micros: i64) {
        if self
            .expires_at_micros
            .is_some_and(|expiry| expiry <= logical_time_micros)
        {
            self.value = None;
            self.expires_at_micros = None;
        }
    }

    fn ttl(&self, logical_time_micros: i64) -> Ttl {
        match (&self.value, self.expires_at_micros) {
            (None, _) => Ttl::Missing,
            (Some(_), None) => Ttl::Persistent,
            (Some(_), Some(expiry)) => {
                Ttl::RemainingMicros(expiry.saturating_sub(logical_time_micros))
            }
        }
    }
}

#[derive(Clone, Debug)]
struct SetModel {
    members: BTreeSet<Vec<u8>>,
    expires_at_micros: Option<i64>,
}

impl SetModel {
    fn ttl(&self, logical_time_micros: i64) -> Ttl {
        self.expires_at_micros.map_or(Ttl::Persistent, |expiry| {
            Ttl::RemainingMicros(expiry.saturating_sub(logical_time_micros))
        })
    }
}

fn audit_scalar(
    database: &NativeDatabase,
    model: &ScalarModel,
    logical_time_micros: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let actual = database.get_latest_structure(b"counter", logical_time_micros)?;
    if actual != model.value {
        return Err(io::Error::other(format!(
            "scalar model divergence at time {logical_time_micros}: actual={actual:?} expected={:?}",
            model.value
        ))
        .into());
    }
    let actual_ttl = database.ttl_latest_structure(b"counter", logical_time_micros)?;
    let expected_ttl = model.ttl(logical_time_micros);
    if actual_ttl != expected_ttl {
        return Err(io::Error::other(format!(
            "scalar TTL divergence at time {logical_time_micros}: actual={actual_ttl:?} expected={expected_ttl:?}"
        ))
        .into());
    }
    Ok(())
}

fn audit_set(
    database: &NativeDatabase,
    model: Option<&SetModel>,
    logical_time_micros: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let actual_ttl = database.ttl_latest_set(b"members", logical_time_micros)?;
    let expected_ttl = model.map_or(Ttl::Missing, |set| set.ttl(logical_time_micros));
    if actual_ttl != expected_ttl {
        return Err(io::Error::other(format!(
            "set TTL divergence at time {logical_time_micros}: actual={actual_ttl:?} expected={expected_ttl:?}"
        ))
        .into());
    }

    if let Some(model) = model {
        let actual_members = database.sscan_latest_set_at(
            b"members",
            None,
            model.members.len() + 1,
            logical_time_micros,
        )?;
        let expected_members = model.members.iter().cloned().collect::<Vec<_>>();
        if actual_members != expected_members {
            return Err(io::Error::other(format!(
                "set content divergence at time {logical_time_micros}: actual={actual_members:?} expected={expected_members:?}"
            ))
            .into());
        }
        if database.scard_latest_set_at(b"members", logical_time_micros)? != model.members.len() {
            return Err(io::Error::other(format!(
                "set cardinality divergence at time {logical_time_micros}"
            ))
            .into());
        }
    } else {
        assert!(matches!(
            database.sscan_latest_set_at(b"members", None, 1, logical_time_micros),
            Err(NativeRuntimeError::UnknownStructureSet)
        ));
        assert!(matches!(
            database.scard_latest_set_at(b"members", logical_time_micros),
            Err(NativeRuntimeError::UnknownStructureSet)
        ));
    }
    Ok(())
}

#[test]
fn perturbed_scalar_and_set_models_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-scalar-set-negative-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut batch = database.begin_optimistic(1, DurabilityClass::Strict)?;
    batch.set(b"counter".to_vec(), b"7".to_vec(), None)?;
    batch.create_set(b"members".to_vec())?;
    batch.sadd(b"members".to_vec(), b"native".to_vec())?;
    database.commit_optimistic(batch)?;

    let scalar_error = audit_scalar(
        &database,
        &ScalarModel {
            value: Some(b"8".to_vec()),
            expires_at_micros: None,
        },
        1,
    )
    .err()
    .ok_or("perturbed scalar oracle was accepted")?;
    assert!(scalar_error.to_string().contains("scalar model divergence"));

    let set_error = audit_set(
        &database,
        Some(&SetModel {
            members: BTreeSet::from([b"perturbed".to_vec()]),
            expires_at_micros: None,
        }),
        1,
    )
    .err()
    .ok_or("perturbed set oracle was accepted")?;
    assert!(set_error.to_string().contains("set content divergence"));

    drop(database);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn seeded_scalar_counter_trace_matches_model() -> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-scalar-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut model = ScalarModel::default();
    let mut random = 7_u64;
    for step in 1..=128_i64 {
        model.advance_to(step);
        let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
        let mut should_commit = false;
        match next_u64(&mut random) % 7 {
            0 => {
                let delta = i64::try_from(random % 11)? - 5;
                if model.value.as_deref() == Some(b"not-an-integer") {
                    assert!(matches!(
                        batch.increment_i64(b"counter".to_vec(), delta),
                        Err(NativeRuntimeError::StructureValueNotInteger)
                    ));
                } else {
                    let base = model
                        .value
                        .as_deref()
                        .map(|value| {
                            std::str::from_utf8(value)
                                .map_err(|_| NativeRuntimeError::StructureValueNotInteger)?
                                .parse::<i64>()
                                .map_err(|_| NativeRuntimeError::StructureValueNotInteger)
                        })
                        .transpose()?
                        .unwrap_or(0);
                    let expected = base
                        .checked_add(delta)
                        .ok_or(NativeRuntimeError::StructureIntegerOverflow)?;
                    assert_eq!(batch.increment_i64(b"counter".to_vec(), delta)?, expected);
                    model.value = Some(expected.to_string().into_bytes());
                    should_commit = true;
                }
            }
            1 => {
                let value = i64::try_from(random % 101)?.to_string().into_bytes();
                batch.set(b"counter".to_vec(), value.clone(), None)?;
                model.value = Some(value);
                model.expires_at_micros = None;
                should_commit = true;
            }
            2 => {
                batch.set(b"counter".to_vec(), b"not-an-integer".to_vec(), None)?;
                model.value = Some(b"not-an-integer".to_vec());
                model.expires_at_micros = None;
                should_commit = true;
            }
            3 => {
                let expected = model.value.is_some();
                assert_eq!(
                    batch.expire_structure(b"counter".to_vec(), step + 3)?,
                    expected
                );
                if expected {
                    model.expires_at_micros = Some(step + 3);
                    should_commit = true;
                }
            }
            4 => {
                let expected = model.value.is_some();
                assert_eq!(batch.delete_structure(b"counter".to_vec())?, expected);
                if expected {
                    model = ScalarModel::default();
                    should_commit = true;
                }
            }
            5 => {
                let condition = if model.value.is_some() {
                    SetCondition::IfAbsent
                } else {
                    SetCondition::IfPresent
                };
                assert_eq!(
                    batch.set_conditional(
                        b"counter".to_vec(),
                        b"must-not-apply".to_vec(),
                        None,
                        condition,
                    )?,
                    SetOutcome::NotApplied
                );
            }
            _ => assert_eq!(batch.get(b"counter"), model.value.as_deref()),
        }
        if should_commit {
            database.commit_optimistic(batch)?;
        }
        audit_scalar(&database, &model, step)?;
        if step % 16 == 0 {
            drop(database);
            database = NativeDatabase::open(&temporary)?;
            audit_scalar(&database, &model, step)?;
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
        let mut model = Some(SetModel {
            members: BTreeSet::new(),
            expires_at_micros: None,
        });
        let mut random = seed;
        for step in 1..=96_i64 {
            if model
                .as_ref()
                .and_then(|set| set.expires_at_micros)
                .is_some_and(|expiry| expiry <= step)
            {
                model = None;
            }
            let member = (next_u64(&mut random) % 32).to_be_bytes().to_vec();
            let action = random % 8;
            let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
            let mut should_commit = false;
            if let Some(set) = model.as_mut() {
                match action {
                    0 => {
                        let expected = set.members.insert(member.clone());
                        assert_eq!(batch.sadd(b"members".to_vec(), member)?, expected);
                        should_commit = expected;
                    }
                    1 => {
                        let expected = set.members.remove(&member);
                        assert_eq!(batch.srem(b"members".to_vec(), member)?, expected);
                        should_commit = expected;
                    }
                    2 => {
                        if let Some(existing) = set.members.first().cloned() {
                            assert!(!batch.sadd(b"members".to_vec(), existing)?);
                        } else {
                            assert!(!batch.srem(b"members".to_vec(), member)?);
                        }
                    }
                    3 => {
                        assert!(batch.expire_set(b"members".to_vec(), step + 3)?);
                        set.expires_at_micros = Some(step + 3);
                        should_commit = true;
                    }
                    4 => {
                        assert!(batch.delete_set(b"members".to_vec())?);
                        model = None;
                        should_commit = true;
                    }
                    5 => assert_eq!(
                        batch.sscan(b"members", None, set.members.len() + 1)?,
                        set.members.iter().cloned().collect::<Vec<_>>()
                    ),
                    6 => assert_eq!(
                        batch.sismember(b"members", &member)?,
                        set.members.contains(&member)
                    ),
                    _ => assert!(matches!(
                        batch.create_set(b"members".to_vec()),
                        Err(NativeRuntimeError::StructureKeyExists)
                    )),
                }
            } else if action % 3 == 0 {
                batch.create_set(b"members".to_vec())?;
                model = Some(SetModel {
                    members: BTreeSet::new(),
                    expires_at_micros: None,
                });
                should_commit = true;
            } else if action % 3 == 1 {
                assert!(!batch.expire_set(b"members".to_vec(), step + 3)?);
            } else {
                assert!(!batch.delete_set(b"members".to_vec())?);
            }
            if should_commit {
                database.commit_optimistic(batch)?;
            }
            audit_set(&database, model.as_ref(), step)?;
            if step % 16 == 0 {
                drop(database);
                database = NativeDatabase::open(&temporary)?;
                audit_set(&database, model.as_ref(), step)?;
            }
        }
        drop(database);
        std::fs::remove_dir_all(&temporary)?;
    }
    Ok(())
}
