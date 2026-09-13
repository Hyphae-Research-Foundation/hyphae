// SPDX-License-Identifier: Apache-2.0

//! Shared local policy for MCP, proactive hooks and operator controls.

use crate::{agent::AgentPaths, exit::CliFailure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Policy {
    pub schema: String,
    pub capture_enabled: bool,
    pub paused_projects: BTreeSet<String>,
    pub context_bytes: usize,
    pub recall_timeout_ms: u64,
    pub collections: [u128; 3],
    pub manage_services: bool,
    pub semantic: Semantic,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Semantic {
    pub enabled: bool,
    pub model_dir: Option<PathBuf>,
    pub model: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "SemanticSearchMode::is_hybrid")]
    pub search_mode: SemanticSearchMode,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SemanticSearchMode {
    #[default]
    Hybrid,
    Semantic,
}

impl SemanticSearchMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Semantic => "semantic",
        }
    }

    // Serde's skip_serializing_if predicate receives a reference.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    const fn is_hybrid(&self) -> bool {
        matches!(self, Self::Hybrid)
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            schema: "hyphae-agent-policy-v1".into(),
            capture_enabled: true,
            paused_projects: BTreeSet::new(),
            context_bytes: 2_000,
            recall_timeout_ms: 1_000,
            collections: [21, 22, 23],
            manage_services: true,
            semantic: Semantic::default(),
        }
    }
}

impl Policy {
    /// Local capture preferences may change independently; native profile
    /// bindings are authoritative after a migration or interrupted cache write.
    pub(crate) async fn current() -> Result<Self, CliFailure> {
        let mut policy = Self::load()?;
        let paths = AgentPaths::resolve()?;
        if paths.data.join("FORMAT").exists() {
            let active = if crate::agent::local_endpoint_present(&paths) {
                Self::active(&crate::agent_control::operator_client()?).await?
            } else {
                let product = hyphae_native_product::NativeProduct::open(&paths.data)?;
                Self::from_product(&product)?.unwrap_or_else(|| policy.clone())
            };
            policy.collections = active.collections;
            policy.semantic = active.semantic;
        }
        policy.validate()?;
        Ok(policy)
    }

    pub(crate) fn from_product(
        product: &hyphae_native_product::NativeProduct,
    ) -> Result<Option<Self>, CliFailure> {
        let snapshot = product.snapshot_bounded(crate::native::logical_time_micros())?;
        snapshot
            .structure_get(crate::agent_control::POLICY_KEY)
            .map(|bytes| {
                if bytes.len() > 64 * 1024 {
                    return Err(CliFailure::invalid());
                }
                let policy: Self = serde_json::from_slice(bytes)?;
                policy.validate()?;
                Ok(policy)
            })
            .transpose()
    }

    pub(crate) async fn active(
        client: &hyphae_client::v2::HyphaeClient,
    ) -> Result<Self, CliFailure> {
        let response = client
            .structure_get(
                crate::agent_control::POLICY_KEY.to_vec(),
                hyphae_client::v2::RequestOptions::default(),
            )
            .await
            .map_err(crate::mcp::normalize_client_error)?;
        match response {
            hyphae_native_product::ProductResponse::StructureValue(Some(bytes)) => {
                if bytes.len() > 64 * 1024 {
                    return Err(CliFailure::invalid());
                }
                let policy: Self = serde_json::from_slice(&bytes)?;
                policy.validate()?;
                Ok(policy)
            }
            hyphae_native_product::ProductResponse::StructureValue(None) => Self::load(),
            _ => Err(CliFailure::invalid()),
        }
    }
    pub(crate) fn load() -> Result<Self, CliFailure> {
        let path = AgentPaths::resolve()?.config.join("memory-policy.json");
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() <= 64 * 1024 => {
                let policy: Self = serde_json::from_slice(&bytes)?;
                policy.validate()?;
                Ok(policy)
            }
            Ok(_) => Err(CliFailure::invalid()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), CliFailure> {
        if self.schema != "hyphae-agent-policy-v1"
            || !(256..=16_384).contains(&self.context_bytes)
            || !(100..=5_000).contains(&self.recall_timeout_ms)
            || self.collections.contains(&0)
            || self
                .collections
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != 3
            || self.paused_projects.len() > 256
            || self
                .paused_projects
                .iter()
                .any(|project| project.is_empty() || project.len() > 256)
            || self.semantic.enabled
                && (self.semantic.model_dir.is_none() || self.model_fingerprint().is_none())
        {
            return Err(CliFailure::invalid());
        }
        Ok(())
    }

    pub(crate) fn save(&self) -> Result<(), CliFailure> {
        self.validate()?;
        let paths = AgentPaths::resolve()?;
        std::fs::create_dir_all(&paths.config)?;
        crate::agent::write_atomic(
            &paths.config.join("memory-policy.json"),
            &(serde_json::to_string_pretty(self)? + "\n").into_bytes(),
        )
    }

    pub(crate) fn capture_allowed(&self, project: &str) -> bool {
        self.capture_enabled && !self.paused_projects.contains(project)
    }

    pub(crate) fn collection(&self, layer: &str) -> Result<u128, CliFailure> {
        match layer {
            "personal" => Ok(self.collections[0]),
            "work" => Ok(self.collections[1]),
            "journal" => Ok(self.collections[2]),
            _ => Err(CliFailure::invalid()),
        }
    }

    pub(crate) fn model_fingerprint(&self) -> Option<&str> {
        self.semantic
            .model
            .as_ref()?
            .get("fingerprint")?
            .as_str()
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    }

    pub(crate) fn embedding_dimensions(&self) -> Option<u16> {
        let value = self.semantic.model.as_ref()?.get("dimensions")?.as_u64()?;
        u16::try_from(value)
            .ok()
            .filter(|value| (1..=4096).contains(value))
    }
}

pub(crate) fn runtime_directory() -> Result<PathBuf, CliFailure> {
    let path = AgentPaths::resolve()?.config.join("runtime");
    std::fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

pub(crate) fn embedding_endpoint() -> Result<PathBuf, CliFailure> {
    Ok(AgentPaths::resolve()?.config.join("runtime/embedding.sock"))
}

pub(crate) fn record_project(project: &str, cwd: &Path) -> Result<(), CliFailure> {
    let path = AgentPaths::resolve()?.config.join("memory-projects.json");
    let mut entries = match std::fs::read(&path) {
        Ok(bytes) if bytes.len() < 128 * 1024 => {
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        _ => return Err(CliFailure::invalid()),
    };
    let label = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Project");
    let label = if crate::agent_hooks::contains_sensitive_data(label) {
        "Project"
    } else {
        label
    };
    entries.insert(
        project.to_owned(),
        serde_json::json!({"id":project,"label":label.chars().take(80).collect::<String>()}),
    );
    if entries.len() > 256 {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::agent::write_atomic(
        &path,
        &(serde_json::to_string(&entries)? + "\n").into_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_semantic_policy_retains_hybrid_shape_and_explicit_mode_roundtrips()
    -> Result<(), serde_json::Error> {
        let legacy = serde_json::json!({"enabled":false,"model_dir":null,"model":null});
        let mut semantic: Semantic = serde_json::from_value(legacy.clone())?;
        assert_eq!(semantic.search_mode, SemanticSearchMode::Hybrid);
        assert_eq!(serde_json::to_value(&semantic)?, legacy);
        semantic.search_mode = SemanticSearchMode::Semantic;
        let encoded = serde_json::to_value(&semantic)?;
        assert_eq!(encoded["search_mode"], "semantic");
        assert_eq!(
            serde_json::from_value::<Semantic>(encoded)?.search_mode,
            SemanticSearchMode::Semantic
        );
        assert!(
            serde_json::from_value::<Semantic>(serde_json::json!({"search_mode":"proxy"})).is_err()
        );
        Ok(())
    }

    #[test]
    fn pause_is_scoped_and_invalid_semantic_state_fails_closed() {
        let mut policy = Policy::default();
        policy.paused_projects.insert("one".into());
        assert!(!policy.capture_allowed("one"));
        assert!(policy.capture_allowed("two"));
        policy.capture_enabled = false;
        assert!(!policy.capture_allowed("two"));
        policy.semantic.enabled = true;
        assert!(policy.validate().is_err());
    }
}
