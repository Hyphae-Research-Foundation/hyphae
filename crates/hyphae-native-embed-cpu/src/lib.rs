// SPDX-License-Identifier: Apache-2.0

//! Optional, bounded, in-process Qwen3 CPU and CUDA execution for Hyphae Native.
//!
//! This crate has no acquisition or listener surface. A caller opens a local
//! manifest and snapshot into [`Qwen3ArtifactDescriptors`], then explicitly
//! registers the verified model with [`Qwen3CpuExecutor`].

#![allow(clippy::result_large_err)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path},
    sync::{Arc, RwLock},
};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::qwen3::{Config as Qwen3Config, Model as Qwen3Model};
#[cfg(feature = "cuda")]
use cudarc::driver::{CudaContext, sys::CUdevice_attribute as CudaAttribute};
use hyphae_native_catalog::{
    EmbeddingPipelineVersion, EmbeddingProfileDefinition, MAX_EMBEDDING_ARTIFACT_MANIFEST_BYTES,
    QWEN3_EMBEDDING_L2_EPSILON, QWEN3_EMBEDDING_NATIVE_DIMENSION,
    QWEN3_EMBEDDING_OUTPUT_DIMENSIONS, QWEN3_EMBEDDING_QUERY_INSTRUCTION,
};
use hyphae_native_product::{
    NativeProduct, ProductEmbeddingBackend, ProductEmbeddingBatchOutput,
    ProductEmbeddingExecutionProfile, ProductEmbeddingExecutor, ProductEmbeddingExecutorRequest,
    ProductEmbeddingPrecision, ProductError, ProductErrorCode,
    ProductLocalEmbeddingExecutionProfile, ProductVector,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokenizers::Tokenizer;

const MANIFEST_SCHEMA: &str = "hyphae-embedding-model-manifest-v1";
const MANIFEST_STATUS: &str = "verified";
const MODEL_REPOSITORY: &str = "Qwen/Qwen3-Embedding-0.6B";
const MODEL_TYPE: &str = "qwen3";
const MODEL_PARAMETER_CLASS: &str = "0.6B";
const WEIGHTS_PATH: &str = "model.safetensors";
const TOKENIZER_PATH: &str = "tokenizer.json";
const CONFIG_PATH: &str = "config.json";
const MAX_ARTIFACT_PATH_BYTES: usize = 1_024;
const MAX_ARTIFACT_DIRECTORY_DEPTH: usize = 8;
const QWEN3_LAYERS: usize = 28;
const QWEN3_ATTENTION_HEADS: usize = 16;
const QWEN3_KV_HEADS: usize = 8;
const QWEN3_HEAD_DIMENSION: usize = 128;
const QWEN3_INTERMEDIATE_DIMENSION: usize = 3_072;
const QWEN3_VOCABULARY: usize = 151_669;
const QWEN3_MAX_POSITIONS: usize = 32_768;
const QWEN3_END_OF_TEXT_TOKEN_ID: u32 = 151_643;
const QWEN3_PAD_TOKEN: &str = "<|endoftext|>";
const CANDLE_VERSION: &str = "0.9.2";
#[cfg(feature = "cuda")]
const VALIDATED_H100_NAME: &str = "NVIDIA H100 80GB HBM3";
#[cfg(feature = "cuda")]
const VALIDATED_H100_MINIMUM_MEMORY_BYTES: usize = 79 * 1024 * 1024 * 1024;

/// Fixed resource ceilings for model loading and CPU execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Qwen3CpuLimits {
    /// Maximum models retained by one process-local registry.
    pub max_models: usize,
    /// Maximum files in one complete model snapshot.
    pub max_snapshot_files: usize,
    /// Maximum aggregate bytes named by one artifact manifest.
    pub max_artifact_bytes: u64,
    /// Maximum conservative transient bytes admitted while loading a model.
    pub max_load_memory_bytes: u64,
    /// Maximum aggregate left-padded token positions in one request.
    pub max_batch_padded_tokens: usize,
    /// Maximum elements in one model tensor created by execution.
    pub max_tensor_elements: usize,
    /// Maximum conservative transient execution bytes, excluding model weights.
    pub max_execution_memory_bytes: u64,
    /// Maximum layer-token logical work in one request.
    pub max_layer_token_work: u64,
    /// Maximum attention score logical work in one request.
    pub max_attention_work: u64,
    /// Maximum token positions evaluated between cancellation checkpoints.
    pub checkpoint_chunk_tokens: usize,
}

impl Default for Qwen3CpuLimits {
    fn default() -> Self {
        Self {
            max_models: 4,
            max_snapshot_files: 64,
            max_artifact_bytes: 2 * 1024 * 1024 * 1024,
            max_load_memory_bytes: 8 * 1024 * 1024 * 1024,
            max_batch_padded_tokens: 1_000_000,
            max_tensor_elements: 128 * 1024 * 1024,
            max_execution_memory_bytes: 10 * 1024 * 1024 * 1024,
            max_layer_token_work: 28_000_000,
            max_attention_work: 300_000_000_000,
            checkpoint_chunk_tokens: 32,
        }
    }
}

impl Qwen3CpuLimits {
    /// Validates that every configured ceiling is finite and usable.
    ///
    /// # Errors
    ///
    /// Returns [`Qwen3CpuError::InvalidLimits`] for a zero or structurally
    /// impossible limit.
    pub fn validate(self) -> Result<(), Qwen3CpuError> {
        if self.max_models == 0
            || self.max_snapshot_files == 0
            || self.max_snapshot_files > 1_024
            || self.max_artifact_bytes == 0
            || self.max_load_memory_bytes == 0
            || self.max_batch_padded_tokens == 0
            || self.max_tensor_elements == 0
            || self.max_execution_memory_bytes == 0
            || self.max_layer_token_work == 0
            || self.max_attention_work == 0
            || self.checkpoint_chunk_tokens == 0
            || self.checkpoint_chunk_tokens > QWEN3_MAX_POSITIONS
        {
            return Err(Qwen3CpuError::InvalidLimits);
        }
        Ok(())
    }
}

/// Failure while opening, verifying, loading, or registering a CPU model.
#[derive(Debug, Error)]
pub enum Qwen3CpuError {
    /// A configured load or execution ceiling is invalid.
    #[error("invalid Qwen3 CPU resource limits")]
    InvalidLimits,
    /// A descriptor could not be opened or read.
    #[error("artifact descriptor I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Manifest JSON is malformed or outside the closed schema.
    #[error("artifact manifest is invalid: {0}")]
    Manifest(&'static str),
    /// Manifest JSON could not be decoded.
    #[error("artifact manifest JSON failed to decode: {0}")]
    ManifestJson(#[from] serde_json::Error),
    /// A snapshot descriptor does not match its manifest entry.
    #[error("artifact snapshot is invalid: {0}")]
    Artifact(&'static str),
    /// The tokenizer cannot implement the closed pipeline.
    #[error("Qwen3 tokenizer is invalid: {0}")]
    Tokenizer(String),
    /// The model configuration or safetensors cannot be loaded.
    #[error("Qwen3 model is invalid: {0}")]
    Model(String),
    /// The registry is full or already contains this profile.
    #[error("Qwen3 CPU registry rejected the model: {0}")]
    Registry(&'static str),
}

/// A complete set of already-open descriptors for one manifest and snapshot.
///
/// Loading seeks and reads these descriptors directly. It never reopens an
/// artifact path between hashing, parsing, and tensor construction.
pub struct Qwen3ArtifactDescriptors {
    manifest: File,
    files: BTreeMap<String, File>,
}

impl std::fmt::Debug for Qwen3ArtifactDescriptors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Qwen3ArtifactDescriptors")
            .field("snapshot_files", &self.files.len())
            .finish_non_exhaustive()
    }
}

impl Qwen3ArtifactDescriptors {
    /// Opens a manifest and every leaf in one local snapshot exactly once.
    ///
    /// Symlinked snapshot files are allowed, as in a content-addressed model
    /// cache, but loading remains bound to the opened target descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failure, invalid names, directory symlinks,
    /// excessive depth, or too many snapshot files.
    pub fn open(
        manifest_path: impl AsRef<Path>,
        snapshot_root: impl AsRef<Path>,
        limits: Qwen3CpuLimits,
    ) -> Result<Self, Qwen3CpuError> {
        limits.validate()?;
        let manifest = File::open(manifest_path)?;
        let root = snapshot_root.as_ref();
        if !root.metadata()?.is_dir() {
            return Err(Qwen3CpuError::Artifact("snapshot root is not a directory"));
        }
        let mut files = BTreeMap::new();
        open_snapshot_directory(root, Path::new(""), 0, limits, &mut files)?;
        if files.is_empty() {
            return Err(Qwen3CpuError::Artifact("snapshot has no files"));
        }
        Ok(Self { manifest, files })
    }

    /// Builds a descriptor set from caller-opened files.
    ///
    /// Keys are slash-separated manifest-relative paths. This is the strictest
    /// loading surface for callers that already hold capability descriptors.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or duplicate-normalized path.
    pub fn from_open_files(
        manifest: File,
        files: BTreeMap<String, File>,
    ) -> Result<Self, Qwen3CpuError> {
        if files.is_empty() || files.keys().any(|path| !valid_artifact_path(path)) {
            return Err(Qwen3CpuError::Artifact("invalid descriptor path set"));
        }
        Ok(Self { manifest, files })
    }

    /// Returns the number of opened snapshot file descriptors.
    pub fn snapshot_file_count(&self) -> usize {
        self.files.len()
    }

    #[cfg(feature = "cuda")]
    fn try_clone(&self) -> Result<Self, Qwen3CpuError> {
        let files = self
            .files
            .iter()
            .map(|(path, descriptor)| Ok((path.clone(), descriptor.try_clone()?)))
            .collect::<Result<BTreeMap<_, _>, std::io::Error>>()?;
        Ok(Self {
            manifest: self.manifest.try_clone()?,
            files,
        })
    }
}

fn open_snapshot_directory(
    root: &Path,
    relative: &Path,
    depth: usize,
    limits: Qwen3CpuLimits,
    files: &mut BTreeMap<String, File>,
) -> Result<(), Qwen3CpuError> {
    if depth > MAX_ARTIFACT_DIRECTORY_DEPTH {
        return Err(Qwen3CpuError::Artifact("snapshot directory is too deep"));
    }
    let directory = root.join(relative);
    for entry in directory.read_dir()? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Qwen3CpuError::Artifact("artifact path is not UTF-8"))?;
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            return Err(Qwen3CpuError::Artifact(
                "artifact path component is invalid",
            ));
        }
        let child = relative.join(name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            open_snapshot_directory(root, &child, depth + 1, limits, files)?;
            continue;
        }
        let metadata = std::fs::metadata(entry.path())?;
        if !metadata.is_file() {
            return Err(Qwen3CpuError::Artifact(
                "snapshot leaf is not a regular file",
            ));
        }
        let path = manifest_path(&child)?;
        let descriptor = File::open(entry.path())?;
        if descriptor.metadata()?.len() != metadata.len() {
            return Err(Qwen3CpuError::Artifact(
                "artifact changed while it was opened",
            ));
        }
        if files.insert(path, descriptor).is_some() {
            return Err(Qwen3CpuError::Artifact("duplicate snapshot path"));
        }
        if files.len() > limits.max_snapshot_files {
            return Err(Qwen3CpuError::Artifact(
                "snapshot file count exceeds its limit",
            ));
        }
    }
    Ok(())
}

fn manifest_path(path: &Path) -> Result<String, Qwen3CpuError> {
    let mut output = String::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(Qwen3CpuError::Artifact(
                "artifact path escapes the snapshot",
            ));
        };
        let component = component
            .to_str()
            .ok_or(Qwen3CpuError::Artifact("artifact path is not UTF-8"))?;
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(component);
    }
    if !valid_artifact_path(&output) {
        return Err(Qwen3CpuError::Artifact("artifact path is invalid"));
    }
    Ok(output)
}

fn valid_artifact_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_ARTIFACT_PATH_BYTES
        && !path.starts_with('/')
        && !path.ends_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactManifest {
    #[serde(rename = "$comment", default)]
    _comment: Option<String>,
    schema: String,
    status: String,
    model: ManifestModel,
    #[serde(rename = "license")]
    _license: serde_json::Value,
    runtime_policy: ManifestRuntimePolicy,
    #[serde(rename = "acquisition_provenance")]
    _acquisition_provenance: serde_json::Value,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestModel {
    repository: String,
    revision: String,
    model_type: String,
    parameter_class: String,
    native_dimensions: u16,
    supported_output_dimensions: Vec<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRuntimePolicy {
    automatic_network: bool,
    local_files_only: bool,
    trust_remote_code: bool,
    weights_format: String,
    canonical_output_dtype: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    role: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct ModelConfigIdentity {
    architectures: Vec<String>,
    attention_dropout: f64,
    bos_token_id: u32,
    eos_token_id: u32,
    model_type: String,
    rope_scaling: serde_json::Value,
    torch_dtype: String,
    use_cache: bool,
}

impl ArtifactManifest {
    fn validate(&self, limits: Qwen3CpuLimits) -> Result<(), Qwen3CpuError> {
        if self.schema != MANIFEST_SCHEMA || self.status != MANIFEST_STATUS {
            return Err(Qwen3CpuError::Manifest("schema or status differs"));
        }
        if self.model.repository != MODEL_REPOSITORY
            || self.model.model_type != MODEL_TYPE
            || self.model.parameter_class != MODEL_PARAMETER_CLASS
            || self.model.native_dimensions != QWEN3_EMBEDDING_NATIVE_DIMENSION
            || self.model.supported_output_dimensions != QWEN3_EMBEDDING_OUTPUT_DIMENSIONS
            || !valid_revision(&self.model.revision)
        {
            return Err(Qwen3CpuError::Manifest("model identity differs"));
        }
        if self.runtime_policy.automatic_network
            || !self.runtime_policy.local_files_only
            || self.runtime_policy.trust_remote_code
            || self.runtime_policy.weights_format != "safetensors-only"
            || self.runtime_policy.canonical_output_dtype != "float32"
        {
            return Err(Qwen3CpuError::Manifest("runtime policy differs"));
        }
        if self.files.is_empty() || self.files.len() > limits.max_snapshot_files {
            return Err(Qwen3CpuError::Manifest("file count exceeds its limit"));
        }
        let mut paths = BTreeSet::new();
        let mut total = 0_u64;
        let mut weights = 0_usize;
        let mut tokenizer = 0_usize;
        let mut config = 0_usize;
        for file in &self.files {
            if !valid_artifact_path(&file.path)
                || file.role.is_empty()
                || file.size_bytes == 0
                || decode_sha256(&file.sha256).is_none()
                || !paths.insert(file.path.as_str())
            {
                return Err(Qwen3CpuError::Manifest(
                    "file entry is invalid or duplicate",
                ));
            }
            total = total
                .checked_add(file.size_bytes)
                .ok_or(Qwen3CpuError::Manifest("artifact byte total overflowed"))?;
            if file.role == "weights" {
                weights += 1;
                if file.path != WEIGHTS_PATH {
                    return Err(Qwen3CpuError::Manifest("weights path is not canonical"));
                }
            }
            tokenizer += usize::from(file.path == TOKENIZER_PATH);
            config += usize::from(file.path == CONFIG_PATH);
        }
        if total > limits.max_artifact_bytes || weights != 1 || tokenizer != 1 || config != 1 {
            return Err(Qwen3CpuError::Manifest("required artifact set differs"));
        }
        Ok(())
    }
}

fn valid_revision(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_sha256(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        output[index] = high << 4 | low;
    }
    Some(output)
}

const fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ModelKey {
    digest: [u8; 32],
    manifest_bytes: u64,
}

#[derive(Clone, Debug)]
struct ExecutionBackend {
    backend: &'static str,
    device: Device,
    device_profile: String,
    compute_dtype: DType,
    compute_dtype_profile: &'static str,
    driver_profile: String,
    runtime_profile: String,
}

impl ExecutionBackend {
    fn cpu() -> Self {
        Self {
            backend: "candle-qwen3",
            device: Device::Cpu,
            device_profile: "cpu".to_owned(),
            compute_dtype: DType::F32,
            compute_dtype_profile: "float32",
            driver_profile: "not-applicable".to_owned(),
            runtime_profile: format!("candle/{CANDLE_VERSION}"),
        }
    }
}

/// CUDA model-compute dtype accepted by the validated H100 executor.
#[cfg(feature = "cuda")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Qwen3CudaComputeDType {
    /// H100 bfloat16 model compute with canonical float32 output.
    #[default]
    BFloat16,
    /// H100 IEEE float16 model compute with canonical float32 output.
    Float16,
}

#[cfg(feature = "cuda")]
impl Qwen3CudaComputeDType {
    const fn candle(self) -> DType {
        match self {
            Self::BFloat16 => DType::BF16,
            Self::Float16 => DType::F16,
        }
    }

    const fn profile(self) -> &'static str {
        match self {
            Self::BFloat16 => "bfloat16-f32-output+whole-batch-cpu-f32-fallback",
            Self::Float16 => "float16-f32-output+whole-batch-cpu-f32-fallback",
        }
    }
}

#[cfg(feature = "cuda")]
fn select_validated_h100(compute_dtype: Qwen3CudaComputeDType) -> Option<ExecutionBackend> {
    let driver_api = cudarc::runtime::result::version::get_driver_version().ok()?;
    let runtime_api = cudarc::runtime::result::version::get_runtime_version().ok()?;
    let driver_profile = nvidia_driver_release().map_or_else(
        || format!("cuda-driver-api-{driver_api}"),
        |release| format!("nvidia-{release};cuda-driver-api-{driver_api}"),
    );
    let count = usize::try_from(CudaContext::device_count().ok()?).ok()?;
    for ordinal in 0..count {
        let Ok(context) = CudaContext::new(ordinal) else {
            continue;
        };
        let (Ok(name), Ok(capability), Ok(memory)) = (
            context.name(),
            context.compute_capability(),
            context.total_mem(),
        ) else {
            continue;
        };
        if name != VALIDATED_H100_NAME
            || capability != (9, 0)
            || memory < VALIDATED_H100_MINIMUM_MEMORY_BYTES
        {
            continue;
        }
        let (Ok(uuid), Ok(domain), Ok(bus), Ok(device_id), Ok(device)) = (
            context.uuid(),
            context.attribute(CudaAttribute::CU_DEVICE_ATTRIBUTE_PCI_DOMAIN_ID),
            context.attribute(CudaAttribute::CU_DEVICE_ATTRIBUTE_PCI_BUS_ID),
            context.attribute(CudaAttribute::CU_DEVICE_ATTRIBUTE_PCI_DEVICE_ID),
            Device::new_cuda(ordinal),
        ) else {
            continue;
        };
        let uuid = format_cuda_uuid(uuid.bytes);
        return Some(ExecutionBackend {
            backend: "candle-qwen3-cuda",
            device,
            device_profile: format!(
                "cuda:{ordinal};{name};sm90;{uuid};{domain:08x}:{bus:02x}:{device_id:02x}.0"
            ),
            compute_dtype: compute_dtype.candle(),
            compute_dtype_profile: compute_dtype.profile(),
            driver_profile,
            runtime_profile: format!("cuda-runtime-api-{runtime_api};candle/{CANDLE_VERSION}"),
        });
    }
    None
}

#[cfg(feature = "cuda")]
fn nvidia_driver_release() -> Option<String> {
    let version = std::fs::read_to_string("/proc/driver/nvidia/version").ok()?;
    version
        .lines()
        .next()?
        .split_ascii_whitespace()
        .find_map(|part| {
            let components: Vec<_> = part.split('.').collect();
            (components.len() == 3
                && components
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())))
            .then(|| part.to_owned())
        })
}

#[cfg(feature = "cuda")]
fn format_cuda_uuid(bytes: [std::ffi::c_char; 16]) -> String {
    let bytes = bytes.map(i8::cast_unsigned);
    format!(
        "GPU-{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

#[derive(Debug)]
struct LoadedModel {
    key: ModelKey,
    revision: String,
    tokenizer: Tokenizer,
    pad_token: u32,
    model: Qwen3Model,
    backend: ExecutionBackend,
}

#[cfg(feature = "cuda")]
#[derive(Clone, Debug)]
struct AcceleratorModel {
    primary: Arc<LoadedModel>,
    cpu_fallback: Option<Arc<LoadedModel>>,
}

impl LoadedModel {
    fn execution_profile(
        &self,
        checkpoint_chunk_tokens: usize,
    ) -> Result<ProductLocalEmbeddingExecutionProfile, ProductError> {
        ProductLocalEmbeddingExecutionProfile::new(
            self.backend.backend,
            CANDLE_VERSION,
            &self.backend.device_profile,
            self.backend.compute_dtype_profile,
            env!("HYPHAE_BUILD_TARGET"),
            &self.revision,
            self.key.digest,
            u32::try_from(checkpoint_chunk_tokens)
                .map_err(|_| product_error(ProductErrorCode::LimitExceeded))?,
        )
    }
}

impl LoadedModel {
    fn product_execution_profile(
        &self,
        embedding_profile: hyphae_native_product::ObjectId,
        checkpoint_chunk_tokens: usize,
    ) -> Result<ProductEmbeddingExecutionProfile, ProductError> {
        let local = self.execution_profile(checkpoint_chunk_tokens)?;
        let mut digest = String::with_capacity(64);
        for byte in local.artifact_manifest_digest() {
            use std::fmt::Write as _;
            write!(digest, "{byte:02x}")
                .map_err(|_| product_error(ProductErrorCode::Unavailable))?;
        }
        let cuda = self.backend.device.is_cuda();
        let backend = if cuda {
            ProductEmbeddingBackend::Cuda
        } else {
            ProductEmbeddingBackend::Cpu
        };
        let device = if cuda {
            local.device().to_owned()
        } else {
            format!("cpu:{}", local.target())
        };
        Ok(ProductEmbeddingExecutionProfile {
            embedding_profile,
            backend,
            device,
            driver: self.backend.driver_profile.clone(),
            runtime: format!(
                "{};model={};manifest={digest};compute={};output=f32;checkpoint_tokens={}",
                self.backend.runtime_profile,
                local.model_revision(),
                local.compute_dtype(),
                local.checkpoint_chunk_tokens(),
            ),
            precision: ProductEmbeddingPrecision::F32,
            kernels: vec![
                format!("qwen3-forward-{}", local.compute_dtype()),
                "last-token-pooling-f32".to_owned(),
                "leading-dimension-projection-f32".to_owned(),
                "l2-normalization-f32".to_owned(),
            ],
            fallback: false,
        })
    }
}

/// Digest-keyed process-local registry and product embedding executor.
#[derive(Debug)]
pub struct Qwen3CpuExecutor {
    limits: Qwen3CpuLimits,
    models: RwLock<BTreeMap<ModelKey, Arc<LoadedModel>>>,
}

impl Qwen3CpuExecutor {
    /// Creates an empty registry with explicit hard resource ceilings.
    ///
    /// # Errors
    ///
    /// Returns an error when a ceiling is zero or structurally invalid.
    pub fn new(limits: Qwen3CpuLimits) -> Result<Self, Qwen3CpuError> {
        limits.validate()?;
        Ok(Self {
            limits,
            models: RwLock::new(BTreeMap::new()),
        })
    }

    /// Verifies and loads one descriptor-bound model, then registers it.
    ///
    /// Model construction happens before the registry write lock is acquired.
    /// A failed load leaves the registry unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error for manifest, artifact, tokenizer, model, memory, or
    /// registry-limit failure.
    pub fn load_and_register(
        &self,
        descriptors: Qwen3ArtifactDescriptors,
    ) -> Result<ProductLocalEmbeddingExecutionProfile, Qwen3CpuError> {
        let loaded = Arc::new(load_model(
            descriptors,
            self.limits,
            ExecutionBackend::cpu(),
        )?);
        let profile = loaded
            .execution_profile(self.limits.checkpoint_chunk_tokens)
            .map_err(|_| Qwen3CpuError::Model("execution profile is invalid".to_owned()))?;
        let mut models = self
            .models
            .write()
            .map_err(|_| Qwen3CpuError::Registry("registry lock is poisoned"))?;
        if models.contains_key(&loaded.key) {
            return Err(Qwen3CpuError::Registry("profile is already loaded"));
        }
        if models.len() >= self.limits.max_models {
            return Err(Qwen3CpuError::Registry("model count exceeds its limit"));
        }
        models.insert(loaded.key, loaded);
        Ok(profile)
    }

    /// Installs this registry as a product handle's process-local executor.
    pub fn install(self: &Arc<Self>, product: &mut NativeProduct) {
        product.set_embedding_executor(self.clone());
    }

    /// Returns the number of currently loaded immutable models.
    pub fn loaded_models(&self) -> usize {
        self.models.read().map_or(0, |models| models.len())
    }

    /// Reports the exact CPU profile for a loaded manifest identity.
    ///
    /// # Errors
    ///
    /// Returns an error only if a process-local lock is poisoned or the
    /// profile fields cannot satisfy product bounds.
    pub fn execution_profile_for(
        &self,
        manifest_digest: [u8; 32],
        manifest_bytes: u64,
    ) -> Result<Option<ProductLocalEmbeddingExecutionProfile>, ProductError> {
        let models = self
            .models
            .read()
            .map_err(|_| product_error(ProductErrorCode::Unavailable))?;
        models
            .get(&ModelKey {
                digest: manifest_digest,
                manifest_bytes,
            })
            .map(|model| model.execution_profile(self.limits.checkpoint_chunk_tokens))
            .transpose()
    }
}

/// Digest-keyed executor that selects a validated H100 or a complete CPU path.
///
/// Backend selection is fixed when the registry is created. A registry that
/// selects CUDA preloads a CPU model and retries an unavailable CUDA result as
/// one complete CPU batch. Cancellation, deadlines, and limit failures never
/// trigger fallback. If no validated H100 is available, the whole registry
/// uses the CPU execution profile instead.
#[cfg(feature = "cuda")]
#[derive(Debug)]
pub struct Qwen3AcceleratorExecutor {
    limits: Qwen3CpuLimits,
    backend: ExecutionBackend,
    models: RwLock<BTreeMap<ModelKey, AcceleratorModel>>,
}

#[cfg(feature = "cuda")]
impl Qwen3AcceleratorExecutor {
    /// Creates a registry that prefers validated H100 bfloat16 execution.
    ///
    /// # Errors
    ///
    /// Returns an error when a resource ceiling is invalid. Missing,
    /// incompatible, or unusable CUDA devices select the complete CPU path.
    pub fn new(limits: Qwen3CpuLimits) -> Result<Self, Qwen3CpuError> {
        Self::new_with_compute_dtype(limits, Qwen3CudaComputeDType::BFloat16)
    }

    /// Creates a registry with an explicit H100 model-compute dtype.
    ///
    /// # Errors
    ///
    /// Returns an error when a resource ceiling is invalid. Missing,
    /// incompatible, or unusable CUDA devices select the complete CPU path.
    pub fn new_with_compute_dtype(
        limits: Qwen3CpuLimits,
        compute_dtype: Qwen3CudaComputeDType,
    ) -> Result<Self, Qwen3CpuError> {
        limits.validate()?;
        Ok(Self {
            limits,
            backend: select_validated_h100(compute_dtype).unwrap_or_else(ExecutionBackend::cpu),
            models: RwLock::new(BTreeMap::new()),
        })
    }

    /// Returns whether this fixed registry selected validated H100 execution.
    pub fn uses_cuda(&self) -> bool {
        self.backend.device.is_cuda()
    }

    /// Returns the exact fixed device identity used by this registry.
    pub fn device_profile(&self) -> &str {
        &self.backend.device_profile
    }

    /// Verifies, loads, and atomically registers one descriptor-bound model.
    ///
    /// # Errors
    ///
    /// Returns an error for manifest, artifact, tokenizer, model, memory, or
    /// registry-limit failure. A failed load leaves the registry unchanged.
    pub fn load_and_register(
        &self,
        descriptors: Qwen3ArtifactDescriptors,
    ) -> Result<ProductLocalEmbeddingExecutionProfile, Qwen3CpuError> {
        let cpu_descriptors = if self.backend.device.is_cuda() {
            Some(descriptors.try_clone()?)
        } else {
            None
        };
        let loaded = Arc::new(load_model(descriptors, self.limits, self.backend.clone())?);
        let cpu_fallback = cpu_descriptors
            .map(|descriptors| load_model(descriptors, self.limits, ExecutionBackend::cpu()))
            .transpose()?
            .map(Arc::new);
        if cpu_fallback
            .as_ref()
            .is_some_and(|fallback| fallback.key != loaded.key)
        {
            return Err(Qwen3CpuError::Model(
                "CPU fallback model identity differs".to_owned(),
            ));
        }
        let profile = loaded
            .execution_profile(self.limits.checkpoint_chunk_tokens)
            .map_err(|_| Qwen3CpuError::Model("execution profile is invalid".to_owned()))?;
        let mut models = self
            .models
            .write()
            .map_err(|_| Qwen3CpuError::Registry("registry lock is poisoned"))?;
        if models.contains_key(&loaded.key) {
            return Err(Qwen3CpuError::Registry("profile is already loaded"));
        }
        if models.len() >= self.limits.max_models {
            return Err(Qwen3CpuError::Registry("model count exceeds its limit"));
        }
        models.insert(
            loaded.key,
            AcceleratorModel {
                primary: loaded,
                cpu_fallback,
            },
        );
        Ok(profile)
    }

    /// Installs this registry as a product handle's process-local executor.
    pub fn install(self: &Arc<Self>, product: &mut NativeProduct) {
        product.set_embedding_executor(self.clone());
    }

    /// Returns the number of currently loaded immutable models.
    pub fn loaded_models(&self) -> usize {
        self.models.read().map_or(0, |models| models.len())
    }

    /// Reports the exact fixed profile for a loaded manifest identity.
    ///
    /// # Errors
    ///
    /// Returns an error only if a process-local lock is poisoned or the
    /// profile fields cannot satisfy product bounds.
    pub fn execution_profile_for(
        &self,
        manifest_digest: [u8; 32],
        manifest_bytes: u64,
    ) -> Result<Option<ProductLocalEmbeddingExecutionProfile>, ProductError> {
        let models = self
            .models
            .read()
            .map_err(|_| product_error(ProductErrorCode::Unavailable))?;
        models
            .get(&ModelKey {
                digest: manifest_digest,
                manifest_bytes,
            })
            .map(|model| {
                model
                    .primary
                    .execution_profile(self.limits.checkpoint_chunk_tokens)
            })
            .transpose()
    }
}

impl ProductEmbeddingExecutor for Qwen3CpuExecutor {
    fn embed_passages(
        &self,
        request: ProductEmbeddingExecutorRequest<'_>,
        checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
    ) -> Result<ProductEmbeddingBatchOutput, ProductError> {
        embed_with_registry(&self.models, self.limits, request, checkpoint)
    }

    fn execution_profile(
        &self,
        profile: &EmbeddingProfileDefinition,
    ) -> Result<Option<ProductLocalEmbeddingExecutionProfile>, ProductError> {
        self.execution_profile_for(
            *profile.artifact_manifest_digest.as_bytes(),
            profile.artifact_manifest_byte_length,
        )
    }
}

#[cfg(feature = "cuda")]
impl ProductEmbeddingExecutor for Qwen3AcceleratorExecutor {
    fn embed_passages(
        &self,
        request: ProductEmbeddingExecutorRequest<'_>,
        checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
    ) -> Result<ProductEmbeddingBatchOutput, ProductError> {
        validate_product_request(request)?;
        let key = ModelKey {
            digest: *request.profile.artifact_manifest_digest.as_bytes(),
            manifest_bytes: request.profile.artifact_manifest_byte_length,
        };
        let model = self
            .models
            .read()
            .map_err(|_| product_error(ProductErrorCode::Unavailable))?
            .get(&key)
            .cloned()
            .ok_or_else(|| product_error(ProductErrorCode::Unavailable))?;
        let primary = embed_with_model(&model.primary, self.limits, request, checkpoint);
        if let Some(cpu_fallback) = model.cpu_fallback.as_ref() {
            whole_batch_cpu_fallback(primary, checkpoint, |checkpoint| {
                let mut output = embed_with_model(cpu_fallback, self.limits, request, checkpoint)?;
                output.execution_profile.fallback = true;
                Ok(output)
            })
        } else {
            primary
        }
    }

    fn execution_profile(
        &self,
        profile: &EmbeddingProfileDefinition,
    ) -> Result<Option<ProductLocalEmbeddingExecutionProfile>, ProductError> {
        self.execution_profile_for(
            *profile.artifact_manifest_digest.as_bytes(),
            profile.artifact_manifest_byte_length,
        )
    }
}

#[cfg(feature = "cuda")]
fn whole_batch_cpu_fallback<T>(
    primary: Result<T, ProductError>,
    checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
    fallback: impl FnOnce(&mut dyn FnMut() -> Result<(), ProductError>) -> Result<T, ProductError>,
) -> Result<T, ProductError> {
    match primary {
        Err(error) if error.code() == ProductErrorCode::Unavailable => {
            checkpoint()?;
            fallback(checkpoint)
        }
        result => result,
    }
}

fn embed_with_registry(
    models: &RwLock<BTreeMap<ModelKey, Arc<LoadedModel>>>,
    limits: Qwen3CpuLimits,
    request: ProductEmbeddingExecutorRequest<'_>,
    checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
) -> Result<ProductEmbeddingBatchOutput, ProductError> {
    validate_product_request(request)?;
    let key = ModelKey {
        digest: *request.profile.artifact_manifest_digest.as_bytes(),
        manifest_bytes: request.profile.artifact_manifest_byte_length,
    };
    let model = models
        .read()
        .map_err(|_| product_error(ProductErrorCode::Unavailable))?
        .get(&key)
        .cloned()
        .ok_or_else(|| product_error(ProductErrorCode::Unavailable))?;
    embed_with_model(&model, limits, request, checkpoint)
}

fn embed_with_model(
    model: &LoadedModel,
    limits: Qwen3CpuLimits,
    request: ProductEmbeddingExecutorRequest<'_>,
    checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
) -> Result<ProductEmbeddingBatchOutput, ProductError> {
    checkpoint()?;
    let inputs: Vec<&str> = request
        .documents
        .iter()
        .map(|document| document.text.as_str())
        .collect();
    let tokenized = tokenize_inputs(
        &model.tokenizer,
        model.pad_token,
        &inputs,
        request.profile.max_input_tokens,
        request.limits.max_input_bytes,
        request.limits.max_total_input_tokens,
        limits,
        checkpoint,
    )?;
    let vectors = evaluate_batch(
        model,
        &tokenized.padded_ids,
        &tokenized.attention_masks,
        usize::from(request.limits.output_dimension),
        limits,
        checkpoint,
    )?;
    let output_bytes = vectors
        .len()
        .checked_mul(usize::from(request.limits.output_dimension))
        .and_then(|value| value.checked_mul(size_of::<f32>()))
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    if output_bytes > request.limits.max_output_bytes {
        return Err(product_error(ProductErrorCode::LimitExceeded));
    }
    Ok(ProductEmbeddingBatchOutput {
        vectors,
        input_tokens: tokenized.input_tokens,
        execution_profile: model
            .product_execution_profile(request.profile.header.id, limits.checkpoint_chunk_tokens)?,
    })
}

fn validate_product_request(
    request: ProductEmbeddingExecutorRequest<'_>,
) -> Result<(), ProductError> {
    request
        .profile
        .validate()
        .map_err(|_| product_error(ProductErrorCode::InvalidRequest))?;
    if request.profile.pipeline_version != EmbeddingPipelineVersion::Qwen3EmbeddingV1
        || request.documents.is_empty()
        || request.documents.len() > request.limits.max_inputs
        || request.limits.output_dimension != request.profile.vector_type.dimension()
        || request.limits.max_input_tokens_per_input != request.profile.max_input_tokens
        || request.limits.max_input_bytes == 0
        || request.limits.max_total_input_tokens == 0
        || request.limits.max_output_bytes == 0
    {
        return Err(product_error(ProductErrorCode::InvalidRequest));
    }
    Ok(())
}

struct TokenizedBatch {
    padded_ids: Vec<Vec<u32>>,
    attention_masks: Vec<Vec<u8>>,
    input_tokens: Vec<u32>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "tokenization keeps product and backend limits explicit"
)]
fn tokenize_inputs(
    tokenizer: &Tokenizer,
    pad_token: u32,
    inputs: &[&str],
    max_input_tokens: u32,
    max_input_bytes: usize,
    max_total_input_tokens: usize,
    limits: Qwen3CpuLimits,
    checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
) -> Result<TokenizedBatch, ProductError> {
    let maximum = usize::try_from(max_input_tokens)
        .map_err(|_| product_error(ProductErrorCode::LimitExceeded))?;
    let mut sequences = Vec::with_capacity(inputs.len());
    let mut input_tokens = Vec::with_capacity(inputs.len());
    let mut total_bytes = 0_usize;
    let mut total_tokens = 0_usize;
    let mut width = 0_usize;
    for input in inputs {
        checkpoint()?;
        total_bytes = total_bytes
            .checked_add(input.len())
            .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
        if total_bytes > max_input_bytes {
            return Err(product_error(ProductErrorCode::LimitExceeded));
        }
        let encoding = tokenizer
            .encode(*input, true)
            .map_err(|_| product_error(ProductErrorCode::InvalidRequest))?;
        let mut ids = encoding.get_ids().to_vec();
        ids.truncate(maximum);
        if ids.is_empty()
            || ids
                .iter()
                .any(|id| usize::try_from(*id).unwrap_or(usize::MAX) >= QWEN3_VOCABULARY)
        {
            return Err(product_error(ProductErrorCode::InvalidRequest));
        }
        total_tokens = total_tokens
            .checked_add(ids.len())
            .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
        if total_tokens > max_total_input_tokens {
            return Err(product_error(ProductErrorCode::LimitExceeded));
        }
        width = width.max(ids.len());
        input_tokens.push(
            u32::try_from(ids.len()).map_err(|_| product_error(ProductErrorCode::LimitExceeded))?,
        );
        sequences.push(ids);
    }
    let padded_positions = width
        .checked_mul(sequences.len())
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    if padded_positions > limits.max_batch_padded_tokens {
        return Err(product_error(ProductErrorCode::LimitExceeded));
    }
    admit_execution_costs(&sequences, padded_positions, limits)?;
    let mut padded_ids = Vec::with_capacity(sequences.len());
    let mut attention_masks = Vec::with_capacity(sequences.len());
    for sequence in sequences {
        let padding = width - sequence.len();
        let mut ids = vec![pad_token; padding];
        ids.extend_from_slice(&sequence);
        let mut mask = vec![0_u8; padding];
        mask.resize(width, 1);
        padded_ids.push(ids);
        attention_masks.push(mask);
    }
    Ok(TokenizedBatch {
        padded_ids,
        attention_masks,
        input_tokens,
    })
}

fn admit_execution_costs(
    sequences: &[Vec<u32>],
    padded_positions: usize,
    limits: Qwen3CpuLimits,
) -> Result<(), ProductError> {
    let total_tokens = sequences.iter().try_fold(0_u64, |total, sequence| {
        total.checked_add(u64::try_from(sequence.len()).ok()?)
    });
    let total_tokens =
        total_tokens.ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    let layer_work = total_tokens
        .checked_mul(QWEN3_LAYERS as u64)
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    let attention_work = sequences.iter().try_fold(0_u64, |total, sequence| {
        let tokens = u64::try_from(sequence.len()).ok()?;
        let triangle = tokens.checked_mul(tokens.checked_add(1)?)?.checked_div(2)?;
        total.checked_add(
            triangle
                .checked_mul(QWEN3_ATTENTION_HEADS as u64)?
                .checked_mul(QWEN3_LAYERS as u64)?,
        )
    });
    let attention_work =
        attention_work.ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    if layer_work > limits.max_layer_token_work || attention_work > limits.max_attention_work {
        return Err(product_error(ProductErrorCode::LimitExceeded));
    }

    let longest = sequences.iter().map(Vec::len).max().unwrap_or(0);
    let chunk = limits.checkpoint_chunk_tokens.min(longest);
    let attention_elements = QWEN3_ATTENTION_HEADS
        .checked_mul(chunk)
        .and_then(|value| value.checked_mul(longest))
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    if attention_elements > limits.max_tensor_elements {
        return Err(product_error(ProductErrorCode::LimitExceeded));
    }
    let kv_bytes = QWEN3_LAYERS
        .checked_mul(2)
        .and_then(|value| value.checked_mul(QWEN3_KV_HEADS))
        .and_then(|value| value.checked_mul(longest))
        .and_then(|value| value.checked_mul(QWEN3_HEAD_DIMENSION))
        .and_then(|value| value.checked_mul(size_of::<f32>()))
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    let attention_bytes = attention_elements
        .checked_mul(size_of::<f32>())
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    let feed_forward_bytes = chunk
        .checked_mul(QWEN3_INTERMEDIATE_DIMENSION)
        .and_then(|value| value.checked_mul(size_of::<f32>()))
        .and_then(|value| value.checked_mul(3))
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    let padded_bytes = padded_positions
        .checked_mul(size_of::<u32>() + size_of::<u8>())
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    let estimated = kv_bytes
        .checked_add(attention_bytes)
        .and_then(|value| value.checked_add(feed_forward_bytes))
        .and_then(|value| value.checked_add(padded_bytes))
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    if u64::try_from(estimated).unwrap_or(u64::MAX) > limits.max_execution_memory_bytes {
        return Err(product_error(ProductErrorCode::LimitExceeded));
    }
    Ok(())
}

fn evaluate_batch(
    loaded: &LoadedModel,
    padded_ids: &[Vec<u32>],
    attention_masks: &[Vec<u8>],
    output_dimension: usize,
    limits: Qwen3CpuLimits,
    checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
) -> Result<Vec<ProductVector>, ProductError> {
    if !QWEN3_EMBEDDING_OUTPUT_DIMENSIONS.contains(
        &u16::try_from(output_dimension)
            .map_err(|_| product_error(ProductErrorCode::InvalidRequest))?,
    ) {
        return Err(product_error(ProductErrorCode::InvalidRequest));
    }
    let mut vectors = Vec::with_capacity(padded_ids.len());
    for (ids, mask) in padded_ids.iter().zip(attention_masks) {
        checkpoint()?;
        let admitted: Vec<u32> = ids
            .iter()
            .zip(mask)
            .filter_map(|(id, admitted)| (*admitted == 1).then_some(*id))
            .collect();
        let mut model = loaded.model.clone();
        vectors.push(evaluate_one(
            &mut model,
            &loaded.backend.device,
            &admitted,
            output_dimension,
            limits.checkpoint_chunk_tokens,
            checkpoint,
        )?);
    }
    Ok(vectors)
}

fn evaluate_one(
    model: &mut Qwen3Model,
    device: &Device,
    ids: &[u32],
    output_dimension: usize,
    chunk_tokens: usize,
    checkpoint: &mut dyn FnMut() -> Result<(), ProductError>,
) -> Result<ProductVector, ProductError> {
    let mut offset = 0_usize;
    let mut final_hidden = None;
    for chunk in ids.chunks(chunk_tokens) {
        checkpoint()?;
        let input = Tensor::from_slice(chunk, (1, chunk.len()), device)
            .map_err(|_| product_error(ProductErrorCode::Unavailable))?;
        let hidden = model
            .forward(&input, offset)
            .map_err(|_| product_error(ProductErrorCode::Unavailable))?;
        final_hidden = Some(
            hidden
                .narrow(1, chunk.len() - 1, 1)
                .and_then(|tensor| tensor.squeeze(0))
                .and_then(|tensor| tensor.squeeze(0))
                .and_then(|tensor| tensor.to_dtype(DType::F32))
                .and_then(|tensor| tensor.to_vec1::<f32>())
                .map_err(|_| product_error(ProductErrorCode::Unavailable))?,
        );
        offset = offset
            .checked_add(chunk.len())
            .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
        checkpoint()?;
    }
    let mut values = final_hidden.ok_or_else(|| product_error(ProductErrorCode::InvalidRequest))?;
    if values.len() != usize::from(QWEN3_EMBEDDING_NATIVE_DIMENSION) {
        return Err(product_error(ProductErrorCode::Unavailable));
    }
    values.truncate(output_dimension);
    let mut squared_norm = 0.0_f32;
    for value in &values {
        if !value.is_finite() {
            return Err(product_error(ProductErrorCode::Unavailable));
        }
        squared_norm += value * value;
    }
    let denominator = squared_norm.sqrt().max(QWEN3_EMBEDDING_L2_EPSILON);
    for value in &mut values {
        *value /= denominator;
    }
    ProductVector::new(values).map_err(|_| product_error(ProductErrorCode::Unavailable))
}

fn load_model(
    mut descriptors: Qwen3ArtifactDescriptors,
    limits: Qwen3CpuLimits,
    backend: ExecutionBackend,
) -> Result<LoadedModel, Qwen3CpuError> {
    let manifest_bytes = read_bounded(
        &mut descriptors.manifest,
        MAX_EMBEDDING_ARTIFACT_MANIFEST_BYTES,
    )?;
    let manifest_length = u64::try_from(manifest_bytes.len())
        .map_err(|_| Qwen3CpuError::Manifest("manifest length overflowed"))?;
    let manifest_digest: [u8; 32] = Sha256::digest(&manifest_bytes).into();
    if manifest_digest == [0; 32] {
        return Err(Qwen3CpuError::Manifest("manifest digest is zero"));
    }
    let manifest: ArtifactManifest = serde_json::from_slice(&manifest_bytes)?;
    manifest.validate(limits)?;
    if descriptors.files.len() != manifest.files.len() {
        return Err(Qwen3CpuError::Artifact("snapshot file set differs"));
    }

    let mut config_bytes = None;
    let mut tokenizer_bytes = None;
    let mut weights_bytes = None;
    let mut load_memory = 0_u64;
    for specification in &manifest.files {
        let mut descriptor = descriptors
            .files
            .remove(&specification.path)
            .ok_or(Qwen3CpuError::Artifact("manifest file is missing"))?;
        if descriptor.metadata()?.len() != specification.size_bytes {
            return Err(Qwen3CpuError::Artifact("artifact length differs"));
        }
        let retain = matches!(
            specification.path.as_str(),
            CONFIG_PATH | TOKENIZER_PATH | WEIGHTS_PATH
        );
        let (digest, bytes) = hash_descriptor(&mut descriptor, specification.size_bytes, retain)?;
        let expected = decode_sha256(&specification.sha256)
            .ok_or(Qwen3CpuError::Manifest("artifact digest is malformed"))?;
        if digest != expected {
            return Err(Qwen3CpuError::Artifact("artifact digest differs"));
        }
        if let Some(bytes) = bytes {
            let multiplier = if specification.path == WEIGHTS_PATH {
                3
            } else {
                1
            };
            load_memory = load_memory
                .checked_add(specification.size_bytes.saturating_mul(multiplier))
                .ok_or(Qwen3CpuError::Artifact("load memory estimate overflowed"))?;
            match specification.path.as_str() {
                CONFIG_PATH => config_bytes = Some(bytes),
                TOKENIZER_PATH => tokenizer_bytes = Some(bytes),
                WEIGHTS_PATH => weights_bytes = Some(bytes),
                _ => {}
            }
        }
    }
    if !descriptors.files.is_empty() || load_memory > limits.max_load_memory_bytes {
        return Err(Qwen3CpuError::Artifact(
            "snapshot contains extras or exceeds load memory",
        ));
    }
    let config_bytes = config_bytes.ok_or(Qwen3CpuError::Artifact("config is missing"))?;
    let tokenizer_bytes = tokenizer_bytes.ok_or(Qwen3CpuError::Artifact("tokenizer is missing"))?;
    let weights_bytes = weights_bytes.ok_or(Qwen3CpuError::Artifact("weights are missing"))?;
    let config: Qwen3Config = serde_json::from_slice(&config_bytes)?;
    let config_identity: ModelConfigIdentity = serde_json::from_slice(&config_bytes)?;
    validate_model_config(&config, &config_identity)?;
    let tokenizer = Tokenizer::from_bytes(&tokenizer_bytes)
        .map_err(|error| Qwen3CpuError::Tokenizer(error.to_string()))?;
    let pad_token = tokenizer
        .get_vocab(true)
        .get(QWEN3_PAD_TOKEN)
        .copied()
        .ok_or_else(|| Qwen3CpuError::Tokenizer("padding token is absent".to_owned()))?;
    if usize::try_from(pad_token).unwrap_or(usize::MAX) >= QWEN3_VOCABULARY {
        return Err(Qwen3CpuError::Tokenizer(
            "padding token is outside the vocabulary".to_owned(),
        ));
    }
    let builder = VarBuilder::from_buffered_safetensors(
        weights_bytes,
        backend.compute_dtype,
        &backend.device,
    )
    .map_err(|error| Qwen3CpuError::Model(error.to_string()))?
    .rename_f(|name| name.strip_prefix("model.").unwrap_or(name).to_owned());
    let model = Qwen3Model::new(&config, builder)
        .map_err(|error| Qwen3CpuError::Model(error.to_string()))?;
    Ok(LoadedModel {
        key: ModelKey {
            digest: manifest_digest,
            manifest_bytes: manifest_length,
        },
        revision: manifest.model.revision,
        tokenizer,
        pad_token,
        model,
        backend,
    })
}

fn validate_model_config(
    config: &Qwen3Config,
    identity: &ModelConfigIdentity,
) -> Result<(), Qwen3CpuError> {
    if config.vocab_size != QWEN3_VOCABULARY
        || config.hidden_size != usize::from(QWEN3_EMBEDDING_NATIVE_DIMENSION)
        || config.intermediate_size != QWEN3_INTERMEDIATE_DIMENSION
        || config.num_hidden_layers != QWEN3_LAYERS
        || config.num_attention_heads != QWEN3_ATTENTION_HEADS
        || config.head_dim != QWEN3_HEAD_DIMENSION
        || config.attention_bias
        || config.num_key_value_heads != QWEN3_KV_HEADS
        || config.max_position_embeddings != QWEN3_MAX_POSITIONS
        || config.sliding_window.is_some()
        || config.max_window_layers != QWEN3_LAYERS
        || !config.tie_word_embeddings
        || config.rope_theta.to_bits() != 1_000_000_f64.to_bits()
        || config.rms_norm_eps.to_bits() != 0.000_001_f64.to_bits()
        || config.use_sliding_window
        || config.hidden_act != candle_nn::Activation::Silu
        || identity.architectures != ["Qwen3ForCausalLM"]
        || identity.attention_dropout.to_bits() != 0.0_f64.to_bits()
        || identity.bos_token_id != QWEN3_END_OF_TEXT_TOKEN_ID
        || identity.eos_token_id != QWEN3_END_OF_TEXT_TOKEN_ID
        || identity.model_type != MODEL_TYPE
        || !identity.rope_scaling.is_null()
        || identity.torch_dtype != "bfloat16"
        || !identity.use_cache
    {
        return Err(Qwen3CpuError::Model(
            "model architecture differs from Qwen3-Embedding-0.6B".to_owned(),
        ));
    }
    Ok(())
}

fn read_bounded(descriptor: &mut File, maximum: u64) -> Result<Vec<u8>, Qwen3CpuError> {
    descriptor.seek(SeekFrom::Start(0))?;
    let length = descriptor.metadata()?.len();
    if length == 0 || length > maximum {
        return Err(Qwen3CpuError::Artifact(
            "descriptor length exceeds its limit",
        ));
    }
    let capacity = usize::try_from(length)
        .map_err(|_| Qwen3CpuError::Artifact("descriptor length cannot be allocated"))?;
    let mut bytes = Vec::with_capacity(capacity);
    descriptor.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() != capacity {
        return Err(Qwen3CpuError::Artifact(
            "descriptor changed while it was read",
        ));
    }
    Ok(bytes)
}

fn hash_descriptor(
    descriptor: &mut File,
    expected_length: u64,
    retain: bool,
) -> Result<([u8; 32], Option<Vec<u8>>), Qwen3CpuError> {
    descriptor.seek(SeekFrom::Start(0))?;
    if retain {
        let bytes = read_bounded(descriptor, expected_length)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_length {
            return Err(Qwen3CpuError::Artifact(
                "artifact length changed while reading",
            ));
        }
        let digest = Sha256::digest(&bytes).into();
        return Ok((digest, Some(bytes)));
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut read = 0_u64;
    loop {
        let count = descriptor.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        read = read
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or(Qwen3CpuError::Artifact("artifact length overflowed"))?;
        if read > expected_length {
            return Err(Qwen3CpuError::Artifact("artifact grew while reading"));
        }
        hasher.update(&buffer[..count]);
    }
    if read != expected_length {
        return Err(Qwen3CpuError::Artifact("artifact shrank while reading"));
    }
    Ok((hasher.finalize().into(), None))
}

/// Formats one query exactly as required by `Qwen3EmbeddingV1`.
///
/// # Errors
///
/// Returns a limit error if the resulting UTF-8 length overflows `usize`.
pub fn format_qwen3_query(text: &str) -> Result<String, ProductError> {
    let capacity = "Instruct: \nQuery: "
        .len()
        .checked_add(QWEN3_EMBEDDING_QUERY_INSTRUCTION.len())
        .and_then(|value| value.checked_add(text.len()))
        .ok_or_else(|| product_error(ProductErrorCode::LimitExceeded))?;
    let mut formatted = String::with_capacity(capacity);
    formatted.push_str("Instruct: ");
    formatted.push_str(QWEN3_EMBEDDING_QUERY_INSTRUCTION);
    formatted.push_str("\nQuery: ");
    formatted.push_str(text);
    Ok(formatted)
}

fn product_error(code: ProductErrorCode) -> ProductError {
    ProductError::from_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_format_is_byte_exact() -> Result<(), ProductError> {
        assert_eq!(
            format_qwen3_query("trees")?,
            "Instruct: Given a web search query, retrieve relevant passages that answer the query\nQuery: trees"
        );
        Ok(())
    }

    #[test]
    fn manifest_paths_and_digests_are_closed() {
        assert!(valid_artifact_path("1_Pooling/config.json"));
        assert!(!valid_artifact_path("../model.safetensors"));
        assert!(!valid_artifact_path("/model.safetensors"));
        assert!(decode_sha256(&"ab".repeat(32)).is_some());
        assert!(decode_sha256(&"AB".repeat(32)).is_none());
    }

    #[test]
    fn execution_cost_rejects_a_tensor_above_its_bound() {
        let limits = Qwen3CpuLimits {
            max_tensor_elements: 1,
            ..Qwen3CpuLimits::default()
        };
        assert!(admit_execution_costs(&[vec![1, 2]], 2, limits).is_err());
    }

    #[cfg(feature = "cuda")]
    #[test]
    fn cuda_fallback_retries_only_the_complete_unavailable_batch() -> Result<(), ProductError> {
        let mut checkpoints = 0_usize;
        let mut fallback_calls = 0_usize;
        let inputs = [1_u32, 2, 3];
        let output = whole_batch_cpu_fallback(
            Err(product_error(ProductErrorCode::Unavailable)),
            &mut || {
                checkpoints += 1;
                Ok(())
            },
            |_| {
                fallback_calls += 1;
                Ok(inputs.to_vec())
            },
        )?;
        assert_eq!(output, inputs);
        assert_eq!(checkpoints, 1);
        assert_eq!(fallback_calls, 1);

        let Err(cancelled) = whole_batch_cpu_fallback(
            Err::<Vec<u32>, _>(product_error(ProductErrorCode::Cancelled)),
            &mut || Ok(()),
            |_| {
                fallback_calls += 1;
                Ok(inputs.to_vec())
            },
        ) else {
            return Err(product_error(ProductErrorCode::InvalidRequest));
        };
        assert_eq!(cancelled.code(), ProductErrorCode::Cancelled);
        assert_eq!(fallback_calls, 1);
        Ok(())
    }

    #[test]
    fn real_descriptor_load_runs_only_when_explicitly_configured() -> Result<(), Qwen3CpuError> {
        let Some(snapshot) = std::env::var_os("HYPHAE_QWEN3_TEST_MODEL_DIR") else {
            return Ok(());
        };
        let manifest = std::env::var_os("HYPHAE_QWEN3_TEST_MANIFEST")
            .ok_or(Qwen3CpuError::Artifact("test manifest is not configured"))?;
        let limits = Qwen3CpuLimits::default();
        let descriptors = Qwen3ArtifactDescriptors::open(manifest, snapshot, limits)?;
        let loaded = load_model(descriptors, limits, ExecutionBackend::cpu())?;
        assert_eq!(loaded.key.manifest_bytes, 8_323);
        assert_eq!(loaded.revision.len(), 40);
        let mut checkpoints = 0_usize;
        let tokenized = tokenize_inputs(
            &loaded.tokenizer,
            loaded.pad_token,
            &["descriptor-stable local embedding"],
            64,
            64,
            64,
            limits,
            &mut || {
                checkpoints += 1;
                Ok(())
            },
        )
        .map_err(|_| Qwen3CpuError::Model("real tokenization failed".to_owned()))?;
        let vectors = evaluate_batch(
            &loaded,
            &tokenized.padded_ids,
            &tokenized.attention_masks,
            384,
            limits,
            &mut || {
                checkpoints += 1;
                Ok(())
            },
        )
        .map_err(|_| Qwen3CpuError::Model("real CPU evaluation failed".to_owned()))?;
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].dimension(), 384);
        assert!(checkpoints >= 4);
        Ok(())
    }

    #[cfg(feature = "cuda")]
    #[test]
    fn real_h100_bfloat16_and_float16_run_under_lock() -> Result<(), Box<dyn std::error::Error>> {
        if std::env::var_os("HYPHAE_QWEN3_CUDA_TEST").is_none() {
            return Ok(());
        }
        let snapshot = std::env::var_os("HYPHAE_QWEN3_TEST_MODEL_DIR").ok_or(
            Qwen3CpuError::Artifact("test model directory is not configured"),
        )?;
        let manifest = std::env::var_os("HYPHAE_QWEN3_TEST_MANIFEST")
            .ok_or(Qwen3CpuError::Artifact("test manifest is not configured"))?;
        let lock_path = std::env::var_os("HYPHAE_QWEN3_CUDA_TEST_LOCK")
            .ok_or(Qwen3CpuError::Artifact("CUDA test lock is not configured"))?;
        let expected_uuid = std::env::var("HYPHAE_QWEN3_CUDA_TEST_UUID")?;
        let expected_pci = std::env::var("HYPHAE_QWEN3_CUDA_TEST_PCI_BUS_ID")?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        fs4::FileExt::lock(&lock)?;

        for (compute_dtype, expected_dtype) in [
            (
                Qwen3CudaComputeDType::BFloat16,
                "bfloat16-f32-output+whole-batch-cpu-f32-fallback",
            ),
            (
                Qwen3CudaComputeDType::Float16,
                "float16-f32-output+whole-batch-cpu-f32-fallback",
            ),
        ] {
            let limits = Qwen3CpuLimits::default();
            let executor = Qwen3AcceleratorExecutor::new_with_compute_dtype(limits, compute_dtype)?;
            if !executor.uses_cuda() {
                return Err(
                    Qwen3CpuError::Model("validated H100 was not selected".to_owned()).into(),
                );
            }
            assert!(executor.device_profile().contains(VALIDATED_H100_NAME));
            assert!(executor.device_profile().contains(&expected_uuid));
            assert!(executor.device_profile().ends_with(&expected_pci));

            let descriptors = Qwen3ArtifactDescriptors::open(&manifest, &snapshot, limits)?;
            let profile = executor.load_and_register(descriptors)?;
            assert_eq!(profile.backend(), "candle-qwen3-cuda");
            assert_eq!(profile.compute_dtype(), expected_dtype);
            assert_eq!(profile.device(), executor.device_profile());
            let loaded = executor
                .models
                .read()
                .map_err(|_| Qwen3CpuError::Registry("registry lock is poisoned"))?
                .values()
                .next()
                .cloned()
                .ok_or(Qwen3CpuError::Registry("test model is absent"))?;
            assert!(loaded.cpu_fallback.is_some());
            let mut checkpoints = 0_usize;
            let tokenized = tokenize_inputs(
                &loaded.primary.tokenizer,
                loaded.primary.pad_token,
                &["descriptor-stable local H100 embedding"],
                64,
                64,
                64,
                limits,
                &mut || {
                    checkpoints += 1;
                    Ok(())
                },
            )?;
            let vectors = evaluate_batch(
                &loaded.primary,
                &tokenized.padded_ids,
                &tokenized.attention_masks,
                384,
                limits,
                &mut || {
                    checkpoints += 1;
                    Ok(())
                },
            )?;
            assert_eq!(vectors.len(), 1);
            assert_eq!(vectors[0].dimension(), 384);
            assert_eq!(
                std::mem::size_of_val(vectors[0].values()),
                384 * size_of::<f32>()
            );
            assert!(vectors[0].values().iter().all(|value| value.is_finite()));
            assert!(checkpoints >= 4);
            eprintln!(
                "backend={} version={} device={} dtype={} output=f32 dimension={}",
                profile.backend(),
                profile.backend_version(),
                profile.device(),
                profile.compute_dtype(),
                vectors[0].dimension()
            );
        }
        Ok(())
    }

    #[test]
    fn manifest_digest_type_remains_compatible() -> Result<(), Box<dyn std::error::Error>> {
        let digest = hyphae_native_catalog::EmbeddingArtifactManifestDigest::new([1; 32])?;
        assert_eq!(digest.as_bytes(), &[1; 32]);
        Ok(())
    }
}
