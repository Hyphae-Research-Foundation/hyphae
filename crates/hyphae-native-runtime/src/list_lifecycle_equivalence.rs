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
            "hyphae-list-lifecycle-{}-{}",
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
fn whole_list_delete_recreates_without_retired_elements_and_preserves_history()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"retired-a".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"retired-b".to_vec())?;
    seed.commit()?;
    let historical = database.snapshot(11)?;

    let mut replace = database.begin(20, DurabilityClass::Strict)?;
    assert!(replace.delete_list(b"queue".to_vec())?);
    assert!(matches!(
        replace.llen(b"queue"),
        Err(NativeRuntimeError::UnknownStructureList)
    ));
    replace.create_list(b"queue".to_vec())?;
    replace.rpush(b"queue".to_vec(), b"current".to_vec())?;
    replace.commit()?;

    assert_eq!(
        historical.lrange(b"queue", 0, -1)?,
        [b"retired-a".to_vec(), b"retired-b".to_vec()]
    );
    assert_eq!(
        database.snapshot(21)?.lrange(b"queue", 0, -1)?,
        [b"current".to_vec()]
    );
    drop(historical);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.snapshot(21)?.lrange(b"queue", 0, -1)?,
        [b"current".to_vec()]
    );
    Ok(())
}
