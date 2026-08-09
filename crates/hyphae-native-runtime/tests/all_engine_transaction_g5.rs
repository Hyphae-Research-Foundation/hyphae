// SPDX-License-Identifier: Apache-2.0

//! Dedicated embedded G5 evidence for one relational/structure/lexical/ANN transaction.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_runtime::{
    AnnSearchOptions, CommitBoundary, HnswConfig, NativeDatabase, NativeRuntimeError,
    NativeSnapshot, NativeWriteBatch, SqlResult, Vector, VectorMetric,
};
use hyphae_native_types::{Csn, DurabilityClass, ObjectId, ScalarValue};

type TestError = Box<dyn std::error::Error>;

const ENGINE_SURFACES: usize = 4;
const INTERRUPTION_BOUNDARIES: usize = 7;
const LEXICAL_INDEX_ID: u128 = 100;
const VECTOR_INDEX_ID: u128 = 200;
const OBJECT_ID: u128 = 300;
const SELECT_EVENT: &str = "SELECT body FROM events WHERE id = ?";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        let ordinal = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hy-g5-all-engine-{}-{ordinal}", std::process::id()));
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

#[derive(Clone, Copy)]
struct Identities {
    lexical: ObjectId,
    vectors: ObjectId,
    object: ObjectId,
}

struct SeededDatabase {
    database: NativeDatabase,
    prior: NativeSnapshot,
    ids: Identities,
    seed_csn: Csn,
}

#[derive(Debug, Eq, PartialEq)]
struct Observation {
    relational: Option<String>,
    structure: Option<Vec<u8>>,
    lexical: bool,
    ann: bool,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct G5Metrics {
    engine_surfaces_verified: usize,
    one_csn_checks: usize,
    prior_snapshot_checks: usize,
    conflict_checks: usize,
    rollback_checks: usize,
    interruption_boundaries: usize,
    reopen_checks: usize,
}

impl G5Metrics {
    fn report(&self, test: &str) {
        eprintln!(
            "g5_metrics test={test} engine_surfaces_verified={} one_csn_checks={} prior_snapshot_checks={} conflict_checks={} rollback_checks={} interruption_boundaries={} reopen_checks={}",
            self.engine_surfaces_verified,
            self.one_csn_checks,
            self.prior_snapshot_checks,
            self.conflict_checks,
            self.rollback_checks,
            self.interruption_boundaries,
            self.reopen_checks,
        );
    }
}

fn ann_config() -> Result<HnswConfig, TestError> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x4735_414c_4c45_4e47)?)
}

fn ann_options() -> Result<AnnSearchOptions, TestError> {
    Ok(AnnSearchOptions::new(1, 8, Some(1))?)
}

fn seed_database(path: &Path) -> Result<SeededDatabase, TestError> {
    let mut database = NativeDatabase::create(path)?;
    let ids = Identities {
        lexical: ObjectId::new(LEXICAL_INDEX_ID)?,
        vectors: ObjectId::new(VECTOR_INDEX_ID)?,
        object: ObjectId::new(OBJECT_ID)?,
    };
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE events (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
        &[],
    )?;
    seed.create_search_index(ids.lexical, "documents")?;
    seed.create_vector_index(
        ids.vectors,
        "embeddings",
        2,
        VectorMetric::Cosine,
        ann_config()?,
    )?;
    let receipt = seed.commit()?;
    let prior = database.snapshot(20)?;
    Ok(SeededDatabase {
        database,
        prior,
        ids,
        seed_csn: receipt.commit_csn,
    })
}

fn stage_all_surfaces(
    batch: &mut NativeWriteBatch,
    ids: Identities,
    body: &str,
    value: &[u8],
    vector: [f32; 2],
) -> Result<(), TestError> {
    assert_eq!(
        batch.execute_sql_dml(
            "INSERT INTO events (id, body) VALUES (?, ?)",
            &[ScalarValue::Signed(1), ScalarValue::Text(body.to_owned()),],
        )?,
        SqlResult::Command {
            rows_affected: 1,
            object_id: None,
        }
    );
    batch.set(b"g5-key".to_vec(), value.to_vec(), None)?;
    batch.index_document(ids.lexical, ids.object.get().to_be_bytes().to_vec(), body)?;
    batch.upsert_vector(ids.vectors, ids.object, Vector::new(vector)?)?;
    Ok(())
}

fn observe(
    snapshot: &NativeSnapshot,
    ids: Identities,
    lexical_token: &str,
    vector: [f32; 2],
) -> Result<Observation, TestError> {
    let prepared = snapshot.prepare_sql(SELECT_EVENT)?;
    let SqlResult::Rows { rows, .. } =
        snapshot.execute_prepared(&prepared, &[ScalarValue::Signed(1)])?
    else {
        return Err("embedded G5 SELECT did not return rows".into());
    };
    let relational = rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            ScalarValue::Text(value) => Some(value.clone()),
            _ => None,
        });
    let lexical = snapshot
        .match_text(ids.lexical, lexical_token, 1)?
        .first()
        .is_some_and(|hit| hit.document_id == ids.object.get().to_be_bytes());
    let ann_receipt = snapshot.search_ann(ids.vectors, &Vector::new(vector)?, ann_options()?)?;
    assert_eq!(ann_receipt.snapshot_csn, snapshot.visible_csn());
    let ann = ann_receipt
        .hits
        .first()
        .is_some_and(|hit| hit.object_id == ids.object);
    Ok(Observation {
        relational,
        structure: snapshot.get(b"g5-key").map(<[u8]>::to_vec),
        lexical,
        ann,
    })
}

fn absent_observation() -> Observation {
    Observation {
        relational: None,
        structure: None,
        lexical: false,
        ann: false,
    }
}

fn present_observation(body: &str, value: &[u8]) -> Observation {
    Observation {
        relational: Some(body.to_owned()),
        structure: Some(value.to_vec()),
        lexical: true,
        ann: true,
    }
}

fn commit_boundaries() -> [CommitBoundary; INTERRUPTION_BOUNDARIES] {
    [
        CommitBoundary::BlobStaged,
        CommitBoundary::BlobPromoted,
        CommitBoundary::PageAppended,
        CommitBoundary::PageSynchronized,
        CommitBoundary::WalAppended,
        CommitBoundary::WalSynchronized,
        CommitBoundary::RootPublished,
    ]
}

fn boundary_recovers_commit(boundary: CommitBoundary) -> bool {
    matches!(
        boundary,
        CommitBoundary::WalAppended
            | CommitBoundary::WalSynchronized
            | CommitBoundary::RootPublished
    )
}

#[test]
fn embedded_commit_has_one_csn_prior_snapshot_isolation_and_reopen() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let SeededDatabase {
        mut database,
        prior,
        ids,
        seed_csn,
    } = seed_database(&data)?;
    let mut batch = database.begin_optimistic(20, DurabilityClass::Strict)?;
    assert_eq!(batch.read_csn(), Some(seed_csn));
    stage_all_surfaces(&mut batch, ids, "g5 committed", b"committed", [1.0, 0.0])?;
    let committed = database.commit_optimistic(batch)?;

    assert_eq!(committed.commit_csn.get(), seed_csn.get() + 1);
    assert_eq!(
        observe(&prior, ids, "committed", [1.0, 0.0])?,
        absent_observation()
    );
    drop(database);
    let reopened = NativeDatabase::open(&data)?;
    let current = reopened.snapshot(20)?;
    assert_eq!(current.visible_csn(), Some(committed.commit_csn));
    assert_eq!(
        observe(&current, ids, "committed", [1.0, 0.0])?,
        present_observation("g5 committed", b"committed")
    );

    let metrics = G5Metrics {
        engine_surfaces_verified: ENGINE_SURFACES,
        one_csn_checks: ENGINE_SURFACES,
        prior_snapshot_checks: ENGINE_SURFACES,
        reopen_checks: 1,
        ..G5Metrics::default()
    };
    assert_eq!(metrics.engine_surfaces_verified, 4);
    metrics.report("commit");
    Ok(())
}

#[test]
fn embedded_conflict_and_rollback_publish_no_partial_surface() -> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let SeededDatabase {
        mut database,
        ids,
        seed_csn,
        ..
    } = seed_database(&data)?;
    let mut winner = database.begin_optimistic(20, DurabilityClass::Strict)?;
    let mut loser = database.begin_optimistic(20, DurabilityClass::Strict)?;
    stage_all_surfaces(&mut winner, ids, "winner token", b"winner", [1.0, 0.0])?;
    stage_all_surfaces(&mut loser, ids, "loser token", b"loser", [0.0, 1.0])?;
    let committed = database.commit_optimistic(winner)?;
    assert!(matches!(
        database.commit_optimistic(loser),
        Err(NativeRuntimeError::WriteConflict(_))
    ));

    let mut rolled_back = database.begin_optimistic(21, DurabilityClass::Strict)?;
    rolled_back.execute_sql_dml(
        "INSERT INTO events (id, body) VALUES (?, ?)",
        &[
            ScalarValue::Signed(2),
            ScalarValue::Text("rollback token".to_owned()),
        ],
    )?;
    rolled_back.set(b"rollback-key".to_vec(), b"rollback".to_vec(), None)?;
    rolled_back.index_document(ids.lexical, b"rollback-doc".to_vec(), "rollback token")?;
    rolled_back.upsert_vector(
        ids.vectors,
        ObjectId::new(OBJECT_ID + 1)?,
        Vector::new([0.0, 1.0])?,
    )?;
    rolled_back.rollback();
    drop(database);

    let reopened = NativeDatabase::open(&data)?;
    let snapshot = reopened.snapshot(21)?;
    assert_eq!(snapshot.visible_csn(), Some(committed.commit_csn));
    assert_eq!(committed.commit_csn.get(), seed_csn.get() + 1);
    assert_eq!(
        observe(&snapshot, ids, "winner", [1.0, 0.0])?,
        present_observation("winner token", b"winner")
    );
    assert!(
        snapshot
            .match_text(ids.lexical, "loser rollback", 10)?
            .is_empty()
    );
    assert_eq!(snapshot.get(b"rollback-key"), None);
    let rollback_select = snapshot.prepare_sql("SELECT body FROM events WHERE id = 2")?;
    let SqlResult::Rows { rows, .. } = snapshot.execute_prepared(&rollback_select, &[])? else {
        return Err("rollback SELECT did not return rows".into());
    };
    assert!(rows.is_empty());
    assert_eq!(
        snapshot
            .search_vector_exact(ids.vectors, &Vector::new([0.0, 1.0])?, 10,)?
            .into_iter()
            .map(|hit| hit.object_id)
            .collect::<Vec<_>>(),
        [ids.object]
    );

    let metrics = G5Metrics {
        engine_surfaces_verified: ENGINE_SURFACES * 2,
        one_csn_checks: ENGINE_SURFACES,
        conflict_checks: ENGINE_SURFACES,
        rollback_checks: ENGINE_SURFACES,
        reopen_checks: 1,
        ..G5Metrics::default()
    };
    assert_eq!(metrics.conflict_checks, 4);
    assert_eq!(metrics.rollback_checks, 4);
    metrics.report("conflict_rollback");
    Ok(())
}

#[test]
fn embedded_seven_boundaries_reopen_to_prior_or_complete_four_surface_commit()
-> Result<(), TestError> {
    let temporary = TemporaryDirectory::create()?;
    let mut reopened_checks = 0;
    for (ordinal, boundary) in commit_boundaries().into_iter().enumerate() {
        let data = temporary.path().join(format!("boundary-{ordinal}"));
        let SeededDatabase {
            mut database,
            ids,
            seed_csn,
            ..
        } = seed_database(&data)?;
        let mut batch = database.begin_optimistic(20, DurabilityClass::Strict)?;
        stage_all_surfaces(&mut batch, ids, "boundary token", b"boundary", [1.0, 0.0])?;
        assert!(matches!(
            database.commit_optimistic_with_interruption(batch, boundary),
            Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary
        ));
        drop(database);

        let reopened = NativeDatabase::open(&data)?;
        let snapshot = reopened.snapshot(20)?;
        let recovered = boundary_recovers_commit(boundary);
        let expected = if recovered {
            present_observation("boundary token", b"boundary")
        } else {
            absent_observation()
        };
        assert_eq!(
            observe(&snapshot, ids, "boundary", [1.0, 0.0])?,
            expected,
            "mixed four-surface recovery at {boundary:?}"
        );
        assert_eq!(
            snapshot.visible_csn().map(Csn::get),
            Some(seed_csn.get() + u64::from(recovered)),
            "wrong recovered CSN at {boundary:?}"
        );
        reopened_checks += 1;
    }

    let metrics = G5Metrics {
        engine_surfaces_verified: ENGINE_SURFACES * INTERRUPTION_BOUNDARIES,
        one_csn_checks: ENGINE_SURFACES * INTERRUPTION_BOUNDARIES,
        interruption_boundaries: INTERRUPTION_BOUNDARIES,
        reopen_checks: reopened_checks,
        ..G5Metrics::default()
    };
    assert_eq!(metrics.interruption_boundaries, 7);
    assert_eq!(metrics.reopen_checks, 7);
    metrics.report("boundaries");
    Ok(())
}
