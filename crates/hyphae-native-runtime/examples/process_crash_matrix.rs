// SPDX-License-Identifier: GPL-3.0-only

//! Process-level crash matrix for native singleton commit and checkpoint paths.
//!
//! The parent starts one child per boundary. The child reaches the deterministic
//! interruption while retaining the database handle and writer lock, signals
//! readiness through stdout, and parks. The parent then hard-kills the child,
//! reopens the directory, and validates transaction atomicity plus checkpoint
//! manifest/WAL authority.

use std::{
    error::Error,
    fs,
    io::{self, BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hyphae_native_runtime::{
    CheckpointBoundary, CommitBoundary, NativeDatabase, NativeRuntimeError, NativeTransaction,
    PromotionBoundary, SnapshotPinBoundary, SnapshotPinId, Ttl,
};
use hyphae_native_types::{Csn, DurabilityClass, ObjectId};

const CHILD_MODE: &str = "--child";
const SEED_CHECKPOINT_MODE: &str = "--seed-checkpoint";
const VERIFY_POWER_LOSS_MODE: &str = "--verify-power-loss";
const READY_PREFIX: &str = "hyphae-native-crash-ready:";
const COMMIT_FAMILY: &str = "commit";
const CHECKPOINT_FAMILY: &str = "checkpoint";
const SNAPSHOT_PIN_FAMILY: &str = "snapshot-pin";
const PROMOTION_FAMILY: &str = "promotion";
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(10);
const LARGE_VALUE_BYTES: usize = 16 * 1024;
const TABLE_ID: u128 = 1;
const SEARCH_INDEX_ID: u128 = 2;

const COMMIT_BOUNDARIES: [(&str, CommitBoundary); 7] = [
    ("blob-staged", CommitBoundary::BlobStaged),
    ("blob-promoted", CommitBoundary::BlobPromoted),
    ("page-appended", CommitBoundary::PageAppended),
    ("page-synchronized", CommitBoundary::PageSynchronized),
    ("wal-appended", CommitBoundary::WalAppended),
    ("wal-synchronized", CommitBoundary::WalSynchronized),
    ("root-published", CommitBoundary::RootPublished),
];

const CHECKPOINT_BOUNDARIES: [(&str, CheckpointBoundary); 4] = [
    ("manifest-staged", CheckpointBoundary::ManifestStaged),
    ("manifest-published", CheckpointBoundary::ManifestPublished),
    ("wal-appended", CheckpointBoundary::WalAppended),
    ("wal-synchronized", CheckpointBoundary::WalSynchronized),
];

const SNAPSHOT_PIN_BOUNDARIES: [(&str, SnapshotPinBoundary); 2] = [
    (
        "record-synchronized",
        SnapshotPinBoundary::RecordSynchronized,
    ),
    ("record-published", SnapshotPinBoundary::RecordPublished),
];
const SNAPSHOT_PIN_ID: u128 = 1;
const PROMOTION_BOUNDARIES: [(&str, PromotionBoundary); 3] = [
    ("before-rename", PromotionBoundary::BeforeRename),
    ("marker-renamed", PromotionBoundary::MarkerRenamed),
    ("parent-synchronized", PromotionBoundary::ParentSynchronized),
];

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-native-process-crash-{}-{timestamp}",
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

struct CommitObservation {
    name: &'static str,
    expected_state: &'static str,
    recovered_csn: Option<u64>,
    recovered_blob_count: usize,
    termination: String,
}

struct CheckpointObservation {
    name: &'static str,
    manifest_count: usize,
    checkpoint_count: usize,
    unanchored_manifest_suffix: usize,
    recovered_temporary_manifests: usize,
    termination: String,
}

struct SnapshotPinObservation {
    name: &'static str,
    expected_pin: &'static str,
    recovered_pin_count: usize,
    pin_directory_files: usize,
    retained_page_generations: usize,
    termination: String,
}

struct PromotionObservation {
    name: &'static str,
    expected_marker: &'static str,
    termination: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let first = arguments
        .next()
        .ok_or_else(|| failure("missing source commit or child mode"))?;
    if first == CHILD_MODE {
        let family = arguments
            .next()
            .ok_or_else(|| failure("child mode requires a boundary family"))?
            .into_string()
            .map_err(|_| failure("boundary family is not valid UTF-8"))?;
        let directory = arguments
            .next()
            .ok_or_else(|| failure("child mode requires a data directory"))?;
        let boundary = arguments
            .next()
            .ok_or_else(|| failure("child mode requires a boundary"))?
            .into_string()
            .map_err(|_| failure("boundary is not valid UTF-8"))?;
        require_no_remaining(arguments)?;
        return run_child(&family, Path::new(&directory), &boundary);
    }
    if first == SEED_CHECKPOINT_MODE {
        let directory = arguments
            .next()
            .ok_or_else(|| failure("checkpoint seeding requires a data directory"))?;
        require_no_remaining(arguments)?;
        return seed_checkpoint_directory(Path::new(&directory));
    }
    if first == VERIFY_POWER_LOSS_MODE {
        let family = arguments
            .next()
            .ok_or_else(|| failure("power-loss verification requires a boundary family"))?
            .into_string()
            .map_err(|_| failure("boundary family is not valid UTF-8"))?;
        let directory = arguments
            .next()
            .ok_or_else(|| failure("power-loss verification requires a data directory"))?;
        let boundary = arguments
            .next()
            .ok_or_else(|| failure("power-loss verification requires a boundary"))?
            .into_string()
            .map_err(|_| failure("boundary is not valid UTF-8"))?;
        require_no_remaining(arguments)?;
        return verify_power_loss_recovery(&family, Path::new(&directory), &boundary);
    }

    let source_commit = first
        .into_string()
        .map_err(|_| failure("source commit is not valid UTF-8"))?;
    let environment = arguments
        .next()
        .ok_or_else(|| failure("missing environment label"))?
        .into_string()
        .map_err(|_| failure("environment label is not valid UTF-8"))?;
    require_no_remaining(arguments)?;
    validate_receipt_label("source commit", &source_commit)?;
    validate_receipt_label("environment", &environment)?;
    run_parent(&source_commit, &environment)
}

fn require_no_remaining(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), Box<dyn Error>> {
    if arguments.next().is_some() {
        return Err(failure("unexpected additional argument"));
    }
    Ok(())
}

fn run_child(family: &str, directory: &Path, boundary_name: &str) -> Result<(), Box<dyn Error>> {
    match family {
        COMMIT_FAMILY => run_commit_child(directory, boundary_name)?,
        CHECKPOINT_FAMILY => run_checkpoint_child(directory, boundary_name)?,
        SNAPSHOT_PIN_FAMILY => run_snapshot_pin_child(directory, boundary_name)?,
        PROMOTION_FAMILY => run_promotion_child(directory, boundary_name)?,
        other => return Err(failure(format!("unknown boundary family: {other}"))),
    }

    println!("{READY_PREFIX}{family}:{boundary_name}");
    io::stdout().flush()?;
    loop {
        thread::park();
    }
}

fn run_commit_child(directory: &Path, boundary_name: &str) -> Result<(), Box<dyn Error>> {
    let boundary = parse_commit_boundary(boundary_name)
        .ok_or_else(|| failure(format!("unknown commit boundary: {boundary_name}")))?;
    let mut database = NativeDatabase::create(directory)?;
    let transaction = stage_vertical(&mut database)?;
    match transaction.commit_with_interruption(boundary) {
        Err(NativeRuntimeError::InjectedCrash(found)) if found == boundary => {}
        other => {
            return Err(failure(format!(
                "boundary {boundary_name} returned an unexpected result: {other:?}"
            )));
        }
    }
    Ok(())
}

fn run_checkpoint_child(directory: &Path, boundary_name: &str) -> Result<(), Box<dyn Error>> {
    let boundary = parse_checkpoint_boundary(boundary_name)
        .ok_or_else(|| failure(format!("unknown checkpoint boundary: {boundary_name}")))?;
    let mut database = NativeDatabase::open(directory)?;
    match database.checkpoint_with_interruption(boundary) {
        Err(NativeRuntimeError::InjectedCheckpointCrash(found)) if found == boundary => {}
        other => {
            return Err(failure(format!(
                "checkpoint boundary {boundary_name} returned an unexpected result: {other:?}"
            )));
        }
    }
    Ok(())
}

fn run_snapshot_pin_child(directory: &Path, boundary_name: &str) -> Result<(), Box<dyn Error>> {
    let boundary = parse_snapshot_pin_boundary(boundary_name)
        .ok_or_else(|| failure(format!("unknown snapshot-pin boundary: {boundary_name}")))?;
    let mut database = NativeDatabase::create(directory)?;
    stage_vertical(&mut database)?.commit()?;
    let pin = SnapshotPinId::new(SNAPSHOT_PIN_ID)?;
    match database.pin_current_with_interruption(pin, 150, boundary) {
        Err(NativeRuntimeError::InjectedSnapshotPinCrash(found)) if found == boundary => {}
        other => {
            return Err(failure(format!(
                "snapshot-pin boundary {boundary_name} returned an unexpected result: {other:?}"
            )));
        }
    }
    Ok(())
}

fn run_promotion_child(directory: &Path, boundary_name: &str) -> Result<(), Box<dyn Error>> {
    let boundary = parse_promotion_boundary(boundary_name)
        .ok_or_else(|| failure(format!("unknown promotion boundary: {boundary_name}")))?;
    let mut database = NativeDatabase::create_pending(directory)?;
    match database.promote_pending_with_interruption(boundary) {
        Err(NativeRuntimeError::Directory(
            hyphae_native_runtime::NativeDirectoryError::InjectedPromotionCrash(found),
        )) if found == boundary => {}
        other => {
            return Err(failure(format!(
                "promotion boundary {boundary_name} returned an unexpected result: {other:?}"
            )));
        }
    }
    Ok(())
}

fn run_parent(source_commit: &str, environment: &str) -> Result<(), Box<dyn Error>> {
    let executable = std::env::current_exe()?;
    let temporary = TemporaryDirectory::create()?;
    fs::create_dir_all(temporary.path())?;
    let commit_observations = run_commit_matrix(&executable, temporary.path())?;
    let checkpoint_observations = run_checkpoint_matrix(&executable, temporary.path())?;
    let snapshot_pin_observations = run_snapshot_pin_matrix(&executable, temporary.path())?;
    let promotion_observations = run_promotion_matrix(&executable, temporary.path())?;
    print_receipt(
        source_commit,
        environment,
        &commit_observations,
        &checkpoint_observations,
        &snapshot_pin_observations,
        &promotion_observations,
    );
    Ok(())
}

fn run_commit_matrix(
    executable: &Path,
    root: &Path,
) -> Result<Vec<CommitObservation>, Box<dyn Error>> {
    let mut observations = Vec::with_capacity(COMMIT_BOUNDARIES.len());
    for (name, boundary) in COMMIT_BOUNDARIES {
        let directory = root.join(format!("{COMMIT_FAMILY}-{name}"));
        let termination = kill_child_at_boundary(executable, COMMIT_FAMILY, &directory, name)?;
        let database = NativeDatabase::open(&directory)?;
        let expected_complete = expects_complete_state(boundary);
        validate_recovered_state(&database, expected_complete)?;
        observations.push(CommitObservation {
            name,
            expected_state: if expected_complete {
                "complete-csn-1"
            } else {
                "prior-empty"
            },
            recovered_csn: database.recovery_report().visible_csn.map(Csn::get),
            recovered_blob_count: database.recovery_report().blob_count,
            termination,
        });
    }
    Ok(observations)
}

fn run_checkpoint_matrix(
    executable: &Path,
    root: &Path,
) -> Result<Vec<CheckpointObservation>, Box<dyn Error>> {
    let mut observations = Vec::with_capacity(CHECKPOINT_BOUNDARIES.len());
    for (name, boundary) in CHECKPOINT_BOUNDARIES {
        let directory = root.join(format!("{CHECKPOINT_FAMILY}-{name}"));
        seed_checkpoint_directory(&directory)?;
        let termination = kill_child_at_boundary(executable, CHECKPOINT_FAMILY, &directory, name)?;
        let database = NativeDatabase::open(&directory)?;
        validate_recovered_state(&database, true)?;
        validate_checkpoint_recovery(&database, boundary)?;
        let report = database.recovery_report();
        observations.push(CheckpointObservation {
            name,
            manifest_count: report.manifest_count,
            checkpoint_count: report.checkpoint_count,
            unanchored_manifest_suffix: report.unanchored_manifest_suffix,
            recovered_temporary_manifests: report.recovered_temporary_manifests,
            termination,
        });
    }
    Ok(observations)
}

fn run_snapshot_pin_matrix(
    executable: &Path,
    root: &Path,
) -> Result<Vec<SnapshotPinObservation>, Box<dyn Error>> {
    let mut observations = Vec::with_capacity(SNAPSHOT_PIN_BOUNDARIES.len());
    for (name, boundary) in SNAPSHOT_PIN_BOUNDARIES {
        let directory = root.join(format!("{SNAPSHOT_PIN_FAMILY}-{name}"));
        let termination =
            kill_child_at_boundary(executable, SNAPSHOT_PIN_FAMILY, &directory, name)?;
        let database = NativeDatabase::open(&directory)?;
        validate_recovered_state(&database, true)?;
        let expected_pin = boundary == SnapshotPinBoundary::RecordPublished;
        let expected_count = usize::from(expected_pin);
        require_equal(
            "recovered snapshot pin count",
            &database.snapshot_pin_count(),
            &expected_count,
        )?;
        validate_recovered_snapshot_pin(&database, expected_pin)?;
        let pin_directory_files = fs::read_dir(directory.join("pins"))?
            .filter_map(Result::ok)
            .count();
        require_equal(
            "snapshot pin directory file count",
            &pin_directory_files,
            &expected_count,
        )?;
        let retained_page_generations = fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| {
                name == "pages.hydb"
                    || name
                        .strip_suffix(".hydb")
                        .is_some_and(|stem| stem.starts_with("pages-"))
            })
            .count();
        require_equal("retained page generations", &retained_page_generations, &1)?;
        observations.push(SnapshotPinObservation {
            name,
            expected_pin: if expected_pin { "complete" } else { "absent" },
            recovered_pin_count: database.snapshot_pin_count(),
            pin_directory_files,
            retained_page_generations,
            termination,
        });
    }
    Ok(observations)
}

fn run_promotion_matrix(
    executable: &Path,
    root: &Path,
) -> Result<Vec<PromotionObservation>, Box<dyn Error>> {
    let mut observations = Vec::with_capacity(PROMOTION_BOUNDARIES.len());
    for (name, boundary) in PROMOTION_BOUNDARIES {
        let directory = root.join(format!("{PROMOTION_FAMILY}-{name}"));
        let termination = kill_child_at_boundary(executable, PROMOTION_FAMILY, &directory, name)?;
        let expects_pending = boundary == PromotionBoundary::BeforeRename;
        validate_promotion_state(&directory, Some(expects_pending))?;
        observations.push(PromotionObservation {
            name,
            expected_marker: if expects_pending {
                "pending"
            } else {
                "authority"
            },
            termination,
        });
    }
    Ok(observations)
}

fn seed_checkpoint_directory(directory: &Path) -> Result<(), Box<dyn Error>> {
    let mut database = NativeDatabase::create(directory)?;
    stage_vertical(&mut database)?.commit()?;
    Ok(())
}

fn kill_child_at_boundary(
    executable: &Path,
    family: &str,
    directory: &Path,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    let mut child = Command::new(executable)
        .arg(CHILD_MODE)
        .arg(family)
        .arg(directory)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let ready = wait_for_child_ready(&mut child)?;
    let expected_ready = format!("{READY_PREFIX}{family}:{name}");
    if ready.trim_end() != expected_ready {
        stop_child(&mut child);
        return Err(failure(format!(
            "{family} boundary {name} emitted unexpected readiness: {ready:?}"
        )));
    }

    child.kill()?;
    let status = child.wait()?;
    validate_hard_kill(&format!("{family}:{name}"), status)
}

fn stage_vertical(database: &mut NativeDatabase) -> Result<NativeTransaction<'_>, Box<dyn Error>> {
    let table = ObjectId::new(TABLE_ID)?;
    let index = ObjectId::new(SEARCH_INDEX_ID)?;
    let mut transaction = database.begin(100, DurabilityClass::Strict)?;
    transaction.create_relation(table, "accounts")?;
    transaction.insert(table, b"mario".to_vec(), large_value())?;
    transaction.set(b"session".to_vec(), b"open".to_vec(), Some(200))?;
    transaction.create_search_index(index, "notes")?;
    transaction.index_document(index, b"doc-1".to_vec(), "native rust process crash")?;
    Ok(transaction)
}

fn validate_recovered_state(
    database: &NativeDatabase,
    expected_complete: bool,
) -> Result<(), Box<dyn Error>> {
    let table = ObjectId::new(TABLE_ID)?;
    let index = ObjectId::new(SEARCH_INDEX_ID)?;
    let snapshot = database.snapshot(150)?;
    let visible_csn = snapshot.visible_csn().map(Csn::get);

    if expected_complete {
        require_equal("visible CSN", &visible_csn, &Some(1))?;
        require_equal(
            "relational value",
            &snapshot.select(table, b"mario"),
            &Some(large_value().as_slice()),
        )?;
        require_equal(
            "structure value",
            &snapshot.get(b"session"),
            &Some(b"open".as_slice()),
        )?;
        require_equal(
            "structure TTL",
            &snapshot.ttl(b"session"),
            &Ttl::RemainingMicros(50),
        )?;
        let matches = snapshot.match_text(index, "crash", 1)?;
        require_equal("lexical match count", &matches.len(), &1)?;
        require_equal(
            "lexical document",
            &matches[0].document_id.as_slice(),
            &b"doc-1".as_slice(),
        )?;
    } else {
        require_equal("visible CSN", &visible_csn, &None)?;
        require_equal(
            "relational absence",
            &snapshot.select(table, b"mario"),
            &None,
        )?;
        require_equal("structure absence", &snapshot.get(b"session"), &None)?;
        if snapshot.match_text(index, "crash", 1).is_ok() {
            return Err(failure(
                "lexical state became visible without a committed CSN",
            ));
        }
    }
    Ok(())
}

fn validate_recovered_snapshot_pin(
    database: &NativeDatabase,
    expected_present: bool,
) -> Result<(), Box<dyn Error>> {
    let pin = SnapshotPinId::new(SNAPSHOT_PIN_ID)?;
    if !expected_present {
        if !matches!(
            database.open_pinned_snapshot(pin),
            Err(NativeRuntimeError::UnknownSnapshotPin)
        ) {
            return Err(failure("temporary snapshot pin became authority"));
        }
        return Ok(());
    }

    let table = ObjectId::new(TABLE_ID)?;
    let index = ObjectId::new(SEARCH_INDEX_ID)?;
    let snapshot = database.open_pinned_snapshot(pin)?;
    require_equal(
        "pinned visible CSN",
        &snapshot.visible_csn(),
        &Some(Csn::new(1)?),
    )?;
    require_equal(
        "pinned relational value",
        &snapshot.select(table, b"mario"),
        &Some(large_value().as_slice()),
    )?;
    require_equal(
        "pinned structure value",
        &snapshot.get(b"session"),
        &Some(b"open".as_slice()),
    )?;
    require_equal(
        "pinned structure TTL",
        &snapshot.ttl(b"session"),
        &Ttl::RemainingMicros(50),
    )?;
    let matches = snapshot.match_text(index, "crash", 1)?;
    require_equal("pinned lexical match count", &matches.len(), &1)?;
    require_equal(
        "pinned lexical document",
        &matches[0].document_id.as_slice(),
        &b"doc-1".as_slice(),
    )
}

fn validate_checkpoint_recovery(
    database: &NativeDatabase,
    boundary: CheckpointBoundary,
) -> Result<(), Box<dyn Error>> {
    let report = database.recovery_report();
    let expected = match boundary {
        CheckpointBoundary::ManifestStaged => (0, 0, 0, 1),
        CheckpointBoundary::ManifestPublished => (1, 0, 1, 0),
        CheckpointBoundary::WalAppended | CheckpointBoundary::WalSynchronized => (1, 1, 0, 0),
    };
    require_equal("manifest count", &report.manifest_count, &expected.0)?;
    require_equal("checkpoint count", &report.checkpoint_count, &expected.1)?;
    require_equal(
        "unanchored manifest suffix",
        &report.unanchored_manifest_suffix,
        &expected.2,
    )?;
    require_equal(
        "recovered temporary manifests",
        &report.recovered_temporary_manifests,
        &expected.3,
    )
}

fn verify_power_loss_recovery(
    family: &str,
    directory: &Path,
    boundary_name: &str,
) -> Result<(), Box<dyn Error>> {
    match family {
        COMMIT_FAMILY => {
            let database = NativeDatabase::open(directory)?;
            let boundary = parse_commit_boundary(boundary_name)
                .ok_or_else(|| failure(format!("unknown commit boundary: {boundary_name}")))?;
            let expected_complete = expects_power_loss_complete_state(boundary);
            validate_recovered_state(&database, expected_complete)?;
            let report = database.recovery_report();
            let recovered_csn = report.visible_csn.map(Csn::get);
            println!("{{");
            println!("  \"family\": \"{COMMIT_FAMILY}\",");
            println!("  \"boundary\": \"{boundary_name}\",");
            println!(
                "  \"expected_state\": \"{}\",",
                if expected_complete {
                    "complete-csn-1"
                } else {
                    "prior-empty"
                }
            );
            match recovered_csn {
                Some(csn) => println!("  \"recovered_csn\": {csn},"),
                None => println!("  \"recovered_csn\": null,"),
            }
            println!("  \"recovered_blob_count\": {}", report.blob_count);
            println!("}}");
        }
        CHECKPOINT_FAMILY => {
            let database = NativeDatabase::open(directory)?;
            let boundary = parse_checkpoint_boundary(boundary_name)
                .ok_or_else(|| failure(format!("unknown checkpoint boundary: {boundary_name}")))?;
            validate_recovered_state(&database, true)?;
            validate_power_loss_checkpoint_recovery(&database, boundary)?;
            let report = database.recovery_report();
            println!("{{");
            println!("  \"family\": \"{CHECKPOINT_FAMILY}\",");
            println!("  \"boundary\": \"{boundary_name}\",");
            println!("  \"expected_state\": \"complete-csn-1\",");
            println!("  \"recovered_csn\": 1,");
            println!("  \"manifest_count\": {},", report.manifest_count);
            println!("  \"checkpoint_count\": {},", report.checkpoint_count);
            println!(
                "  \"unanchored_manifest_suffix\": {},",
                report.unanchored_manifest_suffix
            );
            println!(
                "  \"recovered_temporary_manifests\": {}",
                report.recovered_temporary_manifests
            );
            println!("}}");
        }
        PROMOTION_FAMILY => {
            let boundary = parse_promotion_boundary(boundary_name)
                .ok_or_else(|| failure(format!("unknown promotion boundary: {boundary_name}")))?;
            let expected_pending = match boundary {
                PromotionBoundary::BeforeRename => Some(true),
                PromotionBoundary::MarkerRenamed => None,
                PromotionBoundary::ParentSynchronized => Some(false),
            };
            let marker = validate_promotion_state(directory, expected_pending)?;
            println!("{{");
            println!("  \"family\": \"{PROMOTION_FAMILY}\",");
            println!("  \"boundary\": \"{boundary_name}\",");
            println!("  \"recovered_marker\": \"{marker}\"");
            println!("}}");
        }
        other => return Err(failure(format!("unknown boundary family: {other}"))),
    }
    Ok(())
}

fn validate_promotion_state(
    directory: &Path,
    expected_pending: Option<bool>,
) -> Result<&'static str, Box<dyn Error>> {
    let has_pending = directory.join("FORMAT.pending").try_exists()?;
    let has_authority = directory.join("FORMAT").try_exists()?;
    if has_pending == has_authority {
        return Err(failure(
            "promotion recovery must expose exactly one FORMAT marker",
        ));
    }
    if let Some(expected) = expected_pending
        && has_pending != expected
    {
        return Err(failure(format!(
            "promotion recovery marker differs: pending={has_pending}, expected={expected}"
        )));
    }
    if has_pending {
        drop(NativeDatabase::open_pending(directory)?);
        Ok("pending")
    } else {
        drop(NativeDatabase::open(directory)?);
        Ok("authority")
    }
}

fn validate_power_loss_checkpoint_recovery(
    database: &NativeDatabase,
    boundary: CheckpointBoundary,
) -> Result<(), Box<dyn Error>> {
    let report = database.recovery_report();
    let (manifest_count, checkpoint_count, unanchored_manifest_suffix) =
        power_loss_checkpoint_counts(boundary);
    require_equal("manifest count", &report.manifest_count, &manifest_count)?;
    require_equal(
        "checkpoint count",
        &report.checkpoint_count,
        &checkpoint_count,
    )?;
    require_equal(
        "unanchored manifest suffix",
        &report.unanchored_manifest_suffix,
        &unanchored_manifest_suffix,
    )?;
    if boundary == CheckpointBoundary::ManifestStaged {
        if report.recovered_temporary_manifests > 1 {
            return Err(failure(format!(
                "staged checkpoint recovered {} temporary manifests, expected at most one",
                report.recovered_temporary_manifests
            )));
        }
    } else {
        require_equal(
            "recovered temporary manifests",
            &report.recovered_temporary_manifests,
            &0,
        )?;
    }
    Ok(())
}

fn power_loss_checkpoint_counts(boundary: CheckpointBoundary) -> (usize, usize, usize) {
    match boundary {
        CheckpointBoundary::ManifestStaged => (0, 0, 0),
        CheckpointBoundary::ManifestPublished | CheckpointBoundary::WalAppended => (1, 0, 1),
        CheckpointBoundary::WalSynchronized => (1, 1, 0),
    }
}

fn wait_for_child_ready(child: &mut Child) -> Result<String, Box<dyn Error>> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("child stdout was not piped"))?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ignored = sender.send(result);
    });

    let result = receiver.recv_timeout(CHILD_READY_TIMEOUT);
    if result.is_err() {
        stop_child(child);
    }
    reader
        .join()
        .map_err(|_| failure("child readiness reader panicked"))?;
    match result {
        Ok(line) => Ok(line?),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(failure("child did not reach its crash boundary in time"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(failure("child readiness channel disconnected"))
        }
    }
}

fn stop_child(child: &mut Child) {
    let _ignored = child.kill();
    let _ignored = child.wait();
}

#[cfg(unix)]
fn validate_hard_kill(name: &str, status: ExitStatus) -> Result<String, Box<dyn Error>> {
    use std::os::unix::process::ExitStatusExt as _;

    if status.signal() != Some(9) {
        return Err(failure(format!(
            "boundary {name} child was not terminated by SIGKILL: {status:?}"
        )));
    }
    Ok("signal-9".to_owned())
}

#[cfg(not(unix))]
fn validate_hard_kill(name: &str, status: ExitStatus) -> Result<String, Box<dyn Error>> {
    if status.success() {
        return Err(failure(format!(
            "boundary {name} child exited successfully instead of being killed"
        )));
    }
    Ok(match status.code() {
        Some(code) => format!("exit-code-{code}"),
        None => "terminated-without-exit-code".to_owned(),
    })
}

fn print_receipt(
    source_commit: &str,
    environment: &str,
    commit_observations: &[CommitObservation],
    checkpoint_observations: &[CheckpointObservation],
    snapshot_pin_observations: &[SnapshotPinObservation],
    promotion_observations: &[PromotionObservation],
) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.process-crash-matrix.v4\",");
    println!("  \"status\": \"process-crash-not-power-loss\",");
    println!("  \"source_commit\": \"{source_commit}\",");
    println!("  \"environment\": \"{environment}\",");
    println!(
        "  \"target\": \"{}-{}\",",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!("  \"durability\": \"strict\",");
    println!("  \"all_engine_csn\": 1,");
    print_commit_observations(commit_observations);
    print_checkpoint_observations(checkpoint_observations);
    print_snapshot_pin_observations(snapshot_pin_observations);
    print_promotion_observations(promotion_observations);
    println!("}}");
}

fn print_commit_observations(observations: &[CommitObservation]) {
    println!("  \"commit_boundaries\": [");
    for (index, observation) in observations.iter().enumerate() {
        let suffix = if index + 1 == observations.len() {
            ""
        } else {
            ","
        };
        println!("    {{");
        println!("      \"boundary\": \"{}\",", observation.name);
        println!(
            "      \"expected_state\": \"{}\",",
            observation.expected_state
        );
        match observation.recovered_csn {
            Some(csn) => println!("      \"recovered_csn\": {csn},"),
            None => println!("      \"recovered_csn\": null,"),
        }
        println!(
            "      \"recovered_blob_count\": {},",
            observation.recovered_blob_count
        );
        println!("      \"termination\": \"{}\"", observation.termination);
        println!("    }}{suffix}");
    }
    println!("  ],");
}

fn print_checkpoint_observations(observations: &[CheckpointObservation]) {
    println!("  \"checkpoint_boundaries\": [");
    for (index, observation) in observations.iter().enumerate() {
        let suffix = if index + 1 == observations.len() {
            ""
        } else {
            ","
        };
        println!("    {{");
        println!("      \"boundary\": \"{}\",", observation.name);
        println!("      \"manifest_count\": {},", observation.manifest_count);
        println!(
            "      \"checkpoint_count\": {},",
            observation.checkpoint_count
        );
        println!(
            "      \"unanchored_manifest_suffix\": {},",
            observation.unanchored_manifest_suffix
        );
        println!(
            "      \"recovered_temporary_manifests\": {},",
            observation.recovered_temporary_manifests
        );
        println!("      \"termination\": \"{}\"", observation.termination);
        println!("    }}{suffix}");
    }
    println!("  ],");
}

fn print_snapshot_pin_observations(observations: &[SnapshotPinObservation]) {
    println!("  \"snapshot_pin_boundaries\": [");
    for (index, observation) in observations.iter().enumerate() {
        let suffix = if index + 1 == observations.len() {
            ""
        } else {
            ","
        };
        println!("    {{");
        println!("      \"boundary\": \"{}\",", observation.name);
        println!("      \"expected_pin\": \"{}\",", observation.expected_pin);
        println!(
            "      \"recovered_pin_count\": {},",
            observation.recovered_pin_count
        );
        println!(
            "      \"pin_directory_files\": {},",
            observation.pin_directory_files
        );
        println!(
            "      \"retained_page_generations\": {},",
            observation.retained_page_generations
        );
        println!("      \"termination\": \"{}\"", observation.termination);
        println!("    }}{suffix}");
    }
    println!("  ],");
}

fn print_promotion_observations(observations: &[PromotionObservation]) {
    println!("  \"promotion_boundaries\": [");
    for (index, observation) in observations.iter().enumerate() {
        let suffix = if index + 1 == observations.len() {
            ""
        } else {
            ","
        };
        println!("    {{");
        println!("      \"boundary\": \"{}\",", observation.name);
        println!(
            "      \"expected_marker\": \"{}\",",
            observation.expected_marker
        );
        println!("      \"termination\": \"{}\"", observation.termination);
        println!("    }}{suffix}");
    }
    println!("  ]");
}

fn parse_commit_boundary(name: &str) -> Option<CommitBoundary> {
    COMMIT_BOUNDARIES
        .iter()
        .find_map(|(candidate, boundary)| (*candidate == name).then_some(*boundary))
}

fn parse_checkpoint_boundary(name: &str) -> Option<CheckpointBoundary> {
    CHECKPOINT_BOUNDARIES
        .iter()
        .find_map(|(candidate, boundary)| (*candidate == name).then_some(*boundary))
}

fn parse_snapshot_pin_boundary(name: &str) -> Option<SnapshotPinBoundary> {
    SNAPSHOT_PIN_BOUNDARIES
        .iter()
        .find_map(|(candidate, boundary)| (*candidate == name).then_some(*boundary))
}

fn parse_promotion_boundary(name: &str) -> Option<PromotionBoundary> {
    PROMOTION_BOUNDARIES
        .iter()
        .find_map(|(candidate, boundary)| (*candidate == name).then_some(*boundary))
}

fn expects_complete_state(boundary: CommitBoundary) -> bool {
    matches!(
        boundary,
        CommitBoundary::WalAppended
            | CommitBoundary::WalSynchronized
            | CommitBoundary::RootPublished
    )
}

fn expects_power_loss_complete_state(boundary: CommitBoundary) -> bool {
    matches!(
        boundary,
        CommitBoundary::WalSynchronized | CommitBoundary::RootPublished
    )
}

fn large_value() -> Vec<u8> {
    vec![0x5a; LARGE_VALUE_BYTES]
}

fn validate_receipt_label(name: &str, value: &str) -> Result<(), Box<dyn Error>> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(failure(format!("invalid {name} receipt label")));
    }
    Ok(())
}

fn require_equal<T>(name: &str, actual: &T, expected: &T) -> Result<(), Box<dyn Error>>
where
    T: std::fmt::Debug + PartialEq,
{
    if actual != expected {
        return Err(failure(format!(
            "{name} mismatch: actual {actual:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::other(message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::{
        CHECKPOINT_BOUNDARIES, COMMIT_BOUNDARIES, SNAPSHOT_PIN_BOUNDARIES,
        expects_power_loss_complete_state, parse_snapshot_pin_boundary,
        power_loss_checkpoint_counts,
    };
    use hyphae_native_runtime::{CheckpointBoundary, CommitBoundary, SnapshotPinBoundary};

    #[test]
    fn power_loss_commit_authority_starts_at_synchronized_wal() {
        let actual =
            COMMIT_BOUNDARIES.map(|(_, boundary)| expects_power_loss_complete_state(boundary));
        assert_eq!(actual, [false, false, false, false, false, true, true]);
    }

    #[test]
    fn power_loss_checkpoint_authority_starts_at_synchronized_wal() {
        let actual = CHECKPOINT_BOUNDARIES.map(|(_, boundary)| {
            let (manifests, checkpoints, unanchored) = power_loss_checkpoint_counts(boundary);
            (manifests, checkpoints, unanchored)
        });
        assert_eq!(actual, [(0, 0, 0), (1, 0, 1), (1, 0, 1), (1, 1, 0)]);
        assert_eq!(
            power_loss_checkpoint_counts(CheckpointBoundary::WalAppended),
            (1, 0, 1)
        );
        assert!(!expects_power_loss_complete_state(
            CommitBoundary::WalAppended
        ));
    }

    #[test]
    fn snapshot_pin_boundary_names_are_complete_and_exact() {
        assert_eq!(
            SNAPSHOT_PIN_BOUNDARIES.map(|(name, _)| name),
            ["record-synchronized", "record-published"]
        );
        assert_eq!(
            parse_snapshot_pin_boundary("record-published"),
            Some(SnapshotPinBoundary::RecordPublished)
        );
        assert_eq!(parse_snapshot_pin_boundary("published"), None);
    }
}
