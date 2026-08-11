// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded G5 concurrent-reader consistency harness across all native engines.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use hyphae_native_runtime::{NativeDatabase, NativeSnapshot, SnapshotPinId, SqlResult};
use hyphae_native_types::{Csn, DurabilityClass, ObjectId, ScalarValue};
use serde::Serialize;

type TestError = Box<dyn std::error::Error + Send + Sync>;

const READER_THREADS: usize = 2;
const GENERATIONS: u64 = 6;
const RETAINED_PER_READER: usize = 2;
const INDEX_ID: u128 = 500;
const PIN_GENERATION: u64 = 3;
const SELECT_GENERATION: &str = "SELECT generation FROM state WHERE id = 1";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, TestError> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hy-g5-concurrent-reader-{}-{sequence}",
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
struct ReaderMetrics {
    snapshots_checked: u64,
    engine_observations: u64,
    retained_rechecks: u64,
    concurrent_publication_checks: u64,
    mixed_generation: u64,
    mixed_csn: u64,
}

impl ReaderMetrics {
    fn merge(&mut self, other: &Self) {
        self.snapshots_checked += other.snapshots_checked;
        self.engine_observations += other.engine_observations;
        self.retained_rechecks += other.retained_rechecks;
        self.concurrent_publication_checks += other.concurrent_publication_checks;
        self.mixed_generation += other.mixed_generation;
        self.mixed_csn += other.mixed_csn;
    }
}

#[derive(Serialize)]
struct HarnessMetrics {
    schema: &'static str,
    writer_commits: u64,
    reader_threads: usize,
    snapshots_checked: u64,
    engine_observations: u64,
    retained_rechecks: u64,
    concurrent_publication_checks: u64,
    pinned_snapshots_checked: u64,
    reopened_snapshots_checked: u64,
    mixed_generation: u64,
    mixed_csn: u64,
    first_csn: u64,
    last_csn: u64,
    elapsed_millis: u128,
}

#[derive(Debug)]
struct ReadTask {
    snapshot: NativeSnapshot,
    generation: u64,
    csn: Csn,
    publication_barriers: Option<(Arc<Barrier>, Arc<Barrier>)>,
}

fn token(generation: u64) -> String {
    format!("generationtoken{generation}")
}

fn seed(path: &Path, index: ObjectId) -> Result<(NativeDatabase, Csn), TestError> {
    let mut database = NativeDatabase::create(path)?;
    let mut transaction = database.begin(0, DurabilityClass::Strict)?;
    transaction.execute_sql(
        "CREATE TABLE state (id BIGINT PRIMARY KEY, generation BIGINT NOT NULL)",
        &[],
    )?;
    transaction.execute_sql("INSERT INTO state (id, generation) VALUES (1, 0)", &[])?;
    transaction.set(b"generation".to_vec(), 0_u64.to_le_bytes().to_vec(), None)?;
    transaction.create_search_index(index, "generation_documents")?;
    transaction.index_document(index, b"generation-document".to_vec(), token(0))?;
    let receipt = transaction.commit()?;
    Ok((database, receipt.commit_csn))
}

fn commit_generation(
    database: &mut NativeDatabase,
    index: ObjectId,
    generation: u64,
) -> Result<Csn, TestError> {
    let signed_generation = i64::try_from(generation)?;
    let mut transaction = database.begin(signed_generation, DurabilityClass::Strict)?;
    assert_eq!(
        transaction.execute_sql(
            "UPDATE state SET generation = ? WHERE id = 1",
            &[ScalarValue::Signed(signed_generation)],
        )?,
        SqlResult::Command {
            rows_affected: 1,
            object_id: None,
        }
    );
    transaction.set(
        b"generation".to_vec(),
        generation.to_le_bytes().to_vec(),
        None,
    )?;
    transaction.replace_document(index, b"generation-document".to_vec(), token(generation))?;
    Ok(transaction.commit()?.commit_csn)
}

fn observe_sql(snapshot: &NativeSnapshot) -> Result<Option<u64>, TestError> {
    let prepared = snapshot.prepare_sql(SELECT_GENERATION)?;
    let SqlResult::Rows { rows, .. } = snapshot.execute_prepared(&prepared, &[])? else {
        return Ok(None);
    };
    let Some([ScalarValue::Signed(generation)]) = rows.first().map(Vec::as_slice) else {
        return Ok(None);
    };
    Ok(u64::try_from(*generation).ok())
}

fn observe_structure(snapshot: &NativeSnapshot) -> Option<u64> {
    let bytes: [u8; 8] = snapshot.get(b"generation")?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

fn observe_search(snapshot: &NativeSnapshot, index: ObjectId) -> Result<Option<u64>, TestError> {
    let mut found = None;
    for generation in 0..=GENERATIONS {
        if !snapshot
            .match_text(index, &token(generation), 2)?
            .is_empty()
            && found.replace(generation).is_some()
        {
            return Ok(None);
        }
    }
    Ok(found)
}

fn audit_task(
    task: &ReadTask,
    index: ObjectId,
    metrics: &mut ReaderMetrics,
) -> Result<(), TestError> {
    metrics.snapshots_checked += 1;
    let csn_before = task.snapshot.visible_csn();
    let relational_generation = observe_sql(&task.snapshot)?;
    metrics.engine_observations += 1;

    if let Some((entered, published)) = &task.publication_barriers {
        entered.wait();
        published.wait();
        metrics.concurrent_publication_checks += 1;
    }

    let structure_generation = observe_structure(&task.snapshot);
    let search_generation = observe_search(&task.snapshot, index)?;
    metrics.engine_observations += 2;
    if relational_generation != Some(task.generation)
        || structure_generation != Some(task.generation)
        || search_generation != Some(task.generation)
    {
        metrics.mixed_generation += 1;
    }
    if csn_before != Some(task.csn) || task.snapshot.visible_csn() != Some(task.csn) {
        metrics.mixed_csn += 1;
    }
    Ok(())
}

fn reader(receiver: mpsc::Receiver<ReadTask>, index: ObjectId) -> Result<ReaderMetrics, TestError> {
    let mut metrics = ReaderMetrics::default();
    let mut retained = VecDeque::with_capacity(RETAINED_PER_READER);
    for task in receiver {
        audit_task(&task, index, &mut metrics)?;
        if task.publication_barriers.is_none()
            && let Some(historical) = retained.front()
        {
            audit_task(historical, index, &mut metrics)?;
            metrics.retained_rechecks += 1;
        }
        if task.publication_barriers.is_none() {
            retained.push_back(task);
            if retained.len() > RETAINED_PER_READER {
                retained.pop_front();
            }
        }
        thread::yield_now();
    }
    for historical in &retained {
        audit_task(historical, index, &mut metrics)?;
        metrics.retained_rechecks += 1;
    }
    Ok(metrics)
}

#[test]
#[allow(clippy::too_many_lines)]
fn concurrent_readers_never_observe_mixed_generation_or_csn() -> Result<(), TestError> {
    let started = Instant::now();
    let temporary = TemporaryDirectory::create()?;
    let data = temporary.path().join("data");
    let index = ObjectId::new(INDEX_ID)?;
    let pin_id = SnapshotPinId::new(1)?;
    let (mut database, seed_csn) = seed(&data, index)?;
    let first_csn = seed_csn.get();
    let mut last_csn = seed_csn;
    let mut aggregate = ReaderMetrics::default();

    thread::scope(|scope| -> Result<(), TestError> {
        let mut senders = Vec::with_capacity(READER_THREADS);
        let mut readers = Vec::with_capacity(READER_THREADS);
        for _ in 0..READER_THREADS {
            let (sender, receiver) = mpsc::channel();
            senders.push(sender);
            readers.push(scope.spawn(move || reader(receiver, index)));
        }

        let entered = Arc::new(Barrier::new(READER_THREADS + 1));
        let published = Arc::new(Barrier::new(READER_THREADS + 1));
        last_csn = commit_generation(&mut database, index, 1)?;
        let crossing_snapshot = database.snapshot(1)?;
        for sender in &senders {
            sender.send(ReadTask {
                snapshot: crossing_snapshot.clone(),
                generation: 1,
                csn: last_csn,
                publication_barriers: Some((Arc::clone(&entered), Arc::clone(&published))),
            })?;
        }
        entered.wait();
        last_csn = commit_generation(&mut database, index, 2)?;
        published.wait();

        for generation in 2..=GENERATIONS {
            if generation > 2 {
                last_csn = commit_generation(&mut database, index, generation)?;
            }
            let snapshot = database.snapshot(i64::try_from(generation)?)?;
            for sender in &senders {
                sender.send(ReadTask {
                    snapshot: snapshot.clone(),
                    generation,
                    csn: last_csn,
                    publication_barriers: None,
                })?;
            }
            if generation == PIN_GENERATION {
                let pin = database.pin_current(pin_id, i64::try_from(generation)?)?;
                assert_eq!(pin.visible_csn, last_csn);
            }
        }
        drop(senders);
        for handle in readers {
            let reader_metrics = handle
                .join()
                .map_err(|_| std::io::Error::other("G5 reader panicked"))??;
            aggregate.merge(&reader_metrics);
        }
        Ok(())
    })?;

    drop(database);
    let reopened = NativeDatabase::open(&data)?;
    let mut durable_metrics = ReaderMetrics::default();
    audit_task(
        &ReadTask {
            snapshot: reopened.snapshot(i64::try_from(GENERATIONS)?)?,
            generation: GENERATIONS,
            csn: last_csn,
            publication_barriers: None,
        },
        index,
        &mut durable_metrics,
    )?;
    let pinned_csn = Csn::new(
        first_csn
            .checked_add(PIN_GENERATION)
            .ok_or("pinned CSN overflow")?,
    )?;
    audit_task(
        &ReadTask {
            snapshot: reopened.open_pinned_snapshot(pin_id)?,
            generation: PIN_GENERATION,
            csn: pinned_csn,
            publication_barriers: None,
        },
        index,
        &mut durable_metrics,
    )?;
    aggregate.merge(&durable_metrics);

    let metrics = HarnessMetrics {
        schema: "hyphae.native.g5-concurrent-reader-consistency.v1",
        writer_commits: GENERATIONS,
        reader_threads: READER_THREADS,
        snapshots_checked: aggregate.snapshots_checked,
        engine_observations: aggregate.engine_observations,
        retained_rechecks: aggregate.retained_rechecks,
        concurrent_publication_checks: aggregate.concurrent_publication_checks,
        pinned_snapshots_checked: 1,
        reopened_snapshots_checked: 2,
        mixed_generation: aggregate.mixed_generation,
        mixed_csn: aggregate.mixed_csn,
        first_csn,
        last_csn: last_csn.get(),
        elapsed_millis: started.elapsed().as_millis(),
    };
    println!("{}", serde_json::to_string(&metrics)?);

    assert_eq!(
        metrics.concurrent_publication_checks,
        u64::try_from(READER_THREADS)?
    );
    assert!(metrics.retained_rechecks >= u64::try_from(READER_THREADS)?);
    assert_eq!(metrics.mixed_generation, 0);
    assert_eq!(metrics.mixed_csn, 0);
    Ok(())
}
