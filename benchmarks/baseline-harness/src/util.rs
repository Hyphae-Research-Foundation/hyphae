// SPDX-License-Identifier: Apache-2.0

//! Shared deterministic workload generation, timing, and receipt output.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context};
use sha2::{Digest, Sha256};

pub const BASELINE_RECEIPT_SCHEMA: &str = "hyphae-baseline-harness-v2";
const PRODUCTION_KEYSPACE_SEED: u64 = 0x5eed_2026_0829_0001;

/// Deterministic xorshift64* generator so every engine sees identical work.
pub struct Xorshift(u64);

impl Xorshift {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform value in `[0, bound)`.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }

    /// Skewed value in `[0, bound)` biased toward low indices (hot keys).
    pub fn skewed(&mut self, bound: u64) -> u64 {
        let r = self.next_u64() >> 32;
        ((r * r) >> 32) % bound.max(1)
    }
}

/// Latency recorder with exclusive per-operation timing.
pub struct Recorder {
    nanos: Vec<u64>,
    wall: Instant,
}

impl Recorder {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            nanos: Vec::with_capacity(capacity),
            wall: Instant::now(),
        }
    }

    pub fn record<T, E>(&mut self, mut operation: impl FnMut() -> Result<T, E>) -> Result<T, E> {
        let started = Instant::now();
        let value = operation()?;
        let elapsed = started.elapsed().as_nanos();
        self.nanos.push(u64::try_from(elapsed).unwrap_or(u64::MAX));
        Ok(value)
    }

    pub fn summary(mut self, label: &str) -> serde_json::Value {
        let wall_nanos = u64::try_from(self.wall.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.nanos.sort_unstable();
        let count = self.nanos.len();
        let percentile = |numerator: usize, denominator: usize| -> u64 {
            if count == 0 {
                return 0;
            }
            let index = count
                .saturating_mul(numerator)
                .div_ceil(denominator)
                .saturating_sub(1)
                .min(count - 1);
            self.nanos[index]
        };
        let total: u128 = self.nanos.iter().map(|nanos| u128::from(*nanos)).sum();
        let mean = if count == 0 { 0 } else { total / count as u128 };
        let throughput = if wall_nanos == 0 {
            0.0
        } else {
            (count as f64) / (wall_nanos as f64 / 1e9)
        };
        serde_json::json!({
            "label": label,
            "operations": count,
            "wall_nanos": wall_nanos,
            "ops_per_second": throughput,
            "latency_nanos": {
                "total": total,
                "mean": mean,
                "p50": percentile(50, 100),
                "p95": percentile(95, 100),
                "p99": percentile(99, 100),
                "p999": percentile(999, 1000),
                "max": self.nanos.last().copied().unwrap_or(0),
            },
        })
    }
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn mount_source(target: &str) -> Option<String> {
    std::fs::read_to_string("/proc/mounts")
        .ok()?
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let mountpoint = fields.next()?;
            (mountpoint == target).then(|| source.to_owned())
        })
}

fn memory_total_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemTotal:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

fn parse_cpu_set(value: &str) -> Option<BTreeSet<u32>> {
    let mut cpus = BTreeSet::new();
    for component in value.trim().split(',') {
        let (start, end) = component.split_once('-').map_or_else(
            || component.parse::<u32>().ok().map(|cpu| (cpu, cpu)),
            |(start, end)| Some((start.parse().ok()?, end.parse().ok()?)),
        )?;
        if start > end {
            return None;
        }
        for cpu in start..=end {
            if !cpus.insert(cpu) {
                return None;
            }
        }
    }
    Some(cpus)
}

fn cpu_affinity() -> Option<String> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("Cpus_allowed_list:")
                .map(str::trim)
                .map(str::to_owned)
        })
}

fn cpu_quota() -> serde_json::Value {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                let mut fields = line.splitn(3, ':');
                (fields.next()? == "0" && fields.next()?.is_empty())
                    .then(|| fields.next().unwrap_or("/").to_owned())
            })
        });
    let Some(cgroup) = cgroup else {
        return serde_json::json!({"state": "unknown", "millicores": null});
    };
    let relative = cgroup.trim_start_matches('/');
    let path = Path::new("/sys/fs/cgroup").join(relative).join("cpu.max");
    let Some(value) = std::fs::read_to_string(path).ok() else {
        return serde_json::json!({"state": "unknown", "millicores": null});
    };
    let mut fields = value.split_whitespace();
    let quota = fields.next();
    let period = fields.next().and_then(|value| value.parse::<u64>().ok());
    match (quota, period) {
        (Some("max"), Some(_)) => serde_json::json!({"state": "unlimited", "millicores": null}),
        (Some(quota), Some(period)) if period > 0 => {
            let millicores = quota
                .parse::<u64>()
                .ok()
                .map(|quota| quota.saturating_mul(1_000).div_ceil(period));
            serde_json::json!({"state": "limited", "millicores": millicores})
        }
        _ => serde_json::json!({"state": "unknown", "millicores": null}),
    }
}

fn cpu_topology() -> serde_json::Value {
    let mut packages = BTreeSet::new();
    let mut cores = BTreeSet::new();
    let mut logical_ids = BTreeSet::new();
    let mut sibling_sets = Vec::new();
    let mut complete_smt_siblings = true;
    if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(logical_id) = name
                .strip_prefix("cpu")
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            logical_ids.insert(logical_id);
            let topology = entry.path().join("topology");
            let package = std::fs::read_to_string(topology.join("physical_package_id"))
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            let core = std::fs::read_to_string(topology.join("core_id"))
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            if let (Some(package), Some(core)) = (package, core) {
                packages.insert(package);
                cores.insert((package, core));
            }
            let siblings = std::fs::read_to_string(topology.join("thread_siblings_list"))
                .ok()
                .and_then(|value| parse_cpu_set(&value));
            if siblings
                .as_ref()
                .is_none_or(|siblings| siblings.len() != 2 || !siblings.contains(&logical_id))
            {
                complete_smt_siblings = false;
            }
            if let Some(siblings) = siblings {
                sibling_sets.push(siblings);
            }
        }
    }
    let logical = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(0);
    let physical = cores.len();
    let threads_per_core =
        (physical > 0 && logical.is_multiple_of(physical)).then_some(logical / physical);
    serde_json::json!({
        "logical_cpus": logical,
        "physical_cores": (physical > 0).then_some(physical),
        "sockets": (!packages.is_empty()).then_some(packages.len()),
        "threads_per_core": threads_per_core,
        "logical_topology_entries": logical_ids.len(),
        "complete_smt_siblings": complete_smt_siblings
            && logical_ids.len() == logical
            && sibling_sets.iter().all(|siblings| siblings.is_subset(&logical_ids))
            && logical_ids.iter().copied().eq(0..u32::try_from(logical).unwrap_or(0)),
    })
}

fn scaling_governors() -> (Option<Vec<String>>, usize) {
    let mut governors = BTreeSet::new();
    let mut cpu_count = 0_usize;
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return (None, 0);
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix("cpu")
            .and_then(|value| value.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        if let Ok(value) = std::fs::read_to_string(entry.path().join("cpufreq/scaling_governor")) {
            governors.insert(value.trim().to_owned());
            cpu_count += 1;
        }
    }
    (
        (!governors.is_empty()).then(|| governors.into_iter().collect()),
        cpu_count,
    )
}

/// Host fingerprint embedded in every receipt.
pub fn environment() -> serde_json::Value {
    let cpu_model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                line.strip_prefix("model name")
                    .and_then(|rest| rest.split(':').nth(1))
                    .map(|name| name.trim().to_owned())
            })
        });
    let logical_cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(0);
    let (scaling_governors, scaling_governor_cpu_count) = scaling_governors();
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu_model": cpu_model,
        "logical_cpus": logical_cpus,
        "memory_total_kib": memory_total_kib(),
        "cpu_topology": cpu_topology(),
        "cpu_affinity": cpu_affinity(),
        "cpu_quota": cpu_quota(),
        "kernel": read_trimmed("/proc/sys/kernel/osrelease"),
        "hardware_product_name": read_trimmed("/sys/devices/virtual/dmi/id/product_name"),
        "benchmark_storage_source": mount_source("/mnt/nvme"),
        "scaling_governor":
            read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "scaling_governors": scaling_governors,
        "scaling_governor_cpu_count": scaling_governor_cpu_count,
        "hypervisor_flag": std::fs::read_to_string("/proc/cpuinfo")
            .map(|content| content.contains(" hypervisor"))
            .unwrap_or(false),
    })
}

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn required_environment(name: &str) -> anyhow::Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    if value.trim().is_empty() || value == "unknown" || value.contains('\0') {
        bail!("{name} must be a known nonempty text value");
    }
    Ok(value)
}

fn validate_lower_hex(label: &str, value: &str, length: usize) -> anyhow::Result<()> {
    if value.len() != length
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be one {length}-character lowercase hexadecimal identity");
    }
    Ok(())
}

struct SourceBuildIdentity {
    pre_commit: String,
    pre_tree: String,
    pre_clean: String,
    post_commit: String,
    post_tree: String,
    post_clean: String,
}

struct RustBuildIdentity {
    rustc: String,
    rustc_verbose: String,
    cargo: String,
    profile: String,
    rustflags: String,
    cargo_encoded_rustflags: String,
    rustc_wrapper: String,
    rustc_workspace_wrapper: String,
    cargo_profile_overrides: String,
    cargo_incremental: String,
    command: String,
    runner_expected_sha256: String,
}

struct StorageAuthority {
    model: String,
    device: String,
    filesystem: String,
    rotational: String,
    queue_depth: String,
}

fn execution_identity(
    source: SourceBuildIdentity,
    build: RustBuildIdentity,
    executable: &Path,
    executed_sha256: String,
) -> anyhow::Result<serde_json::Value> {
    validate_lower_hex("HYPHAE_SOURCE_PRE_COMMIT", &source.pre_commit, 40)?;
    validate_lower_hex("HYPHAE_SOURCE_PRE_TREE", &source.pre_tree, 40)?;
    validate_lower_hex("HYPHAE_SOURCE_POST_COMMIT", &source.post_commit, 40)?;
    validate_lower_hex("HYPHAE_SOURCE_POST_TREE", &source.post_tree, 40)?;
    validate_lower_hex(
        "HYPHAE_HARNESS_PRODUCT_BINARY_SHA256",
        &build.runner_expected_sha256,
        64,
    )?;
    validate_lower_hex("executed harness/product SHA-256", &executed_sha256, 64)?;
    if source.pre_clean != "true" || source.post_clean != "true" {
        bail!("Hyphae pre-build and post-build worktree state must both be exactly true");
    }
    if source.pre_commit != source.post_commit || source.pre_tree != source.post_tree {
        bail!("Hyphae source identity changed during the harness/product build");
    }
    for (label, value) in [
        ("HYPHAE_RUSTC", build.rustc.as_str()),
        ("HYPHAE_RUSTC_VERBOSE", build.rustc_verbose.as_str()),
        ("HYPHAE_CARGO_VERSION", build.cargo.as_str()),
    ] {
        if value.trim().is_empty() || value == "unknown" || value.len() > 4_096 {
            bail!("{label} must be a bounded known toolchain identity");
        }
    }
    if !build.rustc.starts_with("rustc 1.96.0")
        || !build.rustc_verbose.contains("release: 1.96.0")
        || !build.cargo.starts_with("cargo 1.96.0")
    {
        bail!("Hyphae build toolchain differs from pinned Rust 1.96.0");
    }
    if build.profile != "release"
        || build.rustflags != "empty"
        || build.cargo_encoded_rustflags != "empty"
        || build.rustc_wrapper != "unset"
        || build.rustc_workspace_wrapper != "unset"
        || build.cargo_profile_overrides != "absent"
        || build.cargo_incremental != "disabled"
        || build.command
            != "cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml"
    {
        bail!("Hyphae build profile, flag channels, or command is not the sanitized authority");
    }
    if !executable.is_absolute() {
        bail!("executed harness/product path must be absolute");
    }
    if build.runner_expected_sha256 != executed_sha256 {
        bail!(
            "executed harness/product binary digest differs: expected {}, found {}",
            build.runner_expected_sha256,
            executed_sha256
        );
    }
    Ok(serde_json::json!({
        "source": {
            "pre_build": {
                "commit": source.pre_commit,
                "tree": source.pre_tree,
                "worktree_clean": true,
            },
            "post_build": {
                "commit": source.post_commit,
                "tree": source.post_tree,
                "worktree_clean": true,
            },
            "stable_during_build": true,
        },
        "build": {
            "rustc": build.rustc,
            "rustc_verbose": build.rustc_verbose,
            "cargo": build.cargo,
            "profile": build.profile,
            "rustflags": build.rustflags,
            "cargo_encoded_rustflags": build.cargo_encoded_rustflags,
            "rustc_wrapper": build.rustc_wrapper,
            "rustc_workspace_wrapper": build.rustc_workspace_wrapper,
            "cargo_profile_overrides": build.cargo_profile_overrides,
            "cargo_incremental": build.cargo_incremental,
            "command": build.command,
            "executable": executable,
            "runner_expected_sha256": build.runner_expected_sha256,
            "executed_harness_product_sha256": executed_sha256,
            "embedded_product": true,
        },
    }))
}

fn evidence_authority(
    status: String,
    qualification: String,
    storage: StorageAuthority,
) -> anyhow::Result<serde_json::Value> {
    if qualification.trim().is_empty() || qualification.len() > 1_024 {
        bail!("HYPHAE_HARDWARE_QUALIFICATION must be bounded and nonempty");
    }
    let rotational = match storage.rotational.as_str() {
        "false" => Some(false),
        "true" => Some(true),
        _ => None,
    };
    let queue_depth = storage.queue_depth.parse::<u64>().ok();
    let nvme_device = storage
        .device
        .strip_prefix("259:")
        .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit()));
    match status.as_str() {
        "authoritative-dedicated-hardware"
            if qualification == "aws-ec2-i7i.metal-24xl"
                && storage.model == "Amazon EC2 NVMe Instance Storage"
                && nvme_device
                && matches!(storage.filesystem.as_str(), "ext4" | "xfs")
                && rotational == Some(false)
                && queue_depth.is_some_and(|depth| depth > 0) => {}
        "non-authoritative-diagnostic" if !storage.model.trim().is_empty() => {}
        _ => bail!("receipt authority status or hardware qualification is invalid"),
    }
    Ok(serde_json::json!({
        "status": status,
        "hardware_qualification": qualification,
        "storage": {
            "model": storage.model,
            "device": storage.device,
            "filesystem": storage.filesystem,
            "rotational": rotational,
            "queue_depth": queue_depth,
        },
    }))
}

fn validate_authoritative_workload(
    authority: &serde_json::Value,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    if authority["status"] != "authoritative-dedicated-hardware" {
        return Ok(());
    }
    let Some(workload) = body.get("keyspace").and_then(|value| value.get("workload")) else {
        return Ok(());
    };
    let expected = serde_json::json!({
        "keys": 1_000_000,
        "gets": 500_000,
        "strict_sets": 10_000,
        "relaxed_sets": 200_000,
        "seed": PRODUCTION_KEYSPACE_SEED,
    });
    if workload != &expected {
        bail!("authoritative keyspace receipt requires the exact production workload");
    }
    Ok(())
}

/// Writes one exact-source receipt JSON document to `path`.
pub fn write_receipt(path: &str, body: serde_json::Value) -> anyhow::Result<()> {
    let executable =
        std::env::current_exe().context("resolving executed harness/product binary")?;
    let executed_sha256 =
        sha256_file(&executable).context("digesting executed harness/product binary")?;
    let identity = execution_identity(
        SourceBuildIdentity {
            pre_commit: required_environment("HYPHAE_SOURCE_PRE_COMMIT")?,
            pre_tree: required_environment("HYPHAE_SOURCE_PRE_TREE")?,
            pre_clean: required_environment("HYPHAE_SOURCE_PRE_CLEAN")?,
            post_commit: required_environment("HYPHAE_SOURCE_POST_COMMIT")?,
            post_tree: required_environment("HYPHAE_SOURCE_POST_TREE")?,
            post_clean: required_environment("HYPHAE_SOURCE_POST_CLEAN")?,
        },
        RustBuildIdentity {
            rustc: required_environment("HYPHAE_RUSTC")?,
            rustc_verbose: required_environment("HYPHAE_RUSTC_VERBOSE")?,
            cargo: required_environment("HYPHAE_CARGO_VERSION")?,
            profile: required_environment("HYPHAE_BUILD_PROFILE")?,
            rustflags: required_environment("HYPHAE_RUSTFLAGS_STATE")?,
            cargo_encoded_rustflags: required_environment("HYPHAE_CARGO_ENCODED_RUSTFLAGS_STATE")?,
            rustc_wrapper: required_environment("HYPHAE_RUSTC_WRAPPER_STATE")?,
            rustc_workspace_wrapper: required_environment("HYPHAE_RUSTC_WORKSPACE_WRAPPER_STATE")?,
            cargo_profile_overrides: required_environment("HYPHAE_CARGO_PROFILE_OVERRIDES_STATE")?,
            cargo_incremental: required_environment("HYPHAE_CARGO_INCREMENTAL_STATE")?,
            command: required_environment("HYPHAE_BUILD_COMMAND")?,
            runner_expected_sha256: required_environment("HYPHAE_HARNESS_PRODUCT_BINARY_SHA256")?,
        },
        &executable,
        executed_sha256,
    )?;
    let authority = evidence_authority(
        required_environment("HYPHAE_RECEIPT_AUTHORITY")?,
        required_environment("HYPHAE_HARDWARE_QUALIFICATION")?,
        StorageAuthority {
            model: required_environment("HYPHAE_EC2_NVME_MODEL")?,
            device: required_environment("HYPHAE_NVME_DEVICE_ID")?,
            filesystem: required_environment("HYPHAE_NVME_FILESYSTEM")?,
            rotational: required_environment("HYPHAE_NVME_ROTATIONAL")?,
            queue_depth: required_environment("HYPHAE_NVME_QUEUE_DEPTH")?,
        },
    )?;
    validate_authoritative_workload(&authority, &body)?;
    let receipt = serde_json::json!({
        "schema": BASELINE_RECEIPT_SCHEMA,
        "evidence_authority": authority,
        "hyphae_execution": identity,
        "environment": environment(),
        "results": body,
    });
    let mut file = std::fs::File::create(path)?;
    file.write_all(serde_json::to_string_pretty(&receipt)?.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Fresh directory path (not yet created) under the scratch root.
pub fn fresh_dir(root: &str, label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::path::Path::new(root).join(format!("{label}-{}-{nanos}", std::process::id()))
}

/// Synthetic ASCII corpus: identical tokenization in Hyphae and Tantivy by
/// construction (`wNNNNNN` words split on alphanumeric boundaries in both).
pub fn synthesize_document(rng: &mut Xorshift, vocabulary: u64) -> String {
    let length = 30 + rng.below(31);
    let mut text = String::with_capacity(8 * length as usize);
    for _ in 0..length {
        let word = rng.skewed(vocabulary);
        text.push_str(&format!("w{word:06} "));
    }
    text
}

/// Mid-frequency two-term query over the same vocabulary skew.
pub fn synthesize_query(rng: &mut Xorshift, vocabulary: u64) -> String {
    let first = vocabulary / 20 + rng.skewed(vocabulary / 4);
    let second = vocabulary / 20 + rng.skewed(vocabulary / 4);
    format!("w{first:06} w{second:06}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage(model: &str) -> StorageAuthority {
        StorageAuthority {
            model: model.to_owned(),
            device: "259:7".to_owned(),
            filesystem: "xfs".to_owned(),
            rotational: "false".to_owned(),
            queue_depth: "1023".to_owned(),
        }
    }

    fn source(clean: &str) -> SourceBuildIdentity {
        SourceBuildIdentity {
            pre_commit: "a".repeat(40),
            pre_tree: "b".repeat(40),
            pre_clean: clean.to_owned(),
            post_commit: "a".repeat(40),
            post_tree: "b".repeat(40),
            post_clean: clean.to_owned(),
        }
    }

    fn build(rustc: &str, digest: &str) -> RustBuildIdentity {
        RustBuildIdentity {
            rustc: rustc.to_owned(),
            rustc_verbose: "rustc 1.96.0\nrelease: 1.96.0\nbinary: rustc".to_owned(),
            cargo: "cargo 1.96.0".to_owned(),
            profile: "release".to_owned(),
            rustflags: "empty".to_owned(),
            cargo_encoded_rustflags: "empty".to_owned(),
            rustc_wrapper: "unset".to_owned(),
            rustc_workspace_wrapper: "unset".to_owned(),
            cargo_profile_overrides: "absent".to_owned(),
            cargo_incremental: "disabled".to_owned(),
            command: "cargo build --release --locked --manifest-path benchmarks/baseline-harness/Cargo.toml"
                .to_owned(),
            runner_expected_sha256: digest.to_owned(),
        }
    }

    #[test]
    fn execution_identity_rejects_dirty_source() {
        let error = execution_identity(
            source("false"),
            build("rustc 1.96.0", &"c".repeat(64)),
            Path::new("/tmp/hyphae-baseline-harness"),
            "c".repeat(64),
        )
        .expect_err("dirty and unknown source identity");
        assert!(error.to_string().contains("worktree state"));
    }

    #[test]
    fn execution_identity_rejects_unknown_compiler() {
        let error = execution_identity(
            source("true"),
            build("unknown", &"c".repeat(64)),
            Path::new("/tmp/hyphae-baseline-harness"),
            "c".repeat(64),
        )
        .expect_err("unknown compiler identity");
        assert!(error.to_string().contains("HYPHAE_RUSTC"));
    }

    #[test]
    fn execution_identity_rejects_binary_digest_drift() {
        let error = execution_identity(
            source("true"),
            build("rustc 1.96.0", &"c".repeat(64)),
            Path::new("/tmp/hyphae-baseline-harness"),
            "d".repeat(64),
        )
        .expect_err("binary digest drift");
        assert!(error.to_string().contains("binary digest differs"));
    }

    #[test]
    fn execution_identity_rejects_source_to_binary_drift() {
        let mut source = source("true");
        source.post_tree = "d".repeat(40);
        let error = execution_identity(
            source,
            build("rustc 1.96.0", &"c".repeat(64)),
            Path::new("/tmp/hyphae-baseline-harness"),
            "c".repeat(64),
        )
        .expect_err("source drift");
        assert!(error.to_string().contains("changed during"));
    }

    #[test]
    fn non_authoritative_hardware_is_explicit() -> anyhow::Result<()> {
        let value = evidence_authority(
            "non-authoritative-diagnostic".to_owned(),
            "local-unqualified".to_owned(),
            storage("unqualified"),
        )?;
        assert_eq!(value["status"], "non-authoritative-diagnostic");
        assert!(evidence_authority(
            "authoritative-dedicated-hardware".to_owned(),
            "local-unqualified".to_owned(),
            storage("unqualified"),
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn authoritative_keyspace_workload_is_exact() -> anyhow::Result<()> {
        let authority = evidence_authority(
            "authoritative-dedicated-hardware".to_owned(),
            "aws-ec2-i7i.metal-24xl".to_owned(),
            storage("Amazon EC2 NVMe Instance Storage"),
        )?;
        let exact = serde_json::json!({"keyspace": {"workload": {
            "keys": 1_000_000,
            "gets": 500_000,
            "strict_sets": 10_000,
            "relaxed_sets": 200_000,
            "seed": PRODUCTION_KEYSPACE_SEED,
        }}});
        validate_authoritative_workload(&authority, &exact)?;
        let mut wrong = exact;
        wrong["keyspace"]["workload"]["gets"] = serde_json::json!(499_999);
        assert!(validate_authoritative_workload(&authority, &wrong).is_err());
        Ok(())
    }
}
