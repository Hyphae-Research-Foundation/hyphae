// SPDX-License-Identifier: Apache-2.0

use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_types::DurabilityClass;

use crate::{NativeDatabase, NativeRuntimeError};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-set-lifecycle-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            path: std::env::temp_dir().join(unique),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn whole_set_delete_recreates_without_retired_members_and_preserves_history()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    seed.sadd_many(
        b"members".to_vec(),
        vec![b"retired-a".to_vec(), b"retired-b".to_vec()],
    )?;
    assert!(seed.expire_set(b"members".to_vec(), 1_000)?);
    seed.commit()?;
    let historical = database.snapshot(11)?;

    let mut replace = database.begin(20, DurabilityClass::Strict)?;
    assert!(replace.delete_set(b"members".to_vec())?);
    assert!(matches!(
        replace.scard(b"members"),
        Err(NativeRuntimeError::UnknownStructureSet)
    ));
    replace.create_set(b"members".to_vec())?;
    replace.sadd_many(b"members".to_vec(), vec![b"current".to_vec()])?;
    replace.commit()?;

    assert_eq!(
        historical.sscan(b"members", None, 10)?,
        [b"retired-a".to_vec(), b"retired-b".to_vec()]
    );
    assert_eq!(
        database.sscan_latest_set_at(b"members", None, 10, 21)?,
        [b"current".to_vec()]
    );
    drop(historical);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.sscan_latest_set_at(b"members", None, 10, 21)?,
        [b"current".to_vec()]
    );
    Ok(())
}
