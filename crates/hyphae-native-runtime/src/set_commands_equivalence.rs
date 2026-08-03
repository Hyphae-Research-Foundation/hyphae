// SPDX-License-Identifier: Apache-2.0

use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_types::DurabilityClass;

use crate::NativeDatabase;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let unique = format!(
            "hyphae-set-commands-{}-{}",
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
fn set_member_commands_match_private_snapshot_physical_and_reopen() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new();
    let path = temporary.path().to_path_buf();
    let mut database = NativeDatabase::create(&path)?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_set(b"members".to_vec())?;
    assert_eq!(
        seed.sadd_many(
            b"members".to_vec(),
            vec![b"b".to_vec(), Vec::new(), b"a".to_vec(), vec![0xff]],
        )?,
        4
    );
    assert_eq!(
        seed.smismember(
            b"members",
            &[b"a".to_vec(), b"missing".to_vec(), b"a".to_vec()],
        )?,
        vec![true, false, true]
    );
    assert_eq!(
        seed.sscan(b"members", None, 3)?,
        vec![Vec::new(), b"a".to_vec(), b"b".to_vec()]
    );
    assert_eq!(
        seed.srem_many(
            b"members".to_vec(),
            vec![b"missing".to_vec(), b"b".to_vec()],
        )?,
        1
    );
    seed.commit()?;

    let snapshot = database.snapshot(11)?;
    assert_eq!(
        snapshot.smismember(
            b"members",
            &[Vec::new(), b"b".to_vec(), vec![0xff], Vec::new()],
        )?,
        vec![true, false, true, true]
    );
    assert_eq!(
        snapshot.sscan(b"members", Some(b"".as_slice()), 8)?,
        vec![b"a".to_vec(), vec![0xff]]
    );
    assert_eq!(
        database.smismember_latest_set_at(
            b"members",
            &[Vec::new(), b"a".to_vec(), b"b".to_vec(), vec![0xff]],
            11,
        )?,
        vec![true, true, false, true]
    );
    assert_eq!(
        database.sscan_latest_set_at(b"members", Some(b"a".as_slice()), 8, 11)?,
        vec![vec![0xff]]
    );
    drop(snapshot);
    drop(database);

    let reopened = NativeDatabase::open(&path)?;
    assert_eq!(
        reopened.sscan_latest_set_at(b"members", None, 8, 11)?,
        vec![Vec::new(), b"a".to_vec(), vec![0xff]]
    );
    Ok(())
}
