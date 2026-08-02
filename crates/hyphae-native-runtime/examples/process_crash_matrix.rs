// SPDX-License-Identifier: Apache-2.0

//! Process-level crash matrix for the first all-engine native commit.
//!
//! The parent starts one child per commit boundary. The child reaches the
//! deterministic boundary while retaining the database handle and writer lock,
//! signals readiness through stdout, and parks. The parent then hard-kills the
//! child, reopens the directory, and requires either the complete prior state
//! or the complete committed CSN across relational, structure, and lexical
//! reads.

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
    CommitBoundary, NativeDatabase, NativeRuntimeError, NativeTransaction, Ttl,
};
use hyphae_native_types::{Csn, DurabilityClass, ObjectId};

const CHILD_MODE: &str = "--child";
const READY_PREFIX: &str = "hyphae-native-crash-ready:";
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(10);
const LARGE_VALUE_BYTES: usize = 16 * 1024;
const TABLE_ID: u128 = 1;
const SEARCH_INDEX_ID: u128 = 2;

const BOUNDARIES: [(&str, CommitBoundary); 7] = [
    ("blob-staged", CommitBoundary::BlobStaged),
    ("blob-promoted", CommitBoundary::BlobPromoted),
    ("page-appended", CommitBoundary::PageAppended),
    ("page-synchronized", CommitBoundary::PageSynchronized),
    ("wal-appended", CommitBoundary::WalAppended),
    ("wal-synchronized", CommitBoundary::WalSynchronized),
    ("root-published", CommitBoundary::RootPublished),
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

struct BoundaryObservation {
    name: &'static str,
    expected_state: &'static str,
    recovered_csn: Option<u64>,
    recovered_blob_count: usize,
    termination: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let first = arguments
        .next()
        .ok_or_else(|| failure("missing source commit or child mode"))?;
    if first == CHILD_MODE {
        let directory = arguments
            .next()
            .ok_or_else(|| failure("child mode requires a data directory"))?;
        let boundary = arguments
            .next()
            .ok_or_else(|| failure("child mode requires a boundary"))?
            .into_string()
            .map_err(|_| failure("boundary is not valid UTF-8"))?;
        require_no_remaining(arguments)?;
        return run_child(Path::new(&directory), &boundary);
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

fn run_child(directory: &Path, boundary_name: &str) -> Result<(), Box<dyn Error>> {
    let boundary = parse_boundary(boundary_name)
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

    println!("{READY_PREFIX}{boundary_name}");
    io::stdout().flush()?;
    loop {
        thread::park();
    }
}

fn run_parent(source_commit: &str, environment: &str) -> Result<(), Box<dyn Error>> {
    let executable = std::env::current_exe()?;
    let temporary = TemporaryDirectory::create()?;
    fs::create_dir_all(temporary.path())?;
    let mut observations = Vec::with_capacity(BOUNDARIES.len());

    for (name, boundary) in BOUNDARIES {
        let directory = temporary.path().join(name);
        let mut child = Command::new(&executable)
            .arg(CHILD_MODE)
            .arg(&directory)
            .arg(name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let ready = wait_for_child_ready(&mut child)?;
        let expected_ready = format!("{READY_PREFIX}{name}");
        if ready.trim_end() != expected_ready {
            stop_child(&mut child);
            return Err(failure(format!(
                "boundary {name} emitted unexpected readiness: {ready:?}"
            )));
        }

        child.kill()?;
        let status = child.wait()?;
        let termination = validate_hard_kill(name, status)?;
        let database = NativeDatabase::open(&directory)?;
        let expected_complete = expects_complete_state(boundary);
        validate_recovered_state(&database, expected_complete)?;
        observations.push(BoundaryObservation {
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

    print_receipt(source_commit, environment, &observations);
    Ok(())
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

fn print_receipt(source_commit: &str, environment: &str, observations: &[BoundaryObservation]) {
    println!("{{");
    println!("  \"schema\": \"hyphae.native.process-crash-matrix.v1\",");
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
    println!("  \"boundaries\": [");
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
    println!("  ]");
    println!("}}");
}

fn parse_boundary(name: &str) -> Option<CommitBoundary> {
    BOUNDARIES
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
