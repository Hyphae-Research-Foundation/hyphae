// SPDX-License-Identifier: AGPL-3.0-only

//! Deterministic sorted-set model equivalence for G3.

use std::{cmp::Ordering, collections::BTreeMap, ops::Bound};

use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, SortedSetEntry, ZAddOutcome};
use hyphae_native_types::DurabilityClass;

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

fn ordered(model: &BTreeMap<Vec<u8>, f64>) -> Vec<(Vec<u8>, f64)> {
    let mut entries = model
        .iter()
        .map(|(member, score)| (member.clone(), *score))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    entries
}

fn expected_rank_range(entries: &[(Vec<u8>, f64)], start: i64, stop: i64) -> Vec<(Vec<u8>, f64)> {
    let length = i128::try_from(entries.len()).unwrap_or(i128::MAX);
    if length == 0 {
        return Vec::new();
    }
    let start = i128::from(start);
    let stop = i128::from(stop);
    let start = if start < 0 { length + start } else { start }.max(0);
    let stop = if stop < 0 { length + stop } else { stop };
    if stop < 0 || start >= length || start > stop {
        return Vec::new();
    }
    let start = usize::try_from(start).unwrap_or(0);
    let stop = usize::try_from(stop.min(length - 1)).unwrap_or(0);
    entries[start..=stop].to_vec()
}

fn score_is_within(score: f64, lower: Bound<f64>, upper: Bound<f64>) -> bool {
    let above_lower = match lower {
        Bound::Included(lower) => score.total_cmp(&lower) != Ordering::Less,
        Bound::Excluded(lower) => score.total_cmp(&lower) == Ordering::Greater,
        Bound::Unbounded => true,
    };
    let below_upper = match upper {
        Bound::Included(upper) => score.total_cmp(&upper) != Ordering::Greater,
        Bound::Excluded(upper) => score.total_cmp(&upper) == Ordering::Less,
        Bound::Unbounded => true,
    };
    above_lower && below_upper
}

fn expected_score_range(
    entries: &[(Vec<u8>, f64)],
    lower: Bound<f64>,
    upper: Bound<f64>,
    offset: usize,
    limit: usize,
) -> Vec<(Vec<u8>, f64)> {
    entries
        .iter()
        .filter(|(_, score)| score_is_within(*score, lower, upper))
        .skip(offset)
        .take(limit)
        .cloned()
        .collect()
}

fn assert_entries(actual: &[SortedSetEntry], expected: &[(Vec<u8>, f64)]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            (actual.member(), actual.score()),
            (expected.0.as_slice(), expected.1)
        );
    }
}

fn assert_latest_matches_model(
    database: &NativeDatabase,
    model: &BTreeMap<Vec<u8>, f64>,
) -> Result<(), NativeRuntimeError> {
    let entries = ordered(model);
    assert_eq!(database.zcard_latest_sorted_set(b"scores")?, entries.len());
    assert_entries(
        &database.zrange_latest_sorted_set(b"scores", 0, -1)?,
        &entries,
    );
    assert_eq!(
        database.zscore_latest_sorted_set(b"scores", b"absent")?,
        None
    );
    assert_eq!(
        database.zrank_latest_sorted_set(b"scores", b"absent")?,
        None
    );
    assert_eq!(
        database.zrevrank_latest_sorted_set(b"scores", b"absent")?,
        None
    );
    for (rank, (member, score)) in entries.iter().enumerate() {
        assert_eq!(
            database.zscore_latest_sorted_set(b"scores", member)?,
            Some(*score)
        );
        assert_eq!(
            database.zrank_latest_sorted_set(b"scores", member)?,
            Some(rank)
        );
        assert_eq!(
            database.zrevrank_latest_sorted_set(b"scores", member)?,
            Some(entries.len() - rank - 1)
        );
    }
    for (start, stop) in [(0, -1), (1, 4), (-5, -2), (3, 1), (100, 200), (-100, 2)] {
        assert_entries(
            &database.zrange_latest_sorted_set(b"scores", start, stop)?,
            &expected_rank_range(&entries, start, stop),
        );
        let mut descending = entries.clone();
        descending.reverse();
        assert_entries(
            &database.zrevrange_latest_sorted_set(b"scores", start, stop)?,
            &expected_rank_range(&descending, start, stop),
        );
    }
    for (lower, upper, offset, limit) in [
        (Bound::Unbounded, Bound::Unbounded, 0, 128),
        (Bound::Included(20.0), Bound::Excluded(70.0), 1, 5),
        (Bound::Excluded(80.0), Bound::Included(10.0), 0, 8),
        (Bound::Unbounded, Bound::Included(50.0), 2, 0),
    ] {
        let expected = expected_score_range(&entries, lower, upper, offset, limit);
        assert_entries(
            &database.zrange_by_score_latest_sorted_set(b"scores", lower, upper, offset, limit)?,
            &expected,
        );
        let mut descending = entries.clone();
        descending.reverse();
        assert_entries(
            &database
                .zrevrange_by_score_latest_sorted_set(b"scores", lower, upper, offset, limit)?,
            &expected_score_range(&descending, lower, upper, offset, limit),
        );
    }
    Ok(())
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

        let control_member = b"negative-control".to_vec();
        let mut rolled_back = database.begin_optimistic(1, DurabilityClass::Strict)?;
        assert_eq!(
            rolled_back.zadd(b"scores".to_vec(), 99.0, control_member.clone())?,
            ZAddOutcome::Added
        );
        drop(rolled_back);
        assert_eq!(
            database.zscore_latest_sorted_set(b"scores", &control_member)?,
            None
        );

        let mut no_op = database.begin_optimistic(1, DurabilityClass::Strict)?;
        assert!(!no_op.zrem(b"scores".to_vec(), b"missing".to_vec())?);
        drop(no_op);
        assert_latest_matches_model(&database, &model)?;

        let mut random = seed;
        for step in 2..=97_i64 {
            let member = (next_u64(&mut random) % 24).to_be_bytes().to_vec();
            let mut batch = database.begin_optimistic(step, DurabilityClass::Strict)?;
            let mut candidate = model.clone();
            let changed = if random % 4 == 0 {
                let expected = candidate.remove(&member).is_some();
                assert_eq!(batch.zrem(b"scores".to_vec(), member)?, expected);
                expected
            } else {
                let score = f64::from((next_u64(&mut random) % 1_000) as u32) / 10.0;
                let expected = match candidate.insert(member.clone(), score) {
                    None => ZAddOutcome::Added,
                    Some(previous) if previous.to_bits() == score.to_bits() => {
                        ZAddOutcome::Unchanged
                    }
                    Some(_) => ZAddOutcome::Updated,
                };
                assert_eq!(batch.zadd(b"scores".to_vec(), score, member)?, expected);
                expected != ZAddOutcome::Unchanged
            };
            let roll_back_effective_change = changed && step % 13 == 0;
            if changed && !roll_back_effective_change {
                database.commit_optimistic(batch)?;
                model = candidate;
            } else {
                drop(batch);
            }
            assert_latest_matches_model(&database, &model)?;
            if step % 16 == 0 {
                drop(database);
                database = NativeDatabase::open(&temporary)?;
                assert_latest_matches_model(&database, &model)?;
            }
        }
        drop(database);
        std::fs::remove_dir_all(&temporary)?;
    }
    Ok(())
}

#[test]
fn sorted_set_lifecycle_and_ttl_do_not_resurrect_members() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-zset-model-lifecycle-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut create = database.begin_optimistic(1, DurabilityClass::Strict)?;
    create.create_sorted_set(b"scores".to_vec())?;
    create.zadd(b"scores".to_vec(), 1.0, b"retired".to_vec())?;
    database.commit_optimistic(create)?;

    let mut expiry = database.begin_optimistic(2, DurabilityClass::Strict)?;
    assert!(expiry.expire_sorted_set(b"scores".to_vec(), 10)?);
    database.commit_optimistic(expiry)?;
    assert_eq!(database.zcard_latest_sorted_set_at(b"scores", 9)?, 1);
    assert!(matches!(
        database.zcard_latest_sorted_set_at(b"scores", 10),
        Err(NativeRuntimeError::UnknownStructureSortedSet)
    ));
    let early_sweep = database.expire_due_structures(9, 8, DurabilityClass::Strict)?;
    assert_eq!(early_sweep.expired_keys, 0);
    assert!(early_sweep.commit.is_none());
    let due_sweep = database.expire_due_structures(10, 8, DurabilityClass::Strict)?;
    assert_eq!(due_sweep.expired_keys, 1);
    assert!(due_sweep.commit.is_some());

    drop(database);
    let mut database = NativeDatabase::open(&temporary)?;
    assert!(matches!(
        database.zcard_latest_sorted_set_at(b"scores", 10),
        Err(NativeRuntimeError::UnknownStructureSortedSet)
    ));
    let mut create_lifecycle = database.begin_optimistic(11, DurabilityClass::Strict)?;
    create_lifecycle.create_sorted_set(b"lifecycle".to_vec())?;
    create_lifecycle.zadd(b"lifecycle".to_vec(), 1.0, b"retired".to_vec())?;
    database.commit_optimistic(create_lifecycle)?;
    let mut delete = database.begin_optimistic(12, DurabilityClass::Strict)?;
    assert!(delete.delete_sorted_set(b"lifecycle".to_vec())?);
    assert!(!delete.delete_sorted_set(b"lifecycle".to_vec())?);
    assert!(!delete.expire_sorted_set(b"lifecycle".to_vec(), 20)?);
    database.commit_optimistic(delete)?;
    drop(database);

    let reopened = NativeDatabase::open(&temporary)?;
    assert!(matches!(
        reopened.zcard_latest_sorted_set(b"lifecycle"),
        Err(NativeRuntimeError::UnknownStructureSortedSet)
    ));
    drop(reopened);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
