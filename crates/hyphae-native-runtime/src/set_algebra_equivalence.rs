// SPDX-License-Identifier: GPL-3.0-only

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_types::{Csn, DurabilityClass};

use crate::{
    NativeDatabase, NativeRuntimeError, SetAlgebraError, SetAlgebraOperation, SetAlgebraRequest,
};

#[test]
fn set_algebra_matches_private_snapshot_physical_and_reopen() -> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let retained_empty = database.snapshot(0)?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    for key in [b"left".as_slice(), b"right".as_slice(), b"third".as_slice()] {
        seed.create_set(key.to_vec())?;
    }
    add_members(&mut seed, b"left", &[b"", b"a", b"c", b"\xff"])?;
    add_members(&mut seed, b"right", &[b"a", b"b", b"\xff"])?;
    add_members(&mut seed, b"third", &[b"a", b"z", b"\xff"])?;

    let union = request(
        SetAlgebraOperation::Union,
        &[b"left", b"right", b"third"],
        16,
        128,
    )?;
    let intersection = request(
        SetAlgebraOperation::Intersection,
        &[b"left", b"right", b"third"],
        16,
        128,
    )?;
    let difference = request(
        SetAlgebraOperation::Difference,
        &[b"left", b"right"],
        16,
        128,
    )?;
    assert_members(
        seed.set_algebra(&union)?.members(),
        &[b"", b"a", b"b", b"c", b"z", b"\xff"],
    );
    assert_members(seed.set_algebra(&intersection)?.members(), &[b"a", b"\xff"]);
    assert_members(seed.set_algebra(&difference)?.members(), &[b"", b"c"]);
    seed.commit()?;

    assert!(retained_empty.set_algebra(&union)?.members().is_empty());
    let snapshot = database.snapshot(11)?;
    for surface in [
        snapshot.set_algebra(&union)?,
        database.set_algebra_latest_at(&union, 11)?,
    ] {
        assert_members(surface.members(), &[b"", b"a", b"b", b"c", b"z", b"\xff"]);
    }
    assert_members(
        database.set_algebra_latest_at(&intersection, 11)?.members(),
        &[b"a", b"\xff"],
    );
    assert_members(
        database.set_algebra_latest_at(&difference, 11)?.members(),
        &[b"", b"c"],
    );
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    assert_members(
        reopened.set_algebra_latest_at(&union, 11)?.members(),
        &[b"", b"a", b"b", b"c", b"z", b"\xff"],
    );
    Ok(())
}

#[test]
fn structure_v3_set_algebra_is_direct_bounded_and_reopen_safe() -> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    for key in [b"left".as_slice(), b"right".as_slice(), b"third".as_slice()] {
        seed.create_set(key.to_vec())?;
    }
    add_members(&mut seed, b"left", &[b"", b"a", b"c", b"\xff"])?;
    add_members(&mut seed, b"right", &[b"a", b"b", b"\xff"])?;
    add_members(&mut seed, b"third", &[b"a", b"z", b"\xff"])?;
    seed.create_list(b"wrong-kind".to_vec())?;
    seed.commit()?;
    database.migrate_structure_to_v3(DurabilityClass::Strict)?;

    let union = request(
        SetAlgebraOperation::Union,
        &[b"left", b"right", b"third"],
        16,
        128,
    )?;
    let intersection = request(
        SetAlgebraOperation::Intersection,
        &[b"left", b"right", b"third"],
        16,
        128,
    )?;
    let difference = request(
        SetAlgebraOperation::Difference,
        &[b"left", b"right"],
        16,
        128,
    )?;
    let missing = request(SetAlgebraOperation::Union, &[b"left", b"missing"], 16, 128)?;
    let wrong_kind = request(
        SetAlgebraOperation::Intersection,
        &[b"missing", b"wrong-kind"],
        16,
        128,
    )?;
    let output_exhausted = request(SetAlgebraOperation::Union, &[b"left"], 1, 128)?;
    let visits_exhausted = request(SetAlgebraOperation::Union, &[b"left"], 16, 1)?;

    crate::FAIL_FULL_STATE_LOAD.set(true);
    crate::FAIL_FULL_STRUCTURE_STATE_LOAD.set(true);
    let result = (|| -> Result<(), Box<dyn Error>> {
        assert_members(
            database.set_algebra_latest_at(&union, 11)?.members(),
            &[b"", b"a", b"b", b"c", b"z", b"\xff"],
        );
        assert_members(
            database.set_algebra_latest_at(&intersection, 11)?.members(),
            &[b"a", b"\xff"],
        );
        assert_members(
            database.set_algebra_latest_at(&difference, 11)?.members(),
            &[b"", b"c"],
        );
        assert_members(
            database.set_algebra_latest_at(&missing, 11)?.members(),
            &[b"", b"a", b"c", b"\xff"],
        );
        assert!(matches!(
            database.set_algebra_latest_at(&wrong_kind, 11),
            Err(NativeRuntimeError::StructureKindMismatch)
        ));
        assert!(matches!(
            database.set_algebra_latest_at(&output_exhausted, 11),
            Err(NativeRuntimeError::SetAlgebra(
                SetAlgebraError::OutputLimitExceeded { maximum: 1 }
            ))
        ));
        assert!(matches!(
            database.set_algebra_latest_at(&visits_exhausted, 11),
            Err(NativeRuntimeError::SetAlgebra(
                SetAlgebraError::VisitLimitExceeded { maximum: 1 }
            ))
        ));
        Ok(())
    })();
    crate::FAIL_FULL_STRUCTURE_STATE_LOAD.set(false);
    crate::FAIL_FULL_STATE_LOAD.set(false);
    result?;

    drop(database);
    let reopened = NativeDatabase::open(temporary.path())?;
    assert_members(
        reopened.set_algebra_latest_at(&intersection, 11)?.members(),
        &[b"a", b"\xff"],
    );
    Ok(())
}

#[test]
fn set_algebra_missing_and_type_rules_are_exact() -> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    seed.create_set(b"members".to_vec())?;
    add_members(&mut seed, b"members", &[b"a", b"b"])?;
    seed.create_list(b"wrong-kind".to_vec())?;
    seed.set(b"due-scalar".to_vec(), b"value".to_vec(), Some(11))?;
    seed.create_hash(b"due-hash".to_vec())?;
    seed.hset(b"due-hash".to_vec(), b"field".to_vec(), b"value".to_vec())?;
    assert!(seed.expire_hash(b"due-hash".to_vec(), 11)?);
    seed.commit()?;

    let union_missing = request(SetAlgebraOperation::Union, &[b"members", b"missing"], 8, 32)?;
    let intersection_missing = request(
        SetAlgebraOperation::Intersection,
        &[b"members", b"missing"],
        8,
        32,
    )?;
    let difference_missing = request(
        SetAlgebraOperation::Difference,
        &[b"members", b"missing"],
        8,
        32,
    )?;
    let repeated_first = request(
        SetAlgebraOperation::Difference,
        &[b"members", b"members"],
        8,
        32,
    )?;
    assert_members(
        database
            .set_algebra_latest_at(&union_missing, 11)?
            .members(),
        &[b"a", b"b"],
    );
    assert!(
        database
            .set_algebra_latest_at(&intersection_missing, 11)?
            .members()
            .is_empty()
    );
    assert_members(
        database
            .set_algebra_latest_at(&difference_missing, 11)?
            .members(),
        &[b"a", b"b"],
    );
    assert!(
        database
            .set_algebra_latest_at(&repeated_first, 11)?
            .members()
            .is_empty()
    );

    let wrong_kind = request(
        SetAlgebraOperation::Intersection,
        &[b"missing", b"wrong-kind"],
        8,
        32,
    )?;
    assert!(matches!(
        database.set_algebra_latest_at(&wrong_kind, 11),
        Err(NativeRuntimeError::StructureKindMismatch)
    ));

    let due_other_families = request(
        SetAlgebraOperation::Union,
        &[b"members", b"due-scalar", b"due-hash"],
        8,
        32,
    )?;
    assert!(matches!(
        database.set_algebra_latest_at(&due_other_families, 10),
        Err(NativeRuntimeError::StructureKindMismatch)
    ));
    assert_members(
        database
            .set_algebra_latest_at(&due_other_families, 11)?
            .members(),
        &[b"a", b"b"],
    );
    Ok(())
}

#[test]
fn set_algebra_execution_bounds_fail_without_results() -> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    seed.create_set(b"members".to_vec())?;
    add_members(&mut seed, b"members", &[b"a", b"b"])?;
    seed.commit()?;
    let snapshot = database.snapshot(11)?;

    let output_exhausted = request(SetAlgebraOperation::Union, &[b"members"], 1, 32)?;
    assert!(matches!(
        snapshot.set_algebra(&output_exhausted),
        Err(NativeRuntimeError::SetAlgebra(
            SetAlgebraError::OutputLimitExceeded { maximum: 1 }
        ))
    ));
    assert!(matches!(
        database.set_algebra_latest_at(&output_exhausted, 11),
        Err(NativeRuntimeError::SetAlgebra(
            SetAlgebraError::OutputLimitExceeded { maximum: 1 }
        ))
    ));
    let visits_exhausted = request(SetAlgebraOperation::Union, &[b"members"], 8, 1)?;
    assert!(matches!(
        snapshot.set_algebra(&visits_exhausted),
        Err(NativeRuntimeError::SetAlgebra(
            SetAlgebraError::VisitLimitExceeded { maximum: 1 }
        ))
    ));
    assert!(matches!(
        database.set_algebra_latest_at(&visits_exhausted, 11),
        Err(NativeRuntimeError::SetAlgebra(
            SetAlgebraError::VisitLimitExceeded { maximum: 1 }
        ))
    ));
    let oversized_identity = SetAlgebraRequest::try_new(
        SetAlgebraOperation::Union,
        vec![vec![b'x'; hyphae_native_btree::BTREE_MAX_KEY_SIZE]],
        8,
        32,
    )?;
    assert!(matches!(
        snapshot.set_algebra(&oversized_identity),
        Err(NativeRuntimeError::StructureIdentityTooLarge)
    ));
    assert!(matches!(
        database.set_algebra_latest_at(&oversized_identity, 11),
        Err(NativeRuntimeError::StructureIdentityTooLarge)
    ));
    Ok(())
}

#[test]
fn multilevel_intersection_uses_the_smallest_set_and_counts_tombstones()
-> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    for key in [
        b"large".as_slice(),
        b"small".as_slice(),
        b"other".as_slice(),
    ] {
        seed.create_set(key.to_vec())?;
    }
    for index in 0..2_048_u32 {
        assert!(seed.sadd(b"large".to_vec(), index.to_be_bytes().to_vec())?);
        assert!(seed.sadd(b"other".to_vec(), index.to_be_bytes().to_vec())?);
    }
    for index in 100..104_u32 {
        assert!(seed.sadd(b"small".to_vec(), index.to_be_bytes().to_vec())?);
    }
    seed.commit()?;
    assert!(database.latest_structure_tree_height()? >= 2);

    let request = request(
        SetAlgebraOperation::Intersection,
        &[b"large", b"small", b"other"],
        16,
        64,
    )?;
    let retained = database.snapshot(11)?;
    let retained_result = retained.set_algebra(&request)?;
    let physical_result = database.set_algebra_latest_at(&request, 11)?;
    assert_eq!(retained_result.members(), physical_result.members());
    assert_eq!(retained_result.visited(), 12);
    assert_eq!(physical_result.visited(), 12);

    let removed = 101_u32.to_be_bytes();
    let mut mutate = database.begin(12, DurabilityClass::Memory)?;
    assert!(mutate.srem(b"small".to_vec(), removed.to_vec())?);
    mutate.commit()?;
    assert_eq!(retained.set_algebra(&request)?.visited(), 12);
    let materialized = database.snapshot(13)?.set_algebra(&request)?;
    let current = database.set_algebra_latest_at(&request, 13)?;
    assert_eq!(materialized.members(), current.members());
    assert_eq!(materialized.members().len(), 3);
    assert_eq!(materialized.visited(), 9);
    assert_eq!(current.visited(), 10);
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    let reopened_result = reopened.set_algebra_latest_at(&request, 13)?;
    assert_eq!(reopened_result.members(), materialized.members());
    assert_eq!(reopened_result.visited(), 10);
    Ok(())
}

#[test]
fn reached_set_algebra_metadata_and_member_corruption_fail_closed() -> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    seed.create_set(b"members".to_vec())?;
    assert!(seed.sadd(b"members".to_vec(), b"a".to_vec())?);
    seed.commit()?;
    let request = request(SetAlgebraOperation::Union, &[b"members"], 8, 32)?;
    let root = database
        .coordinator
        .snapshot(11)?
        .roots()
        .root(crate::SLOT_STRUCTURE)
        .ok_or(NativeRuntimeError::InvalidCommittedRoot)?;

    let bad_count = hyphae_native_btree::BTree::from_root(root)
        .upsert(
            &mut database.pages,
            Csn::new(2)?,
            crate::structure_set_meta_key(b"members"),
            crate::encode_set_metadata(2),
        )?
        .tree;
    assert!(matches!(
        database.set_algebra_in_tree(bad_count, &request, 11),
        Err(NativeRuntimeError::InvalidStructureTree)
    ));

    let bad_member = hyphae_native_btree::BTree::from_root(root)
        .upsert(
            &mut database.pages,
            Csn::new(2)?,
            crate::structure_set_member_key(b"members", b"a")?,
            b"invalid-member-envelope".to_vec(),
        )?
        .tree;
    assert!(matches!(
        database.set_algebra_in_tree(bad_member, &request, 11),
        Err(NativeRuntimeError::InvalidStructureTree)
    ));
    Ok(())
}

#[test]
fn fixed_binary_set_oracle_matches_every_execution_surface() -> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut transaction = database.begin(100, DurabilityClass::Memory)?;
    let mut oracle = BTreeMap::new();
    let mut random = SplitMix64::new(0x4859_5048_4145_5341);
    for set_index in 0..5 {
        let key = algebra_key(set_index);
        transaction.create_set(key.clone())?;
        let mut members = BTreeSet::new();
        for member_index in 0..64 {
            if !random.next().is_multiple_of(3) {
                let member = algebra_member(member_index);
                assert!(transaction.sadd(key.clone(), member.clone())?);
                members.insert(member);
            }
        }
        oracle.insert(key, members);
    }

    let mut cases = Vec::new();
    for case_index in 0..64 {
        let operation = match case_index % 3 {
            0 => SetAlgebraOperation::Union,
            1 => SetAlgebraOperation::Intersection,
            _ => SetAlgebraOperation::Difference,
        };
        let key_count = 1 + random.index(5);
        let keys = (0..key_count)
            .map(|_| algebra_key(random.index(7)))
            .collect::<Vec<_>>();
        let expected = expected_algebra(&oracle, operation, &keys);
        let request = SetAlgebraRequest::try_new(operation, keys, 128, 4_096)?;
        assert_eq!(transaction.set_algebra(&request)?.members(), expected);
        cases.push((request, expected));
    }
    transaction.commit()?;

    let snapshot = database.snapshot(101)?;
    for (request, expected) in &cases {
        assert_eq!(snapshot.set_algebra(request)?.members(), expected);
        assert_eq!(
            database.set_algebra_latest_at(request, 101)?.members(),
            expected
        );
    }
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    for (request, expected) in &cases {
        assert_eq!(
            reopened.set_algebra_latest_at(request, 101)?.members(),
            expected
        );
    }
    Ok(())
}

fn expected_algebra(
    sets: &BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    operation: SetAlgebraOperation,
    keys: &[Vec<u8>],
) -> Vec<Vec<u8>> {
    let output = match operation {
        SetAlgebraOperation::Union => {
            let mut union = BTreeSet::new();
            for key in keys {
                if let Some(members) = sets.get(key) {
                    union.extend(members.iter().cloned());
                }
            }
            union
        }
        SetAlgebraOperation::Intersection => {
            let mut intersection = sets.get(&keys[0]).cloned().unwrap_or_default();
            for key in &keys[1..] {
                let Some(members) = sets.get(key) else {
                    intersection.clear();
                    break;
                };
                intersection.retain(|member| members.contains(member));
            }
            intersection
        }
        SetAlgebraOperation::Difference => {
            let mut difference = sets.get(&keys[0]).cloned().unwrap_or_default();
            for key in &keys[1..] {
                if let Some(members) = sets.get(key) {
                    difference.retain(|member| !members.contains(member));
                }
            }
            difference
        }
    };
    output.into_iter().collect()
}

fn algebra_key(index: usize) -> Vec<u8> {
    match index {
        0..=4 => format!("set:{index}").into_bytes(),
        _ => format!("missing:{index}").into_bytes(),
    }
}

fn algebra_member(index: usize) -> Vec<u8> {
    match index {
        0 => Vec::new(),
        1 => vec![0],
        2 => vec![0xff],
        _ => u16::try_from(index)
            .unwrap_or_default()
            .to_be_bytes()
            .to_vec(),
    }
}

fn request(
    operation: SetAlgebraOperation,
    keys: &[&[u8]],
    output_limit: usize,
    visit_limit: usize,
) -> Result<SetAlgebraRequest, SetAlgebraError> {
    SetAlgebraRequest::try_new(
        operation,
        keys.iter().map(|key| key.to_vec()).collect(),
        output_limit,
        visit_limit,
    )
}

fn add_members(
    transaction: &mut crate::NativeTransaction<'_>,
    key: &[u8],
    members: &[&[u8]],
) -> Result<(), NativeRuntimeError> {
    for member in members {
        assert!(transaction.sadd(key.to_vec(), member.to_vec())?);
    }
    Ok(())
}

fn assert_members(actual: &[Vec<u8>], expected: &[&[u8]]) {
    assert_eq!(
        actual,
        expected
            .iter()
            .map(|member| member.to_vec())
            .collect::<Vec<_>>()
    );
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-set-algebra-{}-{timestamp}",
            std::process::id()
        ))))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, length: usize) -> usize {
        debug_assert!(length > 0);
        let bounded_length = u64::try_from(length).unwrap_or(u64::MAX);
        usize::try_from(self.next() % bounded_length).unwrap_or(0)
    }
}
