// SPDX-License-Identifier: GPL-3.0-only

//! G4 maintenance and fail-closed corruption matrices for lexical and ANN search.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{AnnSearchOptions, HnswConfig, NativeDatabase, Vector, VectorMetric};
use hyphae_native_types::{DurabilityClass, ObjectId};

type TestError = Box<dyn std::error::Error>;

const PAGE_FILE: &str = "pages.hydb";
const WAL_FILE: &str = "wal.hywal";

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hy-search-g4-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
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

#[derive(Default)]
struct CorruptionMetrics {
    attempted_cases: usize,
    rejected_cases: usize,
    silent_acceptances: usize,
    partial_writes: usize,
}

struct CorruptionCase {
    name: &'static str,
    persisted_magic: &'static [u8],
}

fn ann_config() -> Result<HnswConfig, TestError> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x4859_5048_4145)?)
}

fn ann_options() -> Result<AnnSearchOptions, TestError> {
    Ok(AnnSearchOptions::new(3, 8, Some(4))?)
}

fn seed_search_database(path: &Path) -> Result<(ObjectId, ObjectId), TestError> {
    let lexical = ObjectId::new(100)?;
    let ann = ObjectId::new(200)?;
    let mut database = NativeDatabase::create(path)?;
    let mut seed = database.begin(1, DurabilityClass::Strict)?;
    seed.create_search_index(lexical, "documents")?;
    seed.index_document(lexical, b"doc-a".to_vec(), "alpha shared")?;
    seed.index_document(lexical, b"doc-b".to_vec(), "beta shared")?;
    seed.create_vector_index(ann, "vectors", 3, VectorMetric::Cosine, ann_config()?)?;
    seed.upsert_vectors(
        ann,
        [
            (ObjectId::new(201)?, Vector::new([1.0, 0.0, 0.0])?),
            (ObjectId::new(202)?, Vector::new([0.0, 1.0, 0.0])?),
            (ObjectId::new(203)?, Vector::new([0.0, 0.0, 1.0])?),
        ],
    )?;
    seed.commit()?;
    drop(database);
    Ok((lexical, ann))
}

fn corrupt_all_records(path: &Path, magic: &[u8]) -> Result<(), TestError> {
    let mut bytes = fs::read(path)?;
    let offsets = bytes
        .windows(magic.len())
        .enumerate()
        .filter_map(|(offset, window)| (window == magic).then_some(offset))
        .collect::<Vec<_>>();
    if offsets.is_empty() {
        return Err("persisted search record magic was not found".into());
    }
    for offset in offsets {
        bytes[offset] ^= 0x01;
    }
    fs::write(path, bytes)?;
    Ok(())
}

#[test]
fn rebuild_compaction_and_reopen_preserve_lexical_and_ann_results() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let (lexical, ann) = seed_search_database(&data)?;
    let query = Vector::new([1.0, 0.0, 0.0])?;
    let mut database = NativeDatabase::open(&data)?;

    for (revision, component) in (0..12).zip(2_u16..) {
        let mut update = database.begin(2 + revision, DurabilityClass::Strict)?;
        update.replace_document(
            lexical,
            b"doc-a".to_vec(),
            format!("current shared revision{revision}"),
        )?;
        update.upsert_vector(
            ann,
            ObjectId::new(202)?,
            Vector::new([f32::from(component), 1.0, 0.0])?,
        )?;
        update.commit()?;
    }
    let mut delete = database.begin(20, DurabilityClass::Strict)?;
    delete.delete_document(lexical, b"doc-b".to_vec())?;
    delete.delete_vector(ann, ObjectId::new(203)?)?;
    delete.commit()?;

    let lexical_before = database.match_latest_text(lexical, "current shared", 10)?;
    let ann_before = database.search_ann_latest(ann, &query, ann_options()?)?;
    let exact_before = database.search_vector_exact_latest(ann, &query, 3)?;
    let compacted = database.compact_search(DurabilityClass::Strict)?;
    assert!(compacted.dropped_tombstones > 0);
    assert_eq!(
        database.match_latest_text(lexical, "current shared", 10)?,
        lexical_before
    );
    assert_eq!(
        database
            .search_ann_latest(ann, &query, ann_options()?)?
            .build_identity,
        ann_before.build_identity
    );
    assert_eq!(
        database
            .search_ann_latest(ann, &query, ann_options()?)?
            .hits,
        ann_before.hits
    );

    let rebuilt = database.vacuum_pages()?;
    assert!(
        rebuilt.applied,
        "the G4 corpus must exercise a physical rebuild"
    );
    assert!(rebuilt.reclaimed_pages > 0);
    assert_eq!(
        database.search_vector_exact_latest(ann, &query, 3)?,
        exact_before
    );
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    assert_eq!(
        reopened.match_latest_text(lexical, "current shared", 10)?,
        lexical_before
    );
    assert!(reopened.match_latest_text(lexical, "beta", 10)?.is_empty());
    assert_eq!(
        reopened
            .search_ann_latest(ann, &query, ann_options()?)?
            .build_identity,
        ann_before.build_identity
    );
    assert_eq!(
        reopened
            .search_ann_latest(ann, &query, ann_options()?)?
            .hits,
        ann_before.hits
    );
    assert_eq!(
        reopened.search_vector_exact_latest(ann, &query, 3)?,
        exact_before
    );
    Ok(())
}

#[test]
fn structured_corruption_matrix_has_zero_silent_acceptance_or_partial_writes()
-> Result<(), TestError> {
    let cases = [
        CorruptionCase {
            name: "lexical document",
            persisted_magic: b"HYDOCS01",
        },
        CorruptionCase {
            name: "lexical posting",
            persisted_magic: b"HYPOST01",
        },
        CorruptionCase {
            name: "ANN metadata V4",
            persisted_magic: b"HYANNM04",
        },
        CorruptionCase {
            name: "ANN vector",
            persisted_magic: b"HYANNV01",
        },
        CorruptionCase {
            name: "ANN graph",
            persisted_magic: b"HYANNG01",
        },
    ];
    let mut metrics = CorruptionMetrics::default();

    for case in cases {
        metrics.attempted_cases += 1;
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join(case.name.replace(' ', "-"));
        let (lexical, ann) = seed_search_database(&data)?;
        let page_path = data.join(PAGE_FILE);
        let wal_path = data.join(WAL_FILE);
        corrupt_all_records(&page_path, case.persisted_magic)?;
        let pages_before = fs::read(&page_path)?;
        let wal_before = fs::read(&wal_path)?;

        let rejected = match NativeDatabase::open(&data) {
            Err(_) => true,
            Ok(database) if case.name.starts_with("lexical") => {
                database.match_latest_text(lexical, "shared", 10).is_err()
            }
            Ok(database) => database
                .search_ann_latest(ann, &Vector::new([1.0, 0.0, 0.0])?, ann_options()?)
                .is_err(),
        };
        if rejected {
            metrics.rejected_cases += 1;
        } else {
            metrics.silent_acceptances += 1;
        }
        if fs::read(&page_path)? != pages_before || fs::read(&wal_path)? != wal_before {
            metrics.partial_writes += 1;
        }
    }

    assert_eq!(metrics.rejected_cases, metrics.attempted_cases);
    assert_eq!(metrics.silent_acceptances, 0);
    assert_eq!(metrics.partial_writes, 0);
    Ok(())
}
