// SPDX-License-Identifier: Apache-2.0

//! Keyspace point suite: Hyphae native structures vs external Valkey.
//!
//! Workload: `keys` string keys (`key-%010d` -> 64-byte values), then skewed
//! GET and SET phases. Measured surfaces:
//! - `hyphae embedded`: direct library calls (`get_latest_structure`,
//!   `commit_optimistic` with one `set` per commit);
//! - `valkey uds no`: no AOF or RDB persistence, paired only with Hyphae
//!   `Memory` because neither lane acknowledges physical persistence;
//! - `valkey uds always`: AOF `appendfsync always`, paired with Hyphae
//!   `Strict` because both acknowledge an fsync per measured write/commit;
//! - `valkey uds everysec`: asynchronous periodic fsync, reported separately
//!   without a Hyphae durability-equivalence claim.
//!
//! Fairness notes: Valkey is measured over UDS (its fastest local transport)
//! while embedded Hyphae has no transport at all; the receipt therefore also
//! reports every durability lane separately so transport and persistence
//! policy remain visible instead of being blended.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use hyphae_native_runtime::NativeDatabase;
use hyphae_native_types::DurabilityClass;
use redis::ConnectionLike;
use sha2::{Digest, Sha256};

use crate::util::{fresh_dir, sha256_file, Recorder, Xorshift};

const VALKEY_VERSION: &str = "9.1.2";
const VALKEY_SOURCE_COMMIT: &str = "7f1dffedff6de73058b2c2a389422b6ecd56c8fb";
const VALKEY_ARTIFACT_SHA256: &str =
    "19c23908e7d57e8d91ef85b41f5646307582f10f4f0fb999bbf89ed24ec9c983";
const VALKEY_ARTIFACT_URL: &str =
    "https://codeload.github.com/valkey-io/valkey/tar.gz/refs/tags/9.1.2";
const VALKEY_PROFILE_SCHEMA: &str = "hyphae-external-valkey-application-core-v1";
const VALKEY_PROFILE_NAME: &str = "Valkey Application Core";
const VALKEY_PROFILE_STATUS: &str = "foundation_only";
const VALKEY_RECEIPT_SCHEMA: &str = "hyphae-external-valkey-baseline-receipt-v2";
const HYPHAE_SEMANTIC_SOURCE_COMMIT: &str = "7e25fc7eaa7fe91bc55e16314074d66ad943b090";
const HYPHAE_SEMANTIC_SOURCE_TREE: &str = "aea2680c992697d68c2b52582f8399f9189108ac";
const HYPHAE_SEMANTIC_QUALIFICATION: &str =
    "historical 3.0.0 classification source; the executed-source semantic bundle separately binds the candidate tree; no compatibility or performance claim";
const VALKEY_NO_CONFIG_SHA256: &str =
    "6f3abe771321dcd8d832bd3927a0ccdd0ef08270cea41dbe7dcfa2bfae361166";
const VALKEY_ALWAYS_CONFIG_SHA256: &str =
    "e32bcbdaec8a9698a8d3d19957a7ca54527819802f74c83efcd514215d76fa91";
const VALKEY_EVERYSEC_CONFIG_SHA256: &str =
    "07bc14fd2ea328b03641576e6f274ec05dc759540325d2fbc138b31504b6e879";

const ALWAYS_WRITE_DOMAIN: u64 = 0x616c_7761_7973_0001;
const NO_WRITE_DOMAIN: u64 = 0x6e6f_0000_0000_0001;

#[derive(Clone, Copy)]
struct ValkeyLaneSpec {
    name: &'static str,
    appendonly: &'static str,
    appendfsync: &'static str,
    directory: &'static str,
    config_sha256: &'static str,
    pid_environment: &'static str,
}

const NO_LANE: ValkeyLaneSpec = ValkeyLaneSpec {
    name: "no",
    appendonly: "no",
    appendfsync: "no",
    directory: "/mnt/nvme/valkey-no",
    config_sha256: VALKEY_NO_CONFIG_SHA256,
    pid_environment: "HYPHAE_VALKEY_NO_PID",
};
const ALWAYS_LANE: ValkeyLaneSpec = ValkeyLaneSpec {
    name: "always",
    appendonly: "yes",
    appendfsync: "always",
    directory: "/mnt/nvme/valkey-always",
    config_sha256: VALKEY_ALWAYS_CONFIG_SHA256,
    pid_environment: "HYPHAE_VALKEY_ALWAYS_PID",
};
const EVERYSEC_LANE: ValkeyLaneSpec = ValkeyLaneSpec {
    name: "everysec",
    appendonly: "yes",
    appendfsync: "everysec",
    directory: "/mnt/nvme/valkey-everysec",
    config_sha256: VALKEY_EVERYSEC_CONFIG_SHA256,
    pid_environment: "HYPHAE_VALKEY_EVERYSEC_PID",
};

#[derive(Debug)]
struct WriteKeyWorkload {
    reads: Vec<u64>,
    always: Vec<u64>,
    no: Vec<u64>,
}

impl WriteKeyWorkload {
    fn new(config: &KeyspaceSuiteConfig) -> Self {
        Self {
            reads: write_key_sequence(config.seed, config.keys, config.gets),
            always: write_key_sequence(
                config.seed ^ ALWAYS_WRITE_DOMAIN,
                config.keys,
                config.strict_sets,
            ),
            no: write_key_sequence(
                config.seed ^ NO_WRITE_DOMAIN,
                config.keys,
                config.relaxed_sets,
            ),
        }
    }
}

#[derive(Debug)]
struct ValkeyBuildIdentity {
    source_archive_url: String,
    source_archive_sha256: String,
    server_binary_sha256: String,
    compiler: String,
    build_flags: String,
    setup_id: String,
    profile_sha256: String,
    claim_semantics_sha256: String,
    retained_server_artifact: PathBuf,
    executed_source_commit: String,
    executed_source_tree: String,
    semantic_bundle_sha256: String,
}

impl ValkeyBuildIdentity {
    fn from_environment() -> anyhow::Result<Self> {
        let identity = Self {
            source_archive_url: required_environment("HYPHAE_VALKEY_SOURCE_ARCHIVE_URL")?,
            source_archive_sha256: required_environment("HYPHAE_VALKEY_SOURCE_ARCHIVE_SHA256")?,
            server_binary_sha256: required_environment("HYPHAE_VALKEY_SERVER_SHA256")?,
            compiler: required_environment("HYPHAE_VALKEY_COMPILER")?,
            build_flags: required_environment("HYPHAE_VALKEY_BUILD_FLAGS")?,
            setup_id: required_environment("HYPHAE_VALKEY_SETUP_ID")?,
            profile_sha256: required_environment("HYPHAE_VALKEY_PROFILE_SHA256")?,
            claim_semantics_sha256: required_environment("HYPHAE_VALKEY_CLAIM_SEMANTICS_SHA256")?,
            retained_server_artifact: PathBuf::from(required_environment(
                "HYPHAE_VALKEY_RETAINED_SERVER_ARTIFACT",
            )?),
            executed_source_commit: required_environment("HYPHAE_SOURCE_POST_COMMIT")?,
            executed_source_tree: required_environment("HYPHAE_SOURCE_POST_TREE")?,
            semantic_bundle_sha256: required_environment("HYPHAE_VALKEY_SEMANTIC_BUNDLE_SHA256")?,
        };
        if identity.source_archive_url != VALKEY_ARTIFACT_URL {
            bail!("Valkey source archive URL differs from the pinned authority");
        }
        if identity.source_archive_sha256 != VALKEY_ARTIFACT_SHA256 {
            bail!("Valkey source archive digest differs from the pinned authority");
        }
        validate_sha256(
            "HYPHAE_VALKEY_SERVER_SHA256",
            &identity.server_binary_sha256,
        )?;
        validate_sha256("HYPHAE_VALKEY_SETUP_ID", &identity.setup_id)?;
        validate_sha256("HYPHAE_VALKEY_PROFILE_SHA256", &identity.profile_sha256)?;
        validate_sha256(
            "HYPHAE_VALKEY_CLAIM_SEMANTICS_SHA256",
            &identity.claim_semantics_sha256,
        )?;
        if !identity.retained_server_artifact.is_absolute() {
            bail!("retained Valkey server artifact path must be absolute");
        }
        let retained_sha256 = sha256_file(&identity.retained_server_artifact)
            .context("digesting retained Valkey server artifact")?;
        if retained_sha256 != identity.server_binary_sha256 {
            bail!("retained Valkey server artifact digest differs from the runner identity");
        }
        for (label, value) in [
            (
                "executed source commit",
                identity.executed_source_commit.as_str(),
            ),
            (
                "executed source tree",
                identity.executed_source_tree.as_str(),
            ),
        ] {
            if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("{label} is invalid");
            }
        }
        validate_sha256(
            "HYPHAE_VALKEY_SEMANTIC_BUNDLE_SHA256",
            &identity.semantic_bundle_sha256,
        )?;
        if identity.compiler.len() > 4_096 || identity.build_flags.len() > 4_096 {
            bail!("Valkey compiler or build-flags identity is oversized");
        }
        for required in [
            "BUILD_TLS=no",
            "MALLOC=",
            "OPTIMIZATION=",
            "CC=",
            "CFLAGS=",
            "LDFLAGS=",
        ] {
            if !identity
                .build_flags
                .split(';')
                .any(|value| value.starts_with(required))
            {
                bail!("Valkey build flags omit {required}");
            }
        }
        Ok(identity)
    }
}

#[derive(Debug)]
struct ValkeyRuntimeIdentity {
    server: BTreeMap<String, String>,
    process_id: u32,
    config_file: PathBuf,
    executable: PathBuf,
}

struct ValkeyMeasurements {
    setup_identity: serde_json::Value,
    connection_alive: bool,
    get_hits: u64,
    get: serde_json::Value,
    read_key_sequence_sha256: String,
    set: serde_json::Value,
    write_key_sequence_sha256: String,
}

pub struct KeyspaceSuiteConfig {
    pub keys: u64,
    pub gets: usize,
    pub strict_sets: usize,
    pub relaxed_sets: usize,
    pub scratch_root: String,
    pub seed: u64,
    /// Unix socket of Valkey without persistence; empty disables.
    pub valkey_no_socket: String,
    /// Unix socket of Valkey with appendfsync always; empty disables.
    pub valkey_always_socket: String,
    /// Unix socket of Valkey with appendfsync everysec; empty disables.
    pub valkey_everysec_socket: String,
}

fn key(index: u64) -> Vec<u8> {
    format!("key-{index:010}").into_bytes()
}

fn value(index: u64) -> Vec<u8> {
    format!("value-{index:010}-{:040x}", (index as u128) * 0x5851).into_bytes()
}

pub fn run(config: &KeyspaceSuiteConfig) -> anyhow::Result<serde_json::Value> {
    let external_enabled = !config.valkey_no_socket.is_empty()
        || !config.valkey_always_socket.is_empty()
        || !config.valkey_everysec_socket.is_empty();
    let build_identity = external_enabled
        .then(ValkeyBuildIdentity::from_environment)
        .transpose()
        .context("Valkey external build identity")?;
    let writes = WriteKeyWorkload::new(config);
    let dataset_sha256 = initial_dataset_sha256(config.keys);
    let hyphae = hyphae_run(config, &writes, &dataset_sha256).context("hyphae keyspace suite")?;
    let valkey_no = if config.valkey_no_socket.is_empty() {
        serde_json::Value::Null
    } else {
        valkey_run(
            &config.valkey_no_socket,
            NO_LANE,
            &writes.reads,
            &writes.no,
            build_identity
                .as_ref()
                .context("missing Valkey build identity")?,
            &dataset_sha256,
            config,
        )
        .context("Valkey no-persistence keyspace suite")?
    };
    let valkey_always = if config.valkey_always_socket.is_empty() {
        serde_json::Value::Null
    } else {
        valkey_run(
            &config.valkey_always_socket,
            ALWAYS_LANE,
            &writes.reads,
            &writes.always,
            build_identity
                .as_ref()
                .context("missing Valkey build identity")?,
            &dataset_sha256,
            config,
        )
        .context("Valkey appendfsync-always keyspace suite")?
    };
    let valkey_everysec = if config.valkey_everysec_socket.is_empty() {
        serde_json::Value::Null
    } else {
        valkey_run(
            &config.valkey_everysec_socket,
            EVERYSEC_LANE,
            &writes.reads,
            &writes.no,
            build_identity
                .as_ref()
                .context("missing Valkey build identity")?,
            &dataset_sha256,
            config,
        )
        .context("Valkey appendfsync-everysec keyspace suite")?
    };
    validate_distinct_valkey_runtime_identities(&[&valkey_no, &valkey_always, &valkey_everysec])?;
    Ok(serde_json::json!({
        "workload": {
            "keys": config.keys,
            "gets": config.gets,
            "strict_sets": config.strict_sets,
            "relaxed_sets": config.relaxed_sets,
            "seed": config.seed,
        },
        "hyphae": hyphae,
        "valkey_no": valkey_no,
        "valkey_always": valkey_always,
        "valkey_everysec": valkey_everysec,
    }))
}

fn validate_distinct_valkey_runtime_identities(
    receipts: &[&serde_json::Value],
) -> anyhow::Result<()> {
    let mut process_ids = BTreeSet::new();
    let mut run_ids = BTreeSet::new();
    let mut count = 0_usize;
    for receipt in receipts.iter().filter(|receipt| !receipt.is_null()) {
        let runtime = receipt
            .get("external_identity")
            .and_then(|identity| identity.get("runtime"))
            .and_then(serde_json::Value::as_object)
            .context("Valkey receipt runtime identity is missing")?;
        let process_id = runtime
            .get("process_id")
            .and_then(serde_json::Value::as_str)
            .context("Valkey receipt process_id is missing")?;
        let run_id = runtime
            .get("run_id")
            .and_then(serde_json::Value::as_str)
            .context("Valkey receipt run_id is missing")?;
        if !process_ids.insert(process_id.to_owned()) || !run_ids.insert(run_id.to_owned()) {
            bail!("Valkey lanes must use distinct process_id and run_id identities");
        }
        count += 1;
    }
    if process_ids.len() != count || run_ids.len() != count {
        bail!("Valkey runtime identity cardinality differs from enabled lanes");
    }
    Ok(())
}

fn hyphae_run(
    config: &KeyspaceSuiteConfig,
    writes: &WriteKeyWorkload,
    dataset_sha256: &str,
) -> anyhow::Result<serde_json::Value> {
    let always = hyphae_lane_run(
        config,
        "always",
        DurabilityClass::Strict,
        "fsync_per_commit",
        &writes.reads,
        &writes.always,
        dataset_sha256,
    )?;
    let no = hyphae_lane_run(
        config,
        "no",
        DurabilityClass::Memory,
        "none",
        &writes.reads,
        &writes.no,
        dataset_sha256,
    )?;
    let always_directory = always["setup_identity"]["data_directory"]
        .as_str()
        .context("Hyphae always setup directory identity")?;
    let no_directory = no["setup_identity"]["data_directory"]
        .as_str()
        .context("Hyphae no setup directory identity")?;
    if always_directory == no_directory {
        bail!("paired Hyphae lanes must use isolated data directories");
    }
    Ok(serde_json::json!({
        "engine": "hyphae-native-embedded",
        "transport": "none (in-process library call)",
        "always_comparison": always,
        "no_comparison": no,
    }))
}

fn hyphae_lane_run(
    config: &KeyspaceSuiteConfig,
    lane: &str,
    durability: DurabilityClass,
    persistence_acknowledgement: &str,
    reads: &[u64],
    writes: &[u64],
    dataset_sha256: &str,
) -> anyhow::Result<serde_json::Value> {
    let path = fresh_dir(&config.scratch_root, &format!("keyspace-hyphae-{lane}"));
    if path.exists() {
        bail!("Hyphae lane {lane} data directory already exists");
    }
    let mut database = NativeDatabase::create(&path)?;
    let initial_probe_absent = database.get_latest_structure(&key(0), 0)?.is_none();
    if !initial_probe_absent {
        bail!("new Hyphae lane {lane} database was not empty");
    }

    // Load all keys in batches of 1,000 under strict durability through the
    // point-resolved delta path (no full-state materialization per begin).
    let mut loaded = 0_u64;
    while loaded < config.keys {
        let upper = (loaded + 1_000).min(config.keys);
        let mut batch = database.begin_optimistic_delta(0, DurabilityClass::Strict)?;
        for index in loaded..upper {
            database.stage_delta_set(&mut batch, key(index), value(index), None)?;
        }
        database.commit_optimistic(batch)?;
        loaded = upper;
    }
    let loaded_probe_matches =
        database.get_latest_structure(&key(0), 0)?.as_deref() == Some(value(0).as_slice());
    if !loaded_probe_matches {
        bail!("Hyphae lane {lane} dataset verification failed");
    }

    let mut gets = Recorder::with_capacity(config.gets);
    let mut hit = 0_u64;
    for &index in reads {
        let lookup = key(index);
        let found = gets.record(|| database.get_latest_structure(&lookup, 0))?;
        if found.is_some() {
            hit += 1;
        }
    }
    let get_summary = gets.summary("get_latest");

    let mut sets = Recorder::with_capacity(writes.len());
    for_each_write(writes, |sequence, index| {
        sets.record(|| -> anyhow::Result<()> {
            let mut batch = database.begin_optimistic_delta(0, durability)?;
            database.stage_delta_set(&mut batch, key(index), value(sequence as u64), None)?;
            database.commit_optimistic(batch)?;
            Ok(())
        })
    })?;
    let set_summary = sets.summary(match durability {
        DurabilityClass::Strict => "set_strict_fsync_per_commit",
        DurabilityClass::Memory => "set_memory_no_fsync_ack",
        _ => "set_other",
    });

    drop(database);
    std::fs::remove_dir_all(&path).ok();
    Ok(serde_json::json!({
        "lane": lane,
        "durability": match durability {
            DurabilityClass::Strict => "strict",
            DurabilityClass::Memory => "memory",
            _ => "other",
        },
        "persistence_acknowledgement": persistence_acknowledgement,
        "setup_identity": {
            "state": "fresh-created-database",
            "data_directory": path,
            "initial_probe_absent": initial_probe_absent,
            "loaded_probe_matches": loaded_probe_matches,
            "loaded_keys": config.keys,
            "dataset_sha256": dataset_sha256,
        },
        "get_hits": hit,
        "get": get_summary,
        "read_key_sequence_sha256": read_key_sequence_sha256(reads),
        "write_key_sequence_sha256": write_key_sequence_sha256(writes),
        "set": set_summary,
    }))
}

fn valkey_run(
    socket: &str,
    lane: ValkeyLaneSpec,
    reads: &[u64],
    writes: &[u64],
    build: &ValkeyBuildIdentity,
    dataset_sha256: &str,
    config: &KeyspaceSuiteConfig,
) -> anyhow::Result<serde_json::Value> {
    let client = redis::Client::open(format!("unix://{socket}"))?;
    let mut connection = client.get_connection()?;
    let info: String = redis::cmd("INFO").arg("server").query(&mut connection)?;
    let runtime = parse_runtime_identity(&info)?;
    let started_pid = canonical_pid(
        lane.pid_environment,
        &required_environment(lane.pid_environment)?,
    )?;
    if runtime.process_id != started_pid {
        bail!(
            "Valkey lane {} runtime PID differs from its started PID",
            lane.name
        );
    }
    let raw_config: HashMap<String, String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("*")
        .query(&mut connection)?;
    let effective_config: BTreeMap<String, String> = raw_config.into_iter().collect();
    validate_effective_config(lane, socket, &effective_config)?;

    let config_sha256 = sha256_file(&runtime.config_file)
        .with_context(|| format!("digesting {}", runtime.config_file.display()))?;
    if config_sha256 != lane.config_sha256 {
        bail!(
            "Valkey lane {} config digest differs: expected {}, found {}",
            lane.name,
            lane.config_sha256,
            config_sha256
        );
    }
    let proc_executable = PathBuf::from(format!("/proc/{}/exe", runtime.process_id));
    let running_executable = std::fs::canonicalize(&proc_executable).with_context(|| {
        format!(
            "resolving running Valkey executable for pid {}",
            runtime.process_id
        )
    })?;
    let reported_executable = std::fs::canonicalize(&runtime.executable).with_context(|| {
        format!(
            "resolving reported Valkey executable {}",
            runtime.executable.display()
        )
    })?;
    if running_executable != reported_executable {
        bail!(
            "Valkey INFO executable {} differs from running process executable {}",
            reported_executable.display(),
            running_executable.display()
        );
    }
    let server_binary_sha256 = sha256_file(&proc_executable).with_context(|| {
        format!(
            "digesting running Valkey executable for pid {}",
            runtime.process_id
        )
    })?;
    if server_binary_sha256 != build.server_binary_sha256 {
        bail!(
            "running Valkey binary digest differs: expected {}, found {}",
            build.server_binary_sha256,
            server_binary_sha256
        );
    }
    for (key, expected) in [
        ("valkey_version", VALKEY_VERSION),
        ("server_name", "valkey"),
        ("server_mode", "standalone"),
        ("tcp_port", "0"),
        ("process_supervised", "no"),
    ] {
        let actual = runtime.server.get(key).map(String::as_str);
        if actual != Some(expected) {
            bail!(
                "Valkey lane {} requires INFO {key}={expected}, found {actual:?}",
                lane.name
            );
        }
    }
    let setup_marker = Path::new(lane.directory).join(".hyphae-fresh-setup");
    let expected_marker = format!("{}:{}\n", build.setup_id, lane.name);
    let actual_marker = std::fs::read_to_string(&setup_marker)
        .with_context(|| format!("reading Valkey lane {} fresh-setup marker", lane.name))?;
    if actual_marker != expected_marker {
        bail!("Valkey lane {} fresh-setup marker differs", lane.name);
    }
    let setup_marker_sha256 = sha256_file(&setup_marker)?;
    let initial_dbsize: u64 = redis::cmd("DBSIZE").query(&mut connection)?;
    if initial_dbsize != 0 {
        bail!(
            "Valkey lane {} must start empty, found {initial_dbsize} keys",
            lane.name
        );
    }

    // Load with pipelines of 1,000.
    let mut loaded = 0_u64;
    while loaded < config.keys {
        let upper = (loaded + 1_000).min(config.keys);
        let mut pipeline = redis::pipe();
        for index in loaded..upper {
            pipeline
                .cmd("SET")
                .arg(key(index))
                .arg(value(index))
                .ignore();
        }
        pipeline.exec(&mut connection)?;
        loaded = upper;
    }
    let loaded_dbsize: u64 = redis::cmd("DBSIZE").query(&mut connection)?;
    if loaded_dbsize != config.keys {
        bail!(
            "Valkey lane {} loaded cardinality differs: expected {}, found {}",
            lane.name,
            config.keys,
            loaded_dbsize
        );
    }

    let mut gets = Recorder::with_capacity(config.gets);
    let mut hit = 0_u64;
    for &index in reads {
        let lookup = key(index);
        let found: Option<Vec<u8>> =
            gets.record(|| redis::cmd("GET").arg(&lookup).query(&mut connection))?;
        if found.is_some() {
            hit += 1;
        }
    }
    let get_summary = gets.summary("get_uds");

    let mut sets = Recorder::with_capacity(writes.len());
    for_each_write(writes, |sequence, index| {
        let write_key = key(index);
        let write_value = value(sequence as u64);
        sets.record(|| -> anyhow::Result<()> {
            redis::cmd("SET")
                .arg(&write_key)
                .arg(&write_value)
                .exec(&mut connection)?;
            Ok(())
        })
    })?;
    let set_summary = sets.summary("set_uds");

    let connected = connection.check_connection();
    Ok(valkey_receipt(
        lane,
        build,
        external_identity_receipt(
            build,
            &runtime,
            &running_executable,
            &server_binary_sha256,
            &config_sha256,
            &effective_config,
        ),
        ValkeyMeasurements {
            setup_identity: serde_json::json!({
                "state": "fresh-recreated-directory-and-server",
                "setup_id": build.setup_id,
                "data_directory": lane.directory,
                "marker_sha256": setup_marker_sha256,
                "initial_dbsize": initial_dbsize,
                "loaded_dbsize": loaded_dbsize,
                "dataset_sha256": dataset_sha256,
                "process_id": runtime.server["process_id"],
                "started_pid": started_pid.to_string(),
                "run_id": runtime.server["run_id"],
            }),
            connection_alive: connected,
            get_hits: hit,
            get: get_summary,
            read_key_sequence_sha256: read_key_sequence_sha256(reads),
            set: set_summary,
            write_key_sequence_sha256: write_key_sequence_sha256(writes),
        },
    ))
}

fn required_environment(name: &str) -> anyhow::Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    if value.trim().is_empty() || value.contains('\0') {
        bail!("{name} must be a nonempty text value");
    }
    Ok(value)
}

fn validate_sha256(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be one lowercase SHA-256 digest");
    }
    Ok(())
}

fn canonical_pid(label: &str, value: &str) -> anyhow::Result<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("{label} must be one canonical decimal PID");
    }
    let pid = value
        .parse::<u32>()
        .with_context(|| format!("{label} is outside the PID range"))?;
    if pid == 0 || pid.to_string() != value {
        bail!("{label} must be one positive canonical decimal PID");
    }
    Ok(pid)
}

fn write_key_sequence(seed: u64, keys: u64, writes: usize) -> Vec<u64> {
    let mut rng = Xorshift::new(seed);
    (0..writes).map(|_| rng.skewed(keys)).collect()
}

fn for_each_write(
    writes: &[u64],
    mut operation: impl FnMut(usize, u64) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for (sequence, &index) in writes.iter().enumerate() {
        operation(sequence, index)?;
    }
    Ok(())
}

fn write_key_sequence_sha256(writes: &[u64]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"hyphae-baseline-write-keys-v1\0");
    for index in writes {
        digest.update(index.to_be_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn read_key_sequence_sha256(reads: &[u64]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"hyphae-baseline-read-keys-v1\0");
    for index in reads {
        digest.update(index.to_be_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn initial_dataset_sha256(keys: u64) -> String {
    let mut digest = Sha256::new();
    digest.update(b"hyphae-baseline-keyspace-dataset-v1\0");
    for index in 0..keys {
        let key = key(index);
        let value = value(index);
        digest.update((key.len() as u64).to_be_bytes());
        digest.update(key);
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    format!("{:x}", digest.finalize())
}

fn effective_config_sha256(config: &BTreeMap<String, String>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"hyphae-valkey-effective-config-v1\0");
    for (key, value) in config {
        digest.update((key.len() as u64).to_be_bytes());
        digest.update(key.as_bytes());
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn validate_effective_config(
    lane: ValkeyLaneSpec,
    socket: &str,
    config: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for (key, expected) in [
        ("appendonly", lane.appendonly),
        ("appendfsync", lane.appendfsync),
        ("port", "0"),
        ("unixsocket", socket),
        ("maxmemory-policy", "noeviction"),
        ("maxmemory", "0"),
        ("databases", "1"),
        ("save", ""),
        ("protected-mode", "yes"),
        ("daemonize", "yes"),
        ("supervised", "no"),
        ("dir", lane.directory),
    ] {
        let actual = config.get(key).map(String::as_str);
        if actual != Some(expected) {
            bail!(
                "Valkey lane {} requires {key}={expected:?}, found {actual:?}",
                lane.name
            );
        }
    }
    Ok(())
}

fn parse_runtime_identity(info: &str) -> anyhow::Result<ValkeyRuntimeIdentity> {
    let mut server = BTreeMap::new();
    for raw_line in info.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .with_context(|| format!("malformed INFO server line {line:?}"))?;
        if key.is_empty() || server.insert(key.to_owned(), value.to_owned()).is_some() {
            bail!("duplicate or empty INFO server field {key:?}");
        }
    }
    let required = |key: &str| -> anyhow::Result<&str> {
        server
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("INFO server did not report {key}"))
    };
    for key in [
        "server_name",
        "redis_version",
        "valkey_version",
        "valkey_release_stage",
        "redis_git_sha1",
        "redis_build_id",
        "server_mode",
        "os",
        "arch_bits",
        "gcc_version",
        "process_id",
        "process_supervised",
        "run_id",
        "tcp_port",
        "executable",
        "config_file",
    ] {
        required(key)?;
    }
    if required("redis_git_dirty")? != "0" {
        bail!("running Valkey reports a dirty source build");
    }
    if !matches!(required("arch_bits")?, "32" | "64") {
        bail!("INFO server arch_bits is invalid");
    }
    let run_id = required("run_id")?;
    if run_id.len() != 40 || !run_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("INFO server run_id is invalid");
    }
    let process_id = canonical_pid("INFO server process_id", required("process_id")?)?;
    let config_file = PathBuf::from(required("config_file")?);
    let executable = PathBuf::from(required("executable")?);
    if !config_file.is_absolute() || !executable.is_absolute() {
        bail!("Valkey config_file and executable identities must be absolute paths");
    }
    Ok(ValkeyRuntimeIdentity {
        server,
        process_id,
        config_file,
        executable,
    })
}

fn external_identity_receipt(
    build: &ValkeyBuildIdentity,
    runtime: &ValkeyRuntimeIdentity,
    running_executable: &Path,
    server_binary_sha256: &str,
    config_sha256: &str,
    effective_config: &BTreeMap<String, String>,
) -> serde_json::Value {
    serde_json::json!({
        "source_archive": {
            "url": build.source_archive_url,
            "sha256": build.source_archive_sha256,
        },
        "build": {
            "compiler": build.compiler,
            "flags": build.build_flags,
            "server_binary": running_executable,
            "runner_expected_server_binary_sha256": build.server_binary_sha256,
            "executed_server_binary_sha256": server_binary_sha256,
            "retained_server_artifact": build.retained_server_artifact,
            "retained_server_artifact_sha256": build.server_binary_sha256,
        },
        "configuration": {
            "source_file": runtime.config_file,
            "source_sha256": config_sha256,
            "effective_sha256": effective_config_sha256(effective_config),
            "effective": effective_config,
        },
        "runtime": runtime.server,
    })
}

fn valkey_receipt(
    lane: ValkeyLaneSpec,
    build: &ValkeyBuildIdentity,
    external_identity: serde_json::Value,
    measurements: ValkeyMeasurements,
) -> serde_json::Value {
    serde_json::json!({
        "schema": VALKEY_RECEIPT_SCHEMA,
        "engine": "valkey-external",
        "lane": lane.name,
        "version": VALKEY_VERSION,
        "authority": {
            "application_core": {
                "schema": VALKEY_PROFILE_SCHEMA,
                "profile": VALKEY_PROFILE_NAME,
                "profile_version": 1,
                "status": VALKEY_PROFILE_STATUS,
                "foundation_only": true,
                "claim_semantics_sha256": build.claim_semantics_sha256,
                "profile_sha256": build.profile_sha256,
            },
            "oracle_source_commit": VALKEY_SOURCE_COMMIT,
            "hyphae_semantic_authority": {
                "source_commit": HYPHAE_SEMANTIC_SOURCE_COMMIT,
                "source_tree": HYPHAE_SEMANTIC_SOURCE_TREE,
                "workspace_version": "3.0.0",
                "qualification": HYPHAE_SEMANTIC_QUALIFICATION,
            },
            "executed_source_binding": {
                "policy": "exact-profile-and-sealed-semantic-bundle-v1",
                "source_commit": build.executed_source_commit,
                "source_tree": build.executed_source_tree,
                "semantic_bundle_sha256": build.semantic_bundle_sha256,
            },
        },
        "external_identity": external_identity,
        "setup_identity": measurements.setup_identity,
        "transport": "unix domain socket",
        "persistence": {
            "appendonly": lane.appendonly,
            "appendfsync": lane.appendfsync,
        },
        "connection_alive": measurements.connection_alive,
        "get_hits": measurements.get_hits,
        "get": measurements.get,
        "read_key_sequence_sha256": measurements.read_key_sequence_sha256,
        "write_key_sequence_sha256": measurements.write_key_sequence_sha256,
        "set": measurements.set,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> KeyspaceSuiteConfig {
        KeyspaceSuiteConfig {
            keys: 257,
            gets: 31,
            strict_sets: 67,
            relaxed_sets: 71,
            scratch_root: String::new(),
            seed: 0x4859_5048_4145_3031,
            valkey_no_socket: String::new(),
            valkey_always_socket: String::new(),
            valkey_everysec_socket: String::new(),
        }
    }

    fn collect_keys(writes: &[u64]) -> anyhow::Result<Vec<Vec<u8>>> {
        let mut keys = Vec::new();
        for_each_write(writes, |_, index| {
            keys.push(key(index));
            Ok(())
        })?;
        Ok(keys)
    }

    fn runtime_info(process_id: u32, run_id: &str, executable: &str, config_file: &str) -> String {
        format!(
            "# Server\r\nredis_version:{VALKEY_VERSION}\r\nserver_name:valkey\r\n\
             valkey_version:{VALKEY_VERSION}\r\nvalkey_release_stage:ga\r\n\
             redis_git_sha1:00000000\r\nredis_git_dirty:0\r\nredis_build_id:build-1\r\n\
             server_mode:standalone\r\nos:Linux\r\narch_bits:64\r\n\
             gcc_version:15.2.0\r\nprocess_id:{process_id}\r\nprocess_supervised:no\r\n\
             run_id:{run_id}\r\ntcp_port:0\r\n\
             executable:{executable}\r\nconfig_file:{config_file}\r\n"
        )
    }

    fn valid_runtime_info() -> String {
        runtime_info(
            42,
            "0123456789abcdef0123456789abcdef01234567",
            "/tmp/valkey-server",
            "/tmp/valkey.conf",
        )
    }

    fn effective_config(lane: ValkeyLaneSpec, socket: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("appendonly".to_owned(), lane.appendonly.to_owned()),
            ("appendfsync".to_owned(), lane.appendfsync.to_owned()),
            ("port".to_owned(), "0".to_owned()),
            ("unixsocket".to_owned(), socket.to_owned()),
            ("maxmemory-policy".to_owned(), "noeviction".to_owned()),
            ("maxmemory".to_owned(), "0".to_owned()),
            ("databases".to_owned(), "1".to_owned()),
            ("save".to_owned(), String::new()),
            ("protected-mode".to_owned(), "yes".to_owned()),
            ("daemonize".to_owned(), "yes".to_owned()),
            ("supervised".to_owned(), "no".to_owned()),
            ("dir".to_owned(), lane.directory.to_owned()),
        ])
    }

    fn summary(label: &str, operations: usize) -> serde_json::Value {
        serde_json::json!({
            "label": label,
            "operations": operations,
            "wall_nanos": 1_000_000_000_u64,
            "ops_per_second": operations as f64,
            "latency_nanos": {
                "total": operations,
                "mean": 1,
                "p50": 1,
                "p95": 1,
                "p99": 1,
                "p999": 1,
                "max": 1,
            },
        })
    }

    fn reviewed_profile() -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::from_str(include_str!(
            "../valkey/application-core-v1.json"
        ))?)
    }

    fn reviewed_profile_sha256() -> String {
        format!(
            "{:x}",
            Sha256::digest(include_bytes!("../valkey/application-core-v1.json"))
        )
    }

    fn reviewed_semantic_bundle_sha256() -> anyhow::Result<String> {
        let profile = reviewed_profile()?;
        profile["executed_source_binding"]["semantic_bundle_sha256"]
            .as_str()
            .map(str::to_owned)
            .context("profile semantic bundle digest")
    }

    #[allow(clippy::too_many_arguments)]
    fn interoperability_external_receipt(
        lane: ValkeyLaneSpec,
        socket: &str,
        process_id: u32,
        run_id: &str,
        reads: &[u64],
        writes: &[u64],
        dataset_sha256: &str,
        executable: &Path,
        executable_sha256: &str,
        build: &ValkeyBuildIdentity,
    ) -> anyhow::Result<serde_json::Value> {
        let config_file = format!("/tmp/valkey-{}.conf", lane.name);
        let runtime = parse_runtime_identity(&runtime_info(
            process_id,
            run_id,
            &executable.to_string_lossy(),
            &config_file,
        ))?;
        let effective = effective_config(lane, socket);
        let marker = format!("{}:{}\n", build.setup_id, lane.name);
        let marker_sha256 = format!("{:x}", Sha256::digest(marker.as_bytes()));
        let identity = external_identity_receipt(
            build,
            &runtime,
            executable,
            executable_sha256,
            lane.config_sha256,
            &effective,
        );
        Ok(valkey_receipt(
            lane,
            build,
            identity,
            ValkeyMeasurements {
                setup_identity: serde_json::json!({
                    "state": "fresh-recreated-directory-and-server",
                    "setup_id": build.setup_id,
                    "data_directory": lane.directory,
                    "marker_sha256": marker_sha256,
                    "initial_dbsize": 0,
                    "loaded_dbsize": 11,
                    "dataset_sha256": dataset_sha256,
                    "process_id": process_id.to_string(),
                    "started_pid": process_id.to_string(),
                    "run_id": run_id,
                }),
                connection_alive: true,
                get_hits: reads.len() as u64,
                get: summary("get_uds", reads.len()),
                read_key_sequence_sha256: read_key_sequence_sha256(reads),
                set: summary("set_uds", writes.len()),
                write_key_sequence_sha256: write_key_sequence_sha256(writes),
            },
        ))
    }

    #[test]
    fn paired_durability_lanes_consume_identical_write_keys() -> anyhow::Result<()> {
        let writes = WriteKeyWorkload::new(&config());
        let hyphae_strict = collect_keys(&writes.always)?;
        let valkey_always = collect_keys(&writes.always)?;
        let hyphae_memory = collect_keys(&writes.no)?;
        let valkey_no = collect_keys(&writes.no)?;
        let hyphae_reads = collect_keys(&writes.reads)?;
        let valkey_reads = collect_keys(&writes.reads)?;

        assert_eq!(hyphae_strict, valkey_always);
        assert_eq!(hyphae_memory, valkey_no);
        assert_eq!(hyphae_reads, valkey_reads);
        assert_ne!(hyphae_strict, hyphae_memory);
        Ok(())
    }

    #[test]
    fn valkey_lanes_require_distinct_process_and_run_ids() {
        let receipt = |process_id: &str, run_id: &str| {
            serde_json::json!({
                "external_identity": {
                    "runtime": {"process_id": process_id, "run_id": run_id}
                }
            })
        };
        let no = receipt("10", &"a".repeat(40));
        let always = receipt("11", &"b".repeat(40));
        let duplicate_process = receipt("10", &"c".repeat(40));
        let duplicate_run = receipt("12", &"a".repeat(40));

        validate_distinct_valkey_runtime_identities(&[&no, &always])
            .expect("distinct runtime identities");
        assert!(validate_distinct_valkey_runtime_identities(&[&no, &duplicate_process]).is_err());
        assert!(validate_distinct_valkey_runtime_identities(&[&no, &duplicate_run]).is_err());
    }

    #[test]
    fn paired_hyphae_lanes_use_fresh_isolated_state() -> anyhow::Result<()> {
        let scratch = std::env::temp_dir().join(format!(
            "hyphae-baseline-isolated-lanes-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let mut config = config();
        config.keys = 5;
        config.gets = 3;
        config.strict_sets = 2;
        config.relaxed_sets = 2;
        config.scratch_root = scratch.to_string_lossy().into_owned();
        let writes = WriteKeyWorkload::new(&config);
        let dataset = initial_dataset_sha256(config.keys);
        let receipt = hyphae_run(&config, &writes, &dataset)?;
        let always = &receipt["always_comparison"]["setup_identity"];
        let no = &receipt["no_comparison"]["setup_identity"];

        assert_ne!(always["data_directory"], no["data_directory"]);
        assert_eq!(always["state"], "fresh-created-database");
        assert_eq!(no["state"], "fresh-created-database");
        assert_eq!(always["dataset_sha256"], dataset);
        assert_eq!(no["dataset_sha256"], dataset);
        assert_eq!(always["initial_probe_absent"], true);
        assert_eq!(no["initial_probe_absent"], true);
        std::fs::remove_dir_all(scratch)?;
        Ok(())
    }

    #[test]
    fn rejects_runtime_config_drift() {
        let mut effective = BTreeMap::from([
            ("appendonly".to_owned(), "no".to_owned()),
            ("appendfsync".to_owned(), "no".to_owned()),
            ("port".to_owned(), "0".to_owned()),
            (
                "unixsocket".to_owned(),
                "/run/hyphae-valkey-no.sock".to_owned(),
            ),
            ("maxmemory-policy".to_owned(), "noeviction".to_owned()),
            ("maxmemory".to_owned(), "0".to_owned()),
            ("databases".to_owned(), "1".to_owned()),
            ("save".to_owned(), String::new()),
            ("protected-mode".to_owned(), "yes".to_owned()),
            ("daemonize".to_owned(), "yes".to_owned()),
            ("supervised".to_owned(), "no".to_owned()),
            ("dir".to_owned(), NO_LANE.directory.to_owned()),
        ]);
        validate_effective_config(NO_LANE, "/run/hyphae-valkey-no.sock", &effective)
            .expect("exact no-persistence config");
        effective.insert("appendonly".to_owned(), "yes".to_owned());
        let error = validate_effective_config(NO_LANE, "/run/hyphae-valkey-no.sock", &effective)
            .expect_err("runtime config drift must fail closed");
        assert!(error.to_string().contains("appendonly"));
    }

    #[test]
    fn rejects_malformed_runtime_identity() {
        let malformed = valid_runtime_info()
            .replace("0123456789abcdef0123456789abcdef01234567", "not-a-run-id");
        let error = parse_runtime_identity(&malformed).expect_err("malformed run id");
        assert!(error.to_string().contains("run_id"));
        let aliased = valid_runtime_info().replace("process_id:42", "process_id:042");
        let error = parse_runtime_identity(&aliased).expect_err("aliased PID");
        assert!(error.to_string().contains("canonical decimal PID"));
    }

    #[test]
    fn external_receipt_binds_complete_v2_identity() -> anyhow::Result<()> {
        let runtime = parse_runtime_identity(&valid_runtime_info())?;
        let build = ValkeyBuildIdentity {
            source_archive_url: VALKEY_ARTIFACT_URL.to_owned(),
            source_archive_sha256: VALKEY_ARTIFACT_SHA256.to_owned(),
            server_binary_sha256: "a".repeat(64),
            compiler: "cc 15.2.0".to_owned(),
            build_flags: "BUILD_TLS=no;MALLOC=jemalloc;OPTIMIZATION=-O3;CC=cc;CFLAGS=;LDFLAGS="
                .to_owned(),
            setup_id: "e".repeat(64),
            profile_sha256: reviewed_profile_sha256(),
            claim_semantics_sha256: reviewed_profile()?["claim_semantics_seal"]["sha256"]
                .as_str()
                .context("profile claim seal")?
                .to_owned(),
            retained_server_artifact: PathBuf::from("/tmp/retained-valkey-server"),
            executed_source_commit: "a".repeat(40),
            executed_source_tree: "b".repeat(40),
            semantic_bundle_sha256: reviewed_semantic_bundle_sha256()?,
        };
        let effective = BTreeMap::from([("appendonly".to_owned(), "no".to_owned())]);
        let external_identity = external_identity_receipt(
            &build,
            &runtime,
            Path::new("/tmp/valkey-server"),
            &build.server_binary_sha256,
            VALKEY_NO_CONFIG_SHA256,
            &effective,
        );
        let receipt = valkey_receipt(
            NO_LANE,
            &build,
            external_identity,
            ValkeyMeasurements {
                setup_identity: serde_json::json!({
                    "state": "fresh-recreated-directory-and-server",
                    "setup_id": "e".repeat(64),
                    "data_directory": NO_LANE.directory,
                    "marker_sha256": "f".repeat(64),
                    "initial_dbsize": 0,
                    "loaded_dbsize": 257,
                    "dataset_sha256": initial_dataset_sha256(257),
                    "process_id": "42",
                    "started_pid": "42",
                    "run_id": "0123456789abcdef0123456789abcdef01234567",
                }),
                connection_alive: true,
                get_hits: 1,
                get: serde_json::json!({"operations": 1}),
                read_key_sequence_sha256: "1".repeat(64),
                set: serde_json::json!({"operations": 1}),
                write_key_sequence_sha256: "b".repeat(64),
            },
        );

        assert_eq!(receipt["schema"], VALKEY_RECEIPT_SCHEMA);
        assert_eq!(receipt["lane"], "no");
        assert_eq!(
            receipt["authority"]["application_core"]["claim_semantics_sha256"],
            build.claim_semantics_sha256
        );
        assert_eq!(
            receipt["authority"]["application_core"]["profile_sha256"],
            build.profile_sha256
        );
        assert_eq!(receipt["setup_identity"]["initial_dbsize"], 0);
        assert_eq!(receipt["setup_identity"]["loaded_dbsize"], 257);
        assert_eq!(
            receipt["external_identity"]["source_archive"]["sha256"],
            VALKEY_ARTIFACT_SHA256
        );
        assert_eq!(
            receipt["external_identity"]["build"]["executed_server_binary_sha256"],
            "a".repeat(64)
        );
        assert_eq!(
            receipt["external_identity"]["build"]["runner_expected_server_binary_sha256"],
            receipt["external_identity"]["build"]["executed_server_binary_sha256"]
        );
        assert_eq!(
            receipt["external_identity"]["build"]["retained_server_artifact_sha256"],
            receipt["external_identity"]["build"]["executed_server_binary_sha256"]
        );
        assert_eq!(
            receipt["external_identity"]["build"]["compiler"],
            "cc 15.2.0"
        );
        assert_eq!(
            receipt["external_identity"]["configuration"]["source_sha256"],
            VALKEY_NO_CONFIG_SHA256
        );
        assert_eq!(
            receipt["external_identity"]["runtime"]["valkey_version"],
            VALKEY_VERSION
        );
        assert!(
            receipt["external_identity"]["configuration"]["effective_sha256"]
                .as_str()
                .is_some_and(|value| value.len() == 64)
        );
        Ok(())
    }

    #[test]
    fn receipt_interop_producer_child() -> anyhow::Result<()> {
        let Some(output) = std::env::var_os("HYPHAE_INTEROP_RECEIPT_OUTPUT") else {
            return Ok(());
        };
        let body_path = std::env::var_os("HYPHAE_INTEROP_RECEIPT_BODY")
            .context("HYPHAE_INTEROP_RECEIPT_BODY")?;
        let body: serde_json::Value = serde_json::from_slice(&std::fs::read(body_path)?)?;
        crate::util::write_receipt(&output.to_string_lossy(), body)
    }

    #[test]
    fn rust_producer_interoperates_with_independent_saved_receipt_validator() -> anyhow::Result<()>
    {
        let mut config = config();
        config.keys = 11;
        config.gets = 7;
        config.strict_sets = 3;
        config.relaxed_sets = 5;
        config.seed = 1;
        let workload = WriteKeyWorkload::new(&config);
        let dataset = initial_dataset_sha256(config.keys);
        let executable = std::env::current_exe()?;
        let executable_sha256 = sha256_file(&executable)?;
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest
            .parent()
            .and_then(Path::parent)
            .context("repository root")?;
        let status = std::process::Command::new("git")
            .args([
                "-C",
                &repo_root.to_string_lossy(),
                "status",
                "--porcelain=v1",
            ])
            .output()?;
        let committed_source = status.status.success() && status.stdout.is_empty();
        let git_identity = |revision: &str| -> anyhow::Result<String> {
            let output = std::process::Command::new("git")
                .args(["-C", &repo_root.to_string_lossy(), "rev-parse", revision])
                .output()?;
            if !output.status.success() {
                bail!("git rev-parse {revision} failed");
            }
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        let source_commit = if committed_source {
            git_identity("HEAD^{commit}")?
        } else {
            "a".repeat(40)
        };
        let source_tree = if committed_source {
            git_identity("HEAD^{tree}")?
        } else {
            "b".repeat(40)
        };
        let build = ValkeyBuildIdentity {
            source_archive_url: VALKEY_ARTIFACT_URL.to_owned(),
            source_archive_sha256: VALKEY_ARTIFACT_SHA256.to_owned(),
            server_binary_sha256: executable_sha256.clone(),
            compiler: "cc 15.2.0".to_owned(),
            build_flags: "BUILD_TLS=no;MALLOC=jemalloc;OPTIMIZATION=-O3;CC=cc;CFLAGS=;LDFLAGS="
                .to_owned(),
            setup_id: "e".repeat(64),
            profile_sha256: reviewed_profile_sha256(),
            claim_semantics_sha256: reviewed_profile()?["claim_semantics_seal"]["sha256"]
                .as_str()
                .context("profile claim seal")?
                .to_owned(),
            retained_server_artifact: executable.clone(),
            executed_source_commit: source_commit.clone(),
            executed_source_tree: source_tree.clone(),
            semantic_bundle_sha256: reviewed_semantic_bundle_sha256()?,
        };
        let hyphae_lane = |lane: &str,
                           durability: &str,
                           acknowledgement: &str,
                           directory: &str,
                           writes: &[u64],
                           set_label: &str| {
            serde_json::json!({
                "lane": lane,
                "durability": durability,
                "persistence_acknowledgement": acknowledgement,
                "setup_identity": {
                    "state": "fresh-created-database",
                    "data_directory": directory,
                    "initial_probe_absent": true,
                    "loaded_probe_matches": true,
                    "loaded_keys": config.keys,
                    "dataset_sha256": dataset,
                },
                "get_hits": config.gets,
                "get": summary("get_latest", config.gets),
                "read_key_sequence_sha256": read_key_sequence_sha256(&workload.reads),
                "write_key_sequence_sha256": write_key_sequence_sha256(writes),
                "set": summary(set_label, writes.len()),
            })
        };
        let body = serde_json::json!({"keyspace": {
            "workload": {
                "keys": config.keys,
                "gets": config.gets,
                "strict_sets": config.strict_sets,
                "relaxed_sets": config.relaxed_sets,
                "seed": config.seed,
            },
            "hyphae": {
                "engine": "hyphae-native-embedded",
                "transport": "none (in-process library call)",
                "always_comparison": hyphae_lane(
                    "always", "strict", "fsync_per_commit", "/tmp/hyphae-always",
                    &workload.always, "set_strict_fsync_per_commit"
                ),
                "no_comparison": hyphae_lane(
                    "no", "memory", "none", "/tmp/hyphae-no", &workload.no,
                    "set_memory_no_fsync_ack"
                ),
            },
            "valkey_no": interoperability_external_receipt(
                NO_LANE, "/run/hyphae-valkey-no.sock", 40, &"7".repeat(40),
                &workload.reads, &workload.no, &dataset, &executable,
                &executable_sha256, &build
            )?,
            "valkey_always": interoperability_external_receipt(
                ALWAYS_LANE, "/run/hyphae-valkey-always.sock", 41, &"8".repeat(40),
                &workload.reads, &workload.always, &dataset, &executable,
                &executable_sha256, &build
            )?,
            "valkey_everysec": interoperability_external_receipt(
                EVERYSEC_LANE, "/run/hyphae-valkey-everysec.sock", 42, &"9".repeat(40),
                &workload.reads, &workload.no, &dataset, &executable,
                &executable_sha256, &build
            )?,
        }});
        let receipt_path = std::env::temp_dir().join(format!(
            "hyphae-valkey-receipt-interop-{}.json",
            std::process::id()
        ));
        let body_path = receipt_path.with_extension("body.json");
        std::fs::write(&body_path, serde_json::to_vec(&body)?)?;
        let producer = std::process::Command::new(&executable)
            .args([
                "--exact",
                "keyspace_suite::tests::receipt_interop_producer_child",
                "--nocapture",
            ])
            .env("HYPHAE_INTEROP_RECEIPT_OUTPUT", &receipt_path)
            .env("HYPHAE_INTEROP_RECEIPT_BODY", &body_path)
            .env("HYPHAE_SOURCE_PRE_COMMIT", &source_commit)
            .env("HYPHAE_SOURCE_PRE_TREE", &source_tree)
            .env("HYPHAE_SOURCE_PRE_CLEAN", "true")
            .env("HYPHAE_SOURCE_POST_COMMIT", &source_commit)
            .env("HYPHAE_SOURCE_POST_TREE", &source_tree)
            .env("HYPHAE_SOURCE_POST_CLEAN", "true")
            .env("HYPHAE_RUSTC", "rustc 1.96.0")
            .env(
                "HYPHAE_RUSTC_VERBOSE",
                "rustc 1.96.0\nrelease: 1.96.0\nbinary: rustc",
            )
            .env("HYPHAE_CARGO_VERSION", "cargo 1.96.0")
            .env("HYPHAE_BUILD_PROFILE", "release")
            .env("HYPHAE_RUSTFLAGS_STATE", "empty")
            .env("HYPHAE_CARGO_ENCODED_RUSTFLAGS_STATE", "empty")
            .env("HYPHAE_RUSTC_WRAPPER_STATE", "unset")
            .env("HYPHAE_RUSTC_WORKSPACE_WRAPPER_STATE", "unset")
            .env("HYPHAE_CARGO_PROFILE_OVERRIDES_STATE", "absent")
            .env("HYPHAE_CARGO_INCREMENTAL_STATE", "disabled")
            .env(
                "HYPHAE_BUILD_COMMAND",
                "cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml",
            )
            .env(
                "HYPHAE_HARNESS_PRODUCT_BINARY_SHA256",
                &executable_sha256,
            )
            .env("HYPHAE_RECEIPT_AUTHORITY", "non-authoritative-diagnostic")
            .env(
                "HYPHAE_HARDWARE_QUALIFICATION",
                "rust-python-interoperability-fixture",
            )
            .env("HYPHAE_EC2_NVME_MODEL", "unqualified")
            .env("HYPHAE_NVME_DEVICE_ID", "unqualified")
            .env("HYPHAE_NVME_FILESYSTEM", "unqualified")
            .env("HYPHAE_NVME_ROTATIONAL", "unqualified")
            .env("HYPHAE_NVME_QUEUE_DEPTH", "unqualified")
            .output()?;
        assert!(
            producer.status.success(),
            "{}",
            String::from_utf8_lossy(&producer.stderr)
        );
        let validator = Path::new(env!("CARGO_MANIFEST_DIR")).join("valkey/check_receipt.py");
        let mut validator_command = std::process::Command::new("python3");
        validator_command
            .arg(validator)
            .arg(&receipt_path)
            .args(["--expected-source-commit", &source_commit])
            .args(["--expected-source-tree", &source_tree])
            .arg("--harness")
            .arg(&executable)
            .arg("--valkey-binary-artifact")
            .arg(&executable)
            .env("PYTHONDONTWRITEBYTECODE", "1");
        if committed_source {
            validator_command.arg("--source-root").arg(repo_root);
        }
        let output = validator_command.output()?;
        std::fs::remove_file(receipt_path)?;
        std::fs::remove_file(body_path)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[test]
    fn sha256_file_is_exact() -> anyhow::Result<()> {
        let path =
            std::env::temp_dir().join(format!("hyphae-baseline-sha256-{}", std::process::id()));
        std::fs::write(&path, b"abc")?;
        let digest = sha256_file(&path)?;
        std::fs::remove_file(path)?;
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        Ok(())
    }
}
