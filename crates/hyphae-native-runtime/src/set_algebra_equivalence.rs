// SPDX-License-Identifier: Apache-2.0

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_types::DurabilityClass;

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
fn set_algebra_missing_types_and_bounds_fail_exactly() -> Result<(), Box<dyn Error>> {
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
    let output_exhausted = request(SetAlgebraOperation::Union, &[b"members"], 1, 32)?;
    assert!(matches!(
        database.set_algebra_latest_at(&output_exhausted, 11),
        Err(NativeRuntimeError::SetAlgebra(
            SetAlgebraError::OutputLimitExceeded { maximum: 1 }
        ))
    ));
    let visits_exhausted = request(SetAlgebraOperation::Union, &[b"members"], 8, 1)?;
    assert!(matches!(
        database.set_algebra_latest_at(&visits_exhausted, 11),
        Err(NativeRuntimeError::SetAlgebra(
            SetAlgebraError::VisitLimitExceeded { maximum: 1 }
        ))
    ));
    Ok(())
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
