// SPDX-License-Identifier: Apache-2.0

//! Read-only hardware discovery for reproducible Native scheduling decisions.

use std::{
    fs, io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    thread,
};

#[cfg(any(target_os = "linux", test))]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const PROFILE_SCHEMA: &str = "hyphae-native-hardware-profile-v1";

/// Failure while reading or normalizing the local hardware profile.
#[derive(Debug, Error)]
pub enum HardwareProfileError {
    /// A required host path could not be resolved.
    #[error("hardware discovery could not resolve {path}: {source}")]
    ResolvePath {
        /// Path being resolved.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// The normalized profile could not be encoded for fingerprinting.
    #[error("hardware profile could not be encoded: {0}")]
    Encode(#[from] serde_json::Error),
    /// A serialized hardware profile could not be decoded.
    #[error("hardware profile receipt could not be decoded: {0}")]
    Decode(serde_json::Error),
    /// A serialized hardware profile did not preserve its schema or fingerprint.
    #[error("hardware profile receipt is invalid: {0}")]
    InvalidReceipt(&'static str),
}

/// One normalized processor cache visible to the current process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareCache {
    /// Cache level.
    pub level: u8,
    /// Kernel-reported cache kind.
    pub kind: String,
    /// Capacity in bytes.
    pub size_bytes: u64,
    /// Coherency line size in bytes when reported.
    pub line_size_bytes: Option<u64>,
    /// Logical processors sharing the cache.
    pub shared_cpu_list: String,
}

/// One logical processor with its physical placement where discoverable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareProcessor {
    /// Operating-system logical processor identifier.
    pub logical_id: u32,
    /// Physical core identifier, scoped by `socket_id`.
    pub core_id: u32,
    /// Physical package identifier.
    pub socket_id: u32,
    /// NUMA node containing this processor when exposed by the operating system.
    pub numa_node_id: Option<u32>,
    /// Canonical logical sibling list for the physical core.
    pub thread_siblings: String,
}

/// Normalized processor and topology discovery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareCpu {
    /// Rust target architecture.
    pub architecture: String,
    /// Logical processors available to this process.
    pub logical_processors_available: usize,
    /// Physical cores visible in the admitted affinity set when discoverable.
    pub physical_cores_visible: Option<usize>,
    /// Logical threads per visible physical core when uniform and discoverable.
    pub smt_threads_per_core: Option<usize>,
    /// Physical packages visible in the admitted affinity set when discoverable.
    pub sockets_visible: Option<usize>,
    /// NUMA nodes visible to the host when discoverable.
    pub numa_nodes_visible: Option<usize>,
    /// Kernel affinity list, or `unknown` when unavailable.
    pub affinity: String,
    /// Effective cgroup quota in thousandths of one CPU when bounded.
    pub quota_millicores: Option<u64>,
    /// Runtime-detected instruction-set extensions in canonical order.
    pub instruction_sets: Vec<String>,
    /// Distinct cache domains visible to the admitted processor set.
    pub caches: Vec<HardwareCache>,
    /// Per-logical-processor physical placement inside the admitted affinity set.
    pub processor_topology: Vec<HardwareProcessor>,
    /// Active frequency governors in canonical order.
    pub frequency_governors: Vec<String>,
}

/// CPU and memory placement reported for one NUMA node.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareNumaNode {
    /// Operating-system node identifier.
    pub id: u32,
    /// Logical processors assigned to the node.
    pub cpu_list: String,
    /// Installed memory on the node.
    pub total_bytes: Option<u64>,
    /// Memory available on the node at discovery time.
    pub available_bytes: Option<u64>,
}

/// Normalized memory discovery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareMemory {
    /// Installed memory visible to the operating system.
    pub total_bytes: Option<u64>,
    /// Memory available at discovery time. Excluded from the stable fingerprint.
    pub available_bytes: Option<u64>,
    /// Base kernel page size.
    pub page_size_bytes: Option<u64>,
    /// Configured huge-page size.
    pub huge_page_size_bytes: Option<u64>,
    /// Configured persistent huge-page count.
    pub huge_pages_total: Option<u64>,
    /// Per-node placement where the operating system exposes NUMA topology.
    pub numa_nodes: Vec<HardwareNumaNode>,
}

/// Storage and filesystem properties for the selected Native data path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareStorage {
    /// Canonical existing path used for mount resolution.
    pub path: String,
    /// Filesystem type when discoverable.
    pub filesystem: Option<String>,
    /// Kernel device identity when discoverable.
    pub device: Option<String>,
    /// Canonically ordered mount options.
    pub mount_options: Vec<String>,
    /// Whether the kernel reports rotational media.
    pub rotational: Option<bool>,
    /// Block-device request queue depth.
    pub queue_depth: Option<u64>,
    /// Maximum discard request size in bytes.
    pub discard_max_bytes: Option<u64>,
}

/// Operating-system properties that affect calibrated decisions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareOperatingSystem {
    /// Rust target operating system.
    pub family: String,
    /// Kernel release when discoverable.
    pub kernel_release: String,
    /// `none`, a detected virtualization class, or `unknown`.
    pub virtualization: String,
    /// Direct local product transports compiled for this platform.
    pub local_transports: Vec<String>,
}

/// Read-only hardware snapshot used by calibration and scheduling.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareProfile {
    /// Versioned profile schema.
    pub schema: String,
    /// Stable digest of scheduling-relevant fields.
    pub fingerprint: String,
    /// Processor discovery.
    pub cpu: HardwareCpu,
    /// Memory discovery.
    pub memory: HardwareMemory,
    /// Storage discovery for the selected data path.
    pub storage: HardwareStorage,
    /// Operating-system discovery.
    pub operating_system: HardwareOperatingSystem,
}

#[derive(Serialize)]
struct HardwareFingerprint<'a> {
    schema: &'a str,
    cpu: &'a HardwareCpu,
    total_memory_bytes: Option<u64>,
    page_size_bytes: Option<u64>,
    huge_page_size_bytes: Option<u64>,
    huge_pages_total: Option<u64>,
    numa_nodes: Vec<HardwareNumaFingerprint<'a>>,
    storage: HardwareStorageFingerprint<'a>,
    operating_system: &'a HardwareOperatingSystem,
}

#[derive(Serialize)]
struct HardwareNumaFingerprint<'a> {
    id: u32,
    cpu_list: &'a str,
    total_bytes: Option<u64>,
}

#[derive(Serialize)]
struct HardwareStorageFingerprint<'a> {
    filesystem: &'a Option<String>,
    device: &'a Option<String>,
    mount_options: &'a [String],
    rotational: Option<bool>,
    queue_depth: Option<u64>,
    discard_max_bytes: Option<u64>,
}

impl HardwareProfile {
    /// Decodes and verifies one immutable discovery receipt.
    ///
    /// # Errors
    ///
    /// Returns [`HardwareProfileError`] when the receipt is malformed, uses a
    /// different schema, or its scheduling-relevant fingerprint is invalid.
    pub fn from_json_slice(encoded: &[u8]) -> Result<Self, HardwareProfileError> {
        let profile: Self =
            serde_json::from_slice(encoded).map_err(HardwareProfileError::Decode)?;
        if profile.schema != PROFILE_SCHEMA {
            return Err(HardwareProfileError::InvalidReceipt("unexpected schema"));
        }
        if profile.fingerprint != profile.computed_fingerprint()? {
            return Err(HardwareProfileError::InvalidReceipt(
                "fingerprint does not match receipt fields",
            ));
        }
        Ok(profile)
    }

    /// Discovers hardware relevant to the current process and `data_path`.
    ///
    /// Discovery is read-only. If `data_path` does not exist, its nearest
    /// existing ancestor is used for filesystem and device resolution.
    ///
    /// # Errors
    ///
    /// Returns [`HardwareProfileError`] when no existing ancestor can be
    /// resolved or the normalized fingerprint input cannot be encoded.
    pub fn discover(data_path: impl AsRef<Path>) -> Result<Self, HardwareProfileError> {
        let storage_path = existing_ancestor(data_path.as_ref())?;
        let logical_processors_available =
            thread::available_parallelism().map_or(1, NonZeroUsize::get);

        #[cfg(target_os = "linux")]
        let (cpu, memory, storage, operating_system) = discover_linux(
            &storage_path,
            logical_processors_available,
            Path::new("/proc"),
            Path::new("/sys"),
        );

        #[cfg(target_os = "macos")]
        let (cpu, memory, storage, operating_system) =
            discover_macos(&storage_path, logical_processors_available);

        #[cfg(target_os = "windows")]
        let (cpu, memory, storage, operating_system) =
            discover_windows(&storage_path, logical_processors_available);

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let (cpu, memory, storage, operating_system) =
            discover_portable(&storage_path, logical_processors_available);

        let mut profile = Self {
            schema: PROFILE_SCHEMA.to_owned(),
            fingerprint: String::new(),
            cpu,
            memory,
            storage,
            operating_system,
        };
        profile.fingerprint = profile.computed_fingerprint()?;
        Ok(profile)
    }

    fn computed_fingerprint(&self) -> Result<String, HardwareProfileError> {
        let encoded = serde_json::to_vec(&HardwareFingerprint {
            schema: PROFILE_SCHEMA,
            cpu: &self.cpu,
            total_memory_bytes: self.memory.total_bytes,
            page_size_bytes: self.memory.page_size_bytes,
            huge_page_size_bytes: self.memory.huge_page_size_bytes,
            huge_pages_total: self.memory.huge_pages_total,
            numa_nodes: self
                .memory
                .numa_nodes
                .iter()
                .map(|node| HardwareNumaFingerprint {
                    id: node.id,
                    cpu_list: &node.cpu_list,
                    total_bytes: node.total_bytes,
                })
                .collect(),
            storage: HardwareStorageFingerprint {
                filesystem: &self.storage.filesystem,
                device: &self.storage.device,
                mount_options: &self.storage.mount_options,
                rotational: self.storage.rotational,
                queue_depth: self.storage.queue_depth,
                discard_max_bytes: self.storage.discard_max_bytes,
            },
            operating_system: &self.operating_system,
        })?;
        Ok(blake3::hash(&encoded).to_hex().to_string())
    }
}

fn existing_ancestor(path: &Path) -> Result<PathBuf, HardwareProfileError> {
    let mut candidate = path;
    while !candidate.exists() {
        candidate = candidate
            .parent()
            .ok_or_else(|| HardwareProfileError::ResolvePath {
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::NotFound, "path has no existing ancestor"),
            })?;
    }
    fs::canonicalize(candidate).map_err(|source| HardwareProfileError::ResolvePath {
        path: candidate.to_path_buf(),
        source,
    })
}

fn uniform_threads_per_core(
    logical_processors: usize,
    physical_cores: Option<usize>,
) -> Option<usize> {
    let physical_cores = physical_cores?;
    if physical_cores == 0 || !logical_processors.is_multiple_of(physical_cores) {
        return None;
    }
    Some(logical_processors / physical_cores)
}

#[cfg(target_os = "macos")]
fn discover_macos(
    storage_path: &Path,
    logical_processors_available: usize,
) -> (
    HardwareCpu,
    HardwareMemory,
    HardwareStorage,
    HardwareOperatingSystem,
) {
    let line_size = sysctl_u64("hw.cachelinesize");
    let caches = [
        (1, "data", "hw.l1dcachesize"),
        (1, "instruction", "hw.l1icachesize"),
        (2, "unified", "hw.l2cachesize"),
        (3, "unified", "hw.l3cachesize"),
    ]
    .into_iter()
    .filter_map(|(level, kind, key)| {
        let size_bytes = sysctl_u64(key)?;
        (size_bytes > 0).then(|| HardwareCache {
            level,
            kind: kind.to_owned(),
            size_bytes,
            line_size_bytes: line_size,
            shared_cpu_list: "unknown".to_owned(),
        })
    })
    .collect();
    let physical_cores_visible = sysctl_usize("hw.physicalcpu");
    let cpu = HardwareCpu {
        architecture: std::env::consts::ARCH.to_owned(),
        logical_processors_available,
        physical_cores_visible,
        smt_threads_per_core: uniform_threads_per_core(
            logical_processors_available,
            physical_cores_visible,
        ),
        sockets_visible: sysctl_usize("hw.packages").or(Some(1)),
        numa_nodes_visible: None,
        affinity: "process-default".to_owned(),
        quota_millicores: None,
        instruction_sets: detected_instruction_sets(),
        caches,
        processor_topology: Vec::new(),
        frequency_governors: Vec::new(),
    };
    let memory = HardwareMemory {
        total_bytes: sysctl_u64("hw.memsize"),
        available_bytes: None,
        page_size_bytes: sysctl_u64("hw.pagesize"),
        huge_page_size_bytes: None,
        huge_pages_total: None,
        numa_nodes: Vec::new(),
    };
    let storage = discover_macos_storage(storage_path);
    let virtualization = match sysctl_u64("kern.hv_vmm_present") {
        Some(0) => "none",
        Some(_) => "hypervisor",
        None => "unknown",
    }
    .to_owned();
    let operating_system = HardwareOperatingSystem {
        family: std::env::consts::OS.to_owned(),
        kernel_release: sysctl_value("kern.osrelease").unwrap_or_else(|| "unknown".to_owned()),
        virtualization,
        local_transports: vec!["embedded".to_owned(), "unix-domain-socket".to_owned()],
    };
    (cpu, memory, storage, operating_system)
}

#[cfg(target_os = "macos")]
fn discover_macos_storage(storage_path: &Path) -> HardwareStorage {
    let mount = command_output("/sbin/mount", &[])
        .as_deref()
        .and_then(|value| resolve_macos_mount(value, storage_path));
    HardwareStorage {
        path: storage_path.display().to_string(),
        filesystem: mount.as_ref().map(|value| value.filesystem.clone()),
        device: mount.as_ref().map(|value| value.device.clone()),
        mount_options: mount.map_or_else(Vec::new, |value| value.options),
        rotational: None,
        queue_depth: None,
        discard_max_bytes: None,
    }
}

#[cfg(target_os = "macos")]
fn sysctl_value(name: &str) -> Option<String> {
    command_output("/usr/sbin/sysctl", &["-n", name])
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    sysctl_value(name)?.trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn sysctl_usize(name: &str) -> Option<usize> {
    sysctl_value(name)?.trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}

#[cfg(target_os = "macos")]
fn resolve_macos_mount(mounts: &str, target: &Path) -> Option<MountIdentity> {
    mounts
        .lines()
        .filter_map(parse_macos_mount_line)
        .filter(|mount| target.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.as_os_str().len())
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_mount_line(line: &str) -> Option<MountIdentity> {
    let (device, rest) = line.split_once(" on ")?;
    let (mount_point, details) = rest.rsplit_once(" (")?;
    let details = details.strip_suffix(')')?;
    let mut values = details.split(',').map(str::trim);
    let filesystem = values.next()?.to_owned();
    let mut options = values
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    options.sort();
    options.dedup();
    Some(MountIdentity {
        device: device.to_owned(),
        mount_point: PathBuf::from(mount_point),
        filesystem,
        options,
    })
}

#[cfg(target_os = "linux")]
fn discover_linux(
    storage_path: &Path,
    logical_processors_available: usize,
    proc_root: &Path,
    sys_root: &Path,
) -> (
    HardwareCpu,
    HardwareMemory,
    HardwareStorage,
    HardwareOperatingSystem,
) {
    let cpuinfo = read_optional(proc_root.join("cpuinfo"));
    let status = read_optional(proc_root.join("self/status"));
    let meminfo = read_optional(proc_root.join("meminfo"));
    let smaps = read_optional(proc_root.join("self/smaps"));
    let kernel_release = read_optional(proc_root.join("sys/kernel/osrelease"))
        .unwrap_or_else(|| "unknown".to_owned())
        .trim()
        .to_owned();
    let affinity = status
        .as_deref()
        .and_then(|value| colon_value(value, "Cpus_allowed_list"))
        .unwrap_or("unknown")
        .to_owned();
    let allowed = parse_cpu_list(&affinity);
    let cpuinfo_topology = cpuinfo
        .as_deref()
        .map_or((None, None), |value| physical_topology(value, &allowed));
    let (physical_cores_visible, sockets_visible) = if cpuinfo_topology.0.is_some() {
        cpuinfo_topology
    } else {
        sysfs_physical_topology(sys_root, &allowed)
    };
    let quota_millicores = discover_linux_cpu_quota(proc_root, sys_root);
    let numa_nodes = discover_linux_numa_nodes(sys_root, &allowed);
    let processor_topology = discover_linux_processor_topology(sys_root, &allowed, &numa_nodes);
    let cpu = HardwareCpu {
        architecture: std::env::consts::ARCH.to_owned(),
        logical_processors_available,
        physical_cores_visible,
        smt_threads_per_core: uniform_threads_per_core(
            logical_processors_available,
            physical_cores_visible,
        ),
        sockets_visible,
        numa_nodes_visible: (!numa_nodes.is_empty()).then_some(numa_nodes.len()),
        affinity,
        quota_millicores,
        instruction_sets: detected_instruction_sets(),
        caches: discover_linux_caches(sys_root, &allowed),
        processor_topology,
        frequency_governors: discover_linux_governors(sys_root),
    };
    let memory = HardwareMemory {
        total_bytes: meminfo
            .as_deref()
            .and_then(|value| meminfo_bytes(value, "MemTotal")),
        available_bytes: meminfo
            .as_deref()
            .and_then(|value| meminfo_bytes(value, "MemAvailable")),
        page_size_bytes: smaps
            .as_deref()
            .and_then(|value| meminfo_bytes(value, "KernelPageSize")),
        huge_page_size_bytes: meminfo
            .as_deref()
            .and_then(|value| meminfo_bytes(value, "Hugepagesize")),
        huge_pages_total: meminfo
            .as_deref()
            .and_then(|value| plain_numeric_value(value, "HugePages_Total")),
        numa_nodes,
    };
    let mountinfo = read_optional(proc_root.join("self/mountinfo"));
    let storage = discover_linux_storage(storage_path, mountinfo.as_deref(), sys_root);
    let virtualization = detect_linux_virtualization(
        cpuinfo.as_deref(),
        &kernel_release,
        read_optional(proc_root.join("1/cgroup")).as_deref(),
        mountinfo.as_deref(),
    );
    let operating_system = HardwareOperatingSystem {
        family: std::env::consts::OS.to_owned(),
        kernel_release,
        virtualization,
        local_transports: vec!["embedded".to_owned(), "unix-domain-socket".to_owned()],
    };
    (cpu, memory, storage, operating_system)
}

#[cfg(target_os = "linux")]
fn discover_linux_cpu_quota(proc_root: &Path, sys_root: &Path) -> Option<u64> {
    let cgroup_root = sys_root.join("fs/cgroup");
    let process_cgroup = read_optional(proc_root.join("self/cgroup"))
        .as_deref()
        .and_then(parse_cgroup_v2_path)
        .and_then(|path| safe_cgroup_path(&cgroup_root, path));
    process_cgroup
        .as_ref()
        .and_then(|path| read_optional(path.join("cpu.max")))
        .or_else(|| read_optional(cgroup_root.join("cpu.max")))
        .as_deref()
        .and_then(parse_cpu_max)
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WindowsHardwareProbe {
    #[serde(default)]
    processors: Vec<WindowsProcessorProbe>,
    total_memory_bytes: Option<u64>,
    available_memory_bytes: Option<u64>,
    page_size_bytes: Option<u64>,
    kernel_release: Option<String>,
    manufacturer: Option<String>,
    model: Option<String>,
    disk: Option<WindowsDiskProbe>,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Deserialize)]
struct WindowsProcessorProbe {
    cores: Option<usize>,
    logical: Option<usize>,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Deserialize)]
struct WindowsDiskProbe {
    device: Option<String>,
    filesystem: Option<String>,
}

#[cfg(target_os = "windows")]
const WINDOWS_DISCOVERY_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$processors = @(Get-CimInstance Win32_Processor | ForEach-Object {
  [pscustomobject]@{ cores = [uint64]$_.NumberOfCores; logical = [uint64]$_.NumberOfLogicalProcessors }
})
$os = Get-CimInstance Win32_OperatingSystem
$computer = Get-CimInstance Win32_ComputerSystem
$root = [IO.Path]::GetPathRoot($env:HYPHAE_DISCOVERY_PATH)
$device = if ($root -and $root.Length -ge 2) { $root.Substring(0, 2) } else { $null }
$logicalDisk = if ($device) { Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='$device'" } else { $null }
$disk = if ($logicalDisk) { [pscustomobject]@{ device = [string]$logicalDisk.DeviceID; filesystem = [string]$logicalDisk.FileSystem } } else { $null }
[pscustomobject]@{
  processors = $processors
  totalMemoryBytes = [uint64]$computer.TotalPhysicalMemory
  availableMemoryBytes = [uint64]$os.FreePhysicalMemory * 1024
  pageSizeBytes = [uint64][Environment]::SystemPageSize
  kernelRelease = [string]$os.Version
  manufacturer = [string]$computer.Manufacturer
  model = [string]$computer.Model
  disk = $disk
} | ConvertTo-Json -Compress -Depth 4
"#;

#[cfg(target_os = "windows")]
fn discover_windows(
    storage_path: &Path,
    logical_processors_available: usize,
) -> (
    HardwareCpu,
    HardwareMemory,
    HardwareStorage,
    HardwareOperatingSystem,
) {
    let probe = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            WINDOWS_DISCOVERY_SCRIPT,
        ])
        .env("HYPHAE_DISCOVERY_PATH", storage_path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .as_deref()
        .and_then(parse_windows_probe);
    windows_profile_from_probe(storage_path, logical_processors_available, probe.as_ref())
}

#[cfg(any(target_os = "windows", test))]
fn parse_windows_probe(encoded: &str) -> Option<WindowsHardwareProbe> {
    serde_json::from_str(encoded.trim().trim_start_matches('\u{feff}')).ok()
}

#[cfg(any(target_os = "windows", test))]
fn windows_profile_from_probe(
    storage_path: &Path,
    logical_processors_available: usize,
    probe: Option<&WindowsHardwareProbe>,
) -> (
    HardwareCpu,
    HardwareMemory,
    HardwareStorage,
    HardwareOperatingSystem,
) {
    let reported_physical = probe.and_then(|value| {
        value
            .processors
            .iter()
            .try_fold(0_usize, |total, processor| {
                total.checked_add(processor.cores?)
            })
    });
    let reported_logical = probe.and_then(|value| {
        value
            .processors
            .iter()
            .try_fold(0_usize, |total, processor| {
                total.checked_add(processor.logical?)
            })
    });
    let physical_cores_visible = reported_physical
        .map(|cores| cores.min(logical_processors_available))
        .filter(|cores| *cores > 0);
    let sockets_visible = probe
        .map(|value| value.processors.len())
        .filter(|sockets| *sockets > 0);
    let cpu = HardwareCpu {
        architecture: std::env::consts::ARCH.to_owned(),
        logical_processors_available,
        physical_cores_visible,
        smt_threads_per_core: uniform_threads_per_core(
            reported_logical.map_or(logical_processors_available, |reported| {
                reported.min(logical_processors_available)
            }),
            physical_cores_visible,
        ),
        sockets_visible,
        numa_nodes_visible: None,
        affinity: "process-visible-mask-unresolved".to_owned(),
        quota_millicores: None,
        instruction_sets: detected_instruction_sets(),
        caches: Vec::new(),
        processor_topology: Vec::new(),
        frequency_governors: Vec::new(),
    };
    let memory = HardwareMemory {
        total_bytes: probe.and_then(|value| value.total_memory_bytes),
        available_bytes: probe.and_then(|value| value.available_memory_bytes),
        page_size_bytes: probe.and_then(|value| value.page_size_bytes),
        huge_page_size_bytes: None,
        huge_pages_total: None,
        numa_nodes: Vec::new(),
    };
    let disk = probe.and_then(|value| value.disk.as_ref());
    let storage = HardwareStorage {
        path: storage_path.display().to_string(),
        filesystem: disk.and_then(|value| nonempty(value.filesystem.as_deref())),
        device: disk.and_then(|value| nonempty(value.device.as_deref())),
        mount_options: Vec::new(),
        rotational: None,
        queue_depth: None,
        discard_max_bytes: None,
    };
    let operating_system = HardwareOperatingSystem {
        family: "windows".to_owned(),
        kernel_release: probe
            .and_then(|value| nonempty(value.kernel_release.as_deref()))
            .unwrap_or_else(|| "unknown".to_owned()),
        virtualization: windows_virtualization(
            probe.and_then(|value| value.manufacturer.as_deref()),
            probe.and_then(|value| value.model.as_deref()),
        ),
        local_transports: vec!["embedded".to_owned(), "named-pipe".to_owned()],
    };
    (cpu, memory, storage, operating_system)
}

#[cfg(any(target_os = "windows", test))]
fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(any(target_os = "windows", test))]
fn windows_virtualization(manufacturer: Option<&str>, model: Option<&str>) -> String {
    let identity = format!(
        "{} {}",
        manufacturer.unwrap_or_default(),
        model.unwrap_or_default()
    )
    .to_ascii_lowercase();
    if [
        "virtual",
        "vmware",
        "kvm",
        "hyper-v",
        "virtualbox",
        "parallels",
        "xen",
    ]
    .iter()
    .any(|marker| identity.contains(marker))
    {
        "hypervisor".to_owned()
    } else if manufacturer.is_some() || model.is_some() {
        "none".to_owned()
    } else {
        "unknown".to_owned()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn discover_portable(
    storage_path: &Path,
    logical_processors_available: usize,
) -> (
    HardwareCpu,
    HardwareMemory,
    HardwareStorage,
    HardwareOperatingSystem,
) {
    (
        HardwareCpu {
            architecture: std::env::consts::ARCH.to_owned(),
            logical_processors_available,
            physical_cores_visible: None,
            smt_threads_per_core: None,
            sockets_visible: None,
            numa_nodes_visible: None,
            affinity: "unknown".to_owned(),
            quota_millicores: None,
            instruction_sets: detected_instruction_sets(),
            caches: Vec::new(),
            processor_topology: Vec::new(),
            frequency_governors: Vec::new(),
        },
        HardwareMemory {
            total_bytes: None,
            available_bytes: None,
            page_size_bytes: None,
            huge_page_size_bytes: None,
            huge_pages_total: None,
            numa_nodes: Vec::new(),
        },
        HardwareStorage {
            path: storage_path.display().to_string(),
            filesystem: None,
            device: None,
            mount_options: Vec::new(),
            rotational: None,
            queue_depth: None,
            discard_max_bytes: None,
        },
        HardwareOperatingSystem {
            family: std::env::consts::OS.to_owned(),
            kernel_release: "unknown".to_owned(),
            virtualization: "unknown".to_owned(),
            local_transports: vec!["embedded".to_owned()],
        },
    )
}

#[cfg(target_os = "linux")]
fn discover_linux_caches(sys_root: &Path, allowed: &BTreeSet<u32>) -> Vec<HardwareCache> {
    let cpu_root = sys_root.join("devices/system/cpu");
    let cpu_ids = if allowed.is_empty() {
        read_optional(cpu_root.join("online"))
            .map(|value| parse_cpu_list(value.trim()))
            .unwrap_or_default()
    } else {
        allowed.clone()
    };
    let mut caches = BTreeMap::new();
    for cpu in cpu_ids {
        let cache_root = cpu_root.join(format!("cpu{cpu}/cache"));
        for entry in fs::read_dir(cache_root)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            let Some(level) =
                read_optional(path.join("level")).and_then(|value| value.trim().parse::<u8>().ok())
            else {
                continue;
            };
            let Some(kind) =
                read_optional(path.join("type")).map(|value| value.trim().to_ascii_lowercase())
            else {
                continue;
            };
            let Some(size_bytes) =
                read_optional(path.join("size")).and_then(|value| parse_scaled_bytes(value.trim()))
            else {
                continue;
            };
            let line_size_bytes = read_optional(path.join("coherency_line_size"))
                .and_then(|value| value.trim().parse().ok());
            let shared_cpu_list = read_optional(path.join("shared_cpu_list"))
                .unwrap_or_else(|| "unknown".to_owned())
                .trim()
                .to_owned();
            let identity = (
                level,
                kind.clone(),
                size_bytes,
                line_size_bytes,
                shared_cpu_list.clone(),
            );
            caches.entry(identity).or_insert(HardwareCache {
                level,
                kind,
                size_bytes,
                line_size_bytes,
                shared_cpu_list,
            });
        }
    }
    caches.into_values().collect()
}

#[cfg(target_os = "linux")]
fn discover_linux_governors(sys_root: &Path) -> Vec<String> {
    let root = sys_root.join("devices/system/cpu/cpufreq");
    let mut values = fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| read_optional(entry.path().join("scaling_governor")))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    values.sort();
    values
}

#[cfg(target_os = "linux")]
fn discover_linux_storage(
    storage_path: &Path,
    mountinfo: Option<&str>,
    sys_root: &Path,
) -> HardwareStorage {
    let mount = mountinfo.and_then(|value| resolve_mount(value, storage_path));
    let queue_root = mount
        .as_ref()
        .and_then(|value| fs::canonicalize(sys_root.join("dev/block").join(&value.device)).ok())
        .and_then(find_queue_root);
    HardwareStorage {
        path: storage_path.display().to_string(),
        filesystem: mount.as_ref().map(|value| value.filesystem.clone()),
        device: mount.as_ref().map(|value| value.device.clone()),
        mount_options: mount
            .as_ref()
            .map_or_else(Vec::new, |value| value.options.clone()),
        rotational: queue_root.as_ref().and_then(|root| {
            read_optional(root.join("queue/rotational"))
                .as_deref()
                .and_then(|value| match value.trim() {
                    "0" => Some(false),
                    "1" => Some(true),
                    _ => None,
                })
        }),
        queue_depth: queue_root.as_ref().and_then(|root| {
            read_optional(root.join("queue/nr_requests"))
                .as_deref()
                .and_then(|value| value.trim().parse().ok())
        }),
        discard_max_bytes: queue_root.as_ref().and_then(|root| {
            read_optional(root.join("queue/discard_max_bytes"))
                .as_deref()
                .and_then(|value| value.trim().parse().ok())
        }),
    }
}

#[cfg(target_os = "linux")]
fn find_queue_root(mut path: PathBuf) -> Option<PathBuf> {
    loop {
        if path.join("queue").is_dir() {
            return Some(path);
        }
        path = path.parent()?.to_path_buf();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct MountIdentity {
    device: String,
    mount_point: PathBuf,
    filesystem: String,
    options: Vec<String>,
}

#[cfg(any(target_os = "linux", test))]
fn resolve_mount(mountinfo: &str, target: &Path) -> Option<MountIdentity> {
    mountinfo
        .lines()
        .filter_map(parse_mountinfo_line)
        .filter(|mount| target.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.as_os_str().len())
}

#[cfg(any(target_os = "linux", test))]
fn parse_mountinfo_line(line: &str) -> Option<MountIdentity> {
    let (left, right) = line.split_once(" - ")?;
    let left = left.split_ascii_whitespace().collect::<Vec<_>>();
    let right = right.split_ascii_whitespace().collect::<Vec<_>>();
    if left.len() < 6 || right.len() < 2 {
        return None;
    }
    let mut options = left[5]
        .split(',')
        .chain(right.get(2).into_iter().flat_map(|value| value.split(',')))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    options.sort();
    Some(MountIdentity {
        device: left[2].to_owned(),
        mount_point: PathBuf::from(unescape_mount_path(left[4])),
        filesystem: right[0].to_owned(),
        options,
    })
}

#[cfg(any(target_os = "linux", test))]
fn unescape_mount_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(any(target_os = "linux", test))]
fn read_optional(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

#[cfg(any(target_os = "linux", test))]
fn colon_value<'a>(input: &'a str, name: &str) -> Option<&'a str> {
    input.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key == name).then(|| value.trim())
    })
}

#[cfg(any(target_os = "linux", test))]
fn meminfo_bytes(input: &str, name: &str) -> Option<u64> {
    colon_value(input, name).and_then(parse_kib)
}

#[cfg(target_os = "linux")]
fn plain_numeric_value(input: &str, name: &str) -> Option<u64> {
    colon_value(input, name)?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(any(target_os = "linux", test))]
fn parse_kib(value: &str) -> Option<u64> {
    let mut fields = value.split_ascii_whitespace();
    let amount = fields.next()?.parse::<u64>().ok()?;
    match fields.next() {
        Some("kB") => amount.checked_mul(1024),
        None => Some(amount),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn parse_scaled_bytes(value: &str) -> Option<u64> {
    let split = value.find(|character: char| !character.is_ascii_digit())?;
    let amount = value[..split].parse::<u64>().ok()?;
    match &value[split..] {
        "K" => amount.checked_mul(1024),
        "M" => amount.checked_mul(1024 * 1024),
        "G" => amount.checked_mul(1024 * 1024 * 1024),
        _ => None,
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_cpu_max(value: &str) -> Option<u64> {
    let mut fields = value.split_ascii_whitespace();
    let quota = fields.next()?;
    let period = fields.next()?.parse::<u64>().ok()?;
    if quota == "max" || period == 0 {
        return None;
    }
    quota
        .parse::<u64>()
        .ok()?
        .checked_mul(1000)?
        .checked_div(period)
}

#[cfg(any(target_os = "linux", test))]
fn parse_cgroup_v2_path(value: &str) -> Option<&str> {
    value.lines().find_map(|line| line.strip_prefix("0::"))
}

#[cfg(any(target_os = "linux", test))]
fn safe_cgroup_path(root: &Path, value: &str) -> Option<PathBuf> {
    let relative = value.trim_start_matches('/');
    if relative
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        return None;
    }
    Some(root.join(relative))
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_cpu_list(value: &str) -> BTreeSet<u32> {
    let mut cpus = BTreeSet::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let (Ok(start), Ok(end)) = (start.parse::<u32>(), end.parse::<u32>()) else {
                continue;
            };
            cpus.extend(start..=end);
        } else if let Ok(cpu) = part.parse() {
            cpus.insert(cpu);
        }
    }
    cpus
}

#[cfg(any(target_os = "linux", test))]
fn format_cpu_list(cpus: &BTreeSet<u32>) -> String {
    let mut ranges = Vec::new();
    let mut values = cpus.iter().copied();
    let Some(mut start) = values.next() else {
        return String::new();
    };
    let mut end = start;
    for value in values {
        if value == end.saturating_add(1) {
            end = value;
            continue;
        }
        ranges.push((start, end));
        start = value;
        end = value;
    }
    ranges.push((start, end));
    ranges
        .into_iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(any(target_os = "linux", test))]
fn physical_topology(cpuinfo: &str, allowed: &BTreeSet<u32>) -> (Option<usize>, Option<usize>) {
    let mut cores = BTreeSet::new();
    let mut sockets = BTreeSet::new();
    for record in cpuinfo.split("\n\n") {
        let fields = record
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim(), value.trim()))
            .collect::<BTreeMap<_, _>>();
        let Some(processor) = fields.get("processor").and_then(|value| value.parse().ok()) else {
            continue;
        };
        if !allowed.is_empty() && !allowed.contains(&processor) {
            continue;
        }
        let (Some(package), Some(core)) = (fields.get("physical id"), fields.get("core id")) else {
            continue;
        };
        sockets.insert((*package).to_owned());
        cores.insert(((*package).to_owned(), (*core).to_owned()));
    }
    (
        (!cores.is_empty()).then_some(cores.len()),
        (!sockets.is_empty()).then_some(sockets.len()),
    )
}

#[cfg(target_os = "linux")]
fn sysfs_physical_topology(
    sys_root: &Path,
    allowed: &BTreeSet<u32>,
) -> (Option<usize>, Option<usize>) {
    let mut cores = BTreeSet::new();
    let mut sockets = BTreeSet::new();
    for cpu in allowed {
        let topology = sys_root
            .join("devices/system/cpu")
            .join(format!("cpu{cpu}"))
            .join("topology");
        let (Some(package), Some(core)) = (
            read_optional(topology.join("physical_package_id")),
            read_optional(topology.join("core_id")),
        ) else {
            continue;
        };
        let package = package.trim().to_owned();
        let core = core.trim().to_owned();
        sockets.insert(package.clone());
        cores.insert((package, core));
    }
    (
        (!cores.is_empty()).then_some(cores.len()),
        (!sockets.is_empty()).then_some(sockets.len()),
    )
}

#[cfg(target_os = "linux")]
fn discover_linux_numa_nodes(sys_root: &Path, allowed: &BTreeSet<u32>) -> Vec<HardwareNumaNode> {
    let node_root = sys_root.join("devices/system/node");
    let mut nodes = read_optional(node_root.join("online"))
        .map(|value| parse_cpu_list(value.trim()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| {
            let root = node_root.join(format!("node{id}"));
            let cpu_list = read_optional(root.join("cpulist"))?;
            let node_cpus = parse_cpu_list(cpu_list.trim());
            let visible = if allowed.is_empty() {
                node_cpus
            } else {
                node_cpus.intersection(allowed).copied().collect()
            };
            if visible.is_empty() {
                return None;
            }
            let meminfo = read_optional(root.join("meminfo"));
            Some(HardwareNumaNode {
                id,
                cpu_list: format_cpu_list(&visible),
                total_bytes: meminfo
                    .as_deref()
                    .and_then(|value| meminfo_bytes(value, &format!("Node {id} MemTotal"))),
                available_bytes: None,
            })
        })
        .collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.id);
    nodes
}

#[cfg(any(target_os = "linux", test))]
fn discover_linux_processor_topology(
    sys_root: &Path,
    allowed: &BTreeSet<u32>,
    numa_nodes: &[HardwareNumaNode],
) -> Vec<HardwareProcessor> {
    let cpu_root = sys_root.join("devices/system/cpu");
    let visible = if allowed.is_empty() {
        read_optional(cpu_root.join("online"))
            .map(|value| parse_cpu_list(value.trim()))
            .unwrap_or_default()
    } else {
        allowed.clone()
    };
    let node_cpus = numa_nodes
        .iter()
        .map(|node| (node.id, parse_cpu_list(&node.cpu_list)))
        .collect::<Vec<_>>();
    let mut processors = visible
        .iter()
        .filter_map(|logical_id| {
            let topology = cpu_root.join(format!("cpu{logical_id}")).join("topology");
            let core_id = read_optional(topology.join("core_id"))
                .and_then(|value| value.trim().parse::<u32>().ok())?;
            let socket_id = read_optional(topology.join("physical_package_id"))
                .and_then(|value| value.trim().parse::<u32>().ok())?;
            let siblings = read_optional(topology.join("thread_siblings_list"))
                .map_or_else(
                    || BTreeSet::from([*logical_id]),
                    |value| parse_cpu_list(value.trim()),
                )
                .intersection(&visible)
                .copied()
                .collect::<BTreeSet<_>>();
            let numa_node_id = node_cpus
                .iter()
                .find_map(|(node, cpus)| cpus.contains(logical_id).then_some(*node));
            Some(HardwareProcessor {
                logical_id: *logical_id,
                core_id,
                socket_id,
                numa_node_id,
                thread_siblings: format_cpu_list(&siblings),
            })
        })
        .collect::<Vec<_>>();
    processors.sort_by_key(|processor| processor.logical_id);
    processors
}

#[cfg(any(target_os = "linux", test))]
fn detect_linux_virtualization(
    cpuinfo: Option<&str>,
    kernel_release: &str,
    process_one_cgroup: Option<&str>,
    mountinfo: Option<&str>,
) -> String {
    if kernel_release.to_ascii_lowercase().contains("microsoft") {
        return "wsl".to_owned();
    }
    if process_one_cgroup.is_some_and(|value| {
        ["docker", "containerd", "kubepods", "lxc"]
            .iter()
            .any(|marker| value.contains(marker))
    }) {
        return "container".to_owned();
    }
    if mountinfo_root_filesystem(mountinfo) == Some("overlay") {
        return "container".to_owned();
    }
    if cpuinfo.is_some_and(|value| {
        value.lines().any(|line| {
            line.strip_prefix("flags")
                .or_else(|| line.strip_prefix("Features"))
                .is_some_and(|flags| {
                    flags
                        .split_ascii_whitespace()
                        .any(|flag| flag == "hypervisor")
                })
        })
    }) {
        return "hypervisor".to_owned();
    }
    if cpuinfo.is_some() {
        "none".to_owned()
    } else {
        "unknown".to_owned()
    }
}

#[cfg(any(target_os = "linux", test))]
fn mountinfo_root_filesystem(mountinfo: Option<&str>) -> Option<&str> {
    mountinfo?.lines().find_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        let mount_point = left.split_ascii_whitespace().nth(4)?;
        (mount_point == "/")
            .then(|| right.split_ascii_whitespace().next())
            .flatten()
    })
}

fn detected_instruction_sets() -> Vec<String> {
    let mut features = vec!["scalar".to_owned()];
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        for (name, available) in [
            ("sse2", std::arch::is_x86_feature_detected!("sse2")),
            ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")),
            ("avx", std::arch::is_x86_feature_detected!("avx")),
            ("avx2", std::arch::is_x86_feature_detected!("avx2")),
            ("fma", std::arch::is_x86_feature_detected!("fma")),
            ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
        ] {
            if available {
                features.push(name.to_owned());
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        for (name, available) in [
            ("neon", std::arch::is_aarch64_feature_detected!("neon")),
            ("crc", std::arch::is_aarch64_feature_detected!("crc")),
            ("sve", std::arch::is_aarch64_feature_detected!("sve")),
        ] {
            if available {
                features.push(name.to_owned());
            }
        }
    }
    features
}

#[cfg(test)]
mod tests {
    use super::{
        HardwareNumaNode, HardwareProfile, detect_linux_virtualization,
        discover_linux_processor_topology, format_cpu_list, meminfo_bytes,
        mountinfo_root_filesystem, parse_cgroup_v2_path, parse_cpu_list, parse_cpu_max,
        parse_macos_mount_line, parse_mountinfo_line, parse_windows_probe, physical_topology,
        resolve_mount, safe_cgroup_path, windows_profile_from_probe, windows_virtualization,
    };
    use std::{collections::BTreeSet, fs, path::Path};

    #[test]
    fn parses_affinity_ranges_and_cpu_quota() {
        assert_eq!(
            parse_cpu_list("0-2,8,10-11"),
            BTreeSet::from([0, 1, 2, 8, 10, 11])
        );
        assert_eq!(parse_cpu_max("9600000 100000"), Some(96_000));
        assert_eq!(parse_cpu_max("max 100000"), None);
        assert_eq!(
            format_cpu_list(&BTreeSet::from([0, 1, 2, 8, 10, 11])),
            "0-2,8,10-11"
        );
        assert_eq!(
            parse_cgroup_v2_path("0::/system.slice/hyphae.service\n"),
            Some("/system.slice/hyphae.service")
        );
        assert_eq!(
            safe_cgroup_path(Path::new("/sys/fs/cgroup"), "/../escape"),
            None
        );
    }

    #[test]
    fn limits_physical_topology_to_process_affinity() {
        let cpuinfo = "processor : 0\nphysical id : 0\ncore id : 0\n\nprocessor : 1\nphysical id : 0\ncore id : 0\n\nprocessor : 2\nphysical id : 1\ncore id : 0\n";
        assert_eq!(
            physical_topology(cpuinfo, &BTreeSet::from([0, 1])),
            (Some(1), Some(1))
        );
        assert_eq!(
            physical_topology(cpuinfo, &BTreeSet::from([0, 2])),
            (Some(2), Some(2))
        );
    }

    #[test]
    fn linux_processor_topology_preserves_core_socket_numa_and_siblings()
    -> Result<(), Box<dyn std::error::Error>> {
        let root =
            std::env::temp_dir().join(format!("hyphae-hardware-topology-{}", std::process::id()));
        for cpu in [2_u32, 6] {
            let topology = root
                .join("devices/system/cpu")
                .join(format!("cpu{cpu}/topology"));
            fs::create_dir_all(&topology)?;
            fs::write(topology.join("core_id"), "3\n")?;
            fs::write(topology.join("physical_package_id"), "1\n")?;
            fs::write(topology.join("thread_siblings_list"), "2,6\n")?;
        }
        let nodes = vec![HardwareNumaNode {
            id: 4,
            cpu_list: "2,6".to_owned(),
            total_bytes: Some(1_024),
            available_bytes: Some(512),
        }];
        let topology = discover_linux_processor_topology(&root, &BTreeSet::from([2, 6]), &nodes);
        assert_eq!(topology.len(), 2);
        assert!(topology.iter().all(|processor| {
            processor.core_id == 3
                && processor.socket_id == 1
                && processor.numa_node_id == Some(4)
                && processor.thread_siblings == "2,6"
        }));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn parses_memory_units_without_treating_missing_values_as_zero() {
        let meminfo = "MemTotal:       1024 kB\nMemAvailable:    512 kB\n";
        assert_eq!(meminfo_bytes(meminfo, "MemTotal"), Some(1_048_576));
        assert_eq!(meminfo_bytes(meminfo, "Hugepagesize"), None);
    }

    #[test]
    fn resolves_longest_mount_and_canonicalizes_options() {
        let mountinfo = "24 1 8:1 / / rw,relatime - ext4 /dev/root rw\n25 24 0:42 / /data rw,nosuid - tmpfs tmpfs rw,size=1024k\n";
        let mount = resolve_mount(mountinfo, Path::new("/data/hyphae"));
        assert_eq!(
            mount.as_ref().map(|value| value.device.as_str()),
            Some("0:42")
        );
        assert_eq!(
            mount.map(|value| value.options),
            Some(vec![
                "nosuid".to_owned(),
                "rw".to_owned(),
                "size=1024k".to_owned()
            ])
        );
        assert!(parse_mountinfo_line("invalid").is_none());
    }

    #[test]
    fn parses_macos_mount_without_shell_tokenization() {
        let mount =
            parse_macos_mount_line("/dev/disk3s1 on /Volumes/Native Data (apfs, local, journaled)");
        assert_eq!(
            mount.map(|value| (
                value.device,
                value.mount_point,
                value.filesystem,
                value.options
            )),
            Some((
                "/dev/disk3s1".to_owned(),
                Path::new("/Volumes/Native Data").to_path_buf(),
                "apfs".to_owned(),
                vec!["journaled".to_owned(), "local".to_owned()]
            ))
        );
    }

    #[test]
    fn virtualization_detection_is_explicit_and_fail_closed() {
        assert_eq!(
            detect_linux_virtualization(Some("flags : fpu hypervisor"), "linux", None, None),
            "hypervisor"
        );
        assert_eq!(
            detect_linux_virtualization(None, "linux", None, None),
            "unknown"
        );
        assert_eq!(
            detect_linux_virtualization(
                Some("flags : fpu"),
                "linux",
                None,
                Some("1 0 0:1 / / rw - overlay overlay rw")
            ),
            "container"
        );
        assert_eq!(
            detect_linux_virtualization(Some("flags : fpu"), "linux", None, None),
            "none"
        );
        assert_eq!(
            mountinfo_root_filesystem(Some("1 0 0:1 / / rw - ext4 /dev/root rw")),
            Some("ext4")
        );
    }

    #[test]
    fn parses_windows_cim_probe_without_shell_interpolation()
    -> Result<(), Box<dyn std::error::Error>> {
        let encoded = r#"{
          "processors":[{"cores":24,"logical":48},{"cores":24,"logical":48}],
          "totalMemoryBytes":824633720832,
          "availableMemoryBytes":700000000000,
          "pageSizeBytes":4096,
          "kernelRelease":"10.0.26100",
          "manufacturer":"Microsoft Corporation",
          "model":"Virtual Machine",
          "disk":{"device":"C:","filesystem":"NTFS"}
        }"#;
        let probe = parse_windows_probe(encoded).ok_or("fixture must parse")?;
        let (cpu, memory, storage, operating_system) =
            windows_profile_from_probe(Path::new("C:\\data"), 96, Some(&probe));
        assert_eq!(cpu.physical_cores_visible, Some(48));
        assert_eq!(cpu.smt_threads_per_core, Some(2));
        assert_eq!(cpu.sockets_visible, Some(2));
        assert_eq!(memory.total_bytes, Some(824_633_720_832));
        assert_eq!(memory.available_bytes, Some(700_000_000_000));
        assert_eq!(storage.filesystem.as_deref(), Some("NTFS"));
        assert_eq!(storage.device.as_deref(), Some("C:"));
        assert_eq!(operating_system.virtualization, "hypervisor");
        assert_eq!(
            operating_system.local_transports,
            vec!["embedded".to_owned(), "named-pipe".to_owned()]
        );
        Ok(())
    }

    #[test]
    fn windows_virtualization_remains_explicit_when_probe_is_absent() {
        assert_eq!(windows_virtualization(None, None), "unknown");
        assert_eq!(
            windows_virtualization(Some("Dell Inc."), Some("PowerEdge")),
            "none"
        );
    }

    #[test]
    fn current_process_discovery_has_a_stable_identity() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let first = HardwareProfile::discover(manifest)?;
        let second = HardwareProfile::discover(manifest.join("src"))?;
        assert_eq!(first.schema, "hyphae-native-hardware-profile-v1");
        assert_eq!(first.fingerprint.len(), 64);
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_ne!(first.storage.path, second.storage.path);
        assert!(first.cpu.logical_processors_available > 0);
        assert!(!first.cpu.instruction_sets.is_empty());
        Ok(())
    }

    #[test]
    fn serialized_profile_round_trips_and_rejects_stable_field_tampering()
    -> Result<(), Box<dyn std::error::Error>> {
        let profile = HardwareProfile::discover(env!("CARGO_MANIFEST_DIR"))?;
        let encoded = serde_json::to_vec(&profile)?;
        assert_eq!(HardwareProfile::from_json_slice(&encoded)?, profile);

        let mut tampered = serde_json::to_value(&profile)?;
        tampered["cpu"]["architecture"] = serde_json::Value::String("tampered".to_owned());
        let tampered = serde_json::to_vec(&tampered)?;
        assert!(HardwareProfile::from_json_slice(&tampered).is_err());
        Ok(())
    }
}
