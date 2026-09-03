// SPDX-License-Identifier: Apache-2.0

//! Shared deterministic workload generation, timing, and receipt output.

use std::io::Write;
use std::time::Instant;

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
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu_model": cpu_model,
        "logical_cpus": logical_cpus,
        "kernel": read_trimmed("/proc/sys/kernel/osrelease"),
        "scaling_governor":
            read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "hypervisor_flag": std::fs::read_to_string("/proc/cpuinfo")
            .map(|content| content.contains(" hypervisor"))
            .unwrap_or(false),
    })
}

/// Writes one receipt JSON document to `path`.
pub fn write_receipt(path: &str, benchmark: &str, body: serde_json::Value) -> anyhow::Result<()> {
    let source_commit =
        std::env::var("HYPHAE_SOURCE_COMMIT").unwrap_or_else(|_| "uncommitted".to_owned());
    let rustc = std::env::var("HYPHAE_RUSTC").unwrap_or_else(|_| "unknown".to_owned());
    let receipt = serde_json::json!({
        "benchmark": benchmark,
        "source_commit": source_commit,
        "rustc": rustc,
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
