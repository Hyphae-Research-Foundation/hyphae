// SPDX-License-Identifier: Apache-2.0

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{DurabilityClass, NativeDatabase, Ttl};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-list-ttl-{}-{}",
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
fn whole_list_ttl_is_visible_on_every_execution_surface() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TestDirectory::new();
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Memory)?;
    seed.create_list(b"queue".to_vec())?;
    seed.rpush(b"queue".to_vec(), b"one".to_vec())?;
    assert_eq!(seed.ttl_list(b"queue"), Ttl::Persistent);
    assert!(seed.expire_list(b"queue".to_vec(), 20)?);
    assert_eq!(seed.ttl_list(b"queue"), Ttl::RemainingMicros(10));
    seed.commit()?;

    let before = database.snapshot(19)?;
    assert_eq!(before.ttl_list(b"queue"), Ttl::RemainingMicros(1));
    assert_eq!(database.llen_latest_list_at(b"queue", 19)?, 1);
    assert_eq!(
        database.lrange_latest_list_at(b"queue", 0, -1, 19)?,
        [b"one".to_vec()]
    );
    assert_eq!(
        database.ttl_latest_list(b"queue", 19)?,
        Ttl::RemainingMicros(1)
    );

    let due = database.snapshot(20)?;
    assert_eq!(due.ttl_list(b"queue"), Ttl::Missing);
    Ok(())
}
