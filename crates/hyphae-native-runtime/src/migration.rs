// SPDX-License-Identifier: Apache-2.0

//! Durable, offline migration evidence shared by the importer and validators.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current migration-manifest wire version.
pub const NATIVE_MIGRATION_MANIFEST_VERSION: u16 = 1;
/// Stable migration-manifest type discriminator.
pub const NATIVE_MIGRATION_MANIFEST_KIND: &str = "hyphae-native-migration-manifest";

/// Bounded manifest decoder policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MigrationManifestLimits {
    /// Maximum encoded manifest bytes.
    pub max_bytes: usize,
    /// Maximum source-key mappings.
    pub max_documents: usize,
    /// Maximum object mappings.
    pub max_objects: usize,
    /// Maximum receipt mappings.
    pub max_receipts: usize,
}

impl Default for MigrationManifestLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_documents: 1_000_000,
            max_objects: 128,
            max_receipts: 1_000_000,
        }
    }
}

/// Failure while encoding or validating migration evidence.
#[derive(Debug, Error)]
pub enum MigrationManifestError {
    /// JSON was malformed or contained an unknown field.
    #[error("invalid migration manifest JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The manifest exceeded a configured bound.
    #[error("migration manifest exceeds {field} limit {maximum}")]
    Limit {
        /// Bounded field.
        field: &'static str,
        /// Maximum admitted value.
        maximum: usize,
    },
    /// A required value violated the migration contract.
    #[error("invalid migration manifest: {0}")]
    Invalid(&'static str),
}

/// Verified format-2 source snapshot identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationSource {
    /// Source disk format.
    pub disk_format_version: u16,
    /// Captured source checkpoint sequence.
    pub checkpoint_sequence: u64,
    /// Optional source checkpoint digest as lowercase hexadecimal.
    pub checkpoint_digest: Option<String>,
    /// Complete source logical snapshot digest as lowercase hexadecimal.
    pub snapshot_digest: String,
    /// Source logical entry count.
    pub entry_count: u64,
    /// Source vector-space count.
    pub vector_space_count: u64,
    /// Source vector count.
    pub vector_count: u64,
    /// Source lexical-index count.
    pub lexical_index_count: u64,
    /// Source receipt count.
    pub receipt_count: u64,
    /// Exact source vector-space definitions.
    pub vector_spaces: Vec<MigrationVectorSpace>,
    /// Exact source lexical-index definitions.
    pub lexical_indexes: Vec<MigrationLexicalIndex>,
}

/// One preserved format-2 vector-space definition.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationVectorSpace {
    /// Canonical source name.
    pub name: String,
    /// Fixed source dimension.
    pub dimension: u16,
    /// Source metric discriminant.
    pub metric: u8,
}

/// One preserved format-2 lexical field definition.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationLexicalField {
    /// Exact field path segments.
    pub path: Vec<String>,
    /// Positive source field weight in millionths.
    pub weight_micros: u32,
}

/// One preserved format-2 lexical-index definition.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationLexicalIndex {
    /// Canonical source name.
    pub name: String,
    /// Exact source fields and weights.
    pub fields: Vec<MigrationLexicalField>,
}

/// Native target identity captured at migration validation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationTarget {
    /// Native directory UUIDv7 string.
    pub directory_id: String,
    /// Native history epoch.
    pub history_epoch: u64,
    /// Target logical entry count.
    pub entry_count: u64,
    /// Target vector-space count.
    pub vector_space_count: u64,
    /// Target vector count.
    pub vector_count: u64,
    /// Target lexical-index count.
    pub lexical_index_count: u64,
    /// Target receipt count represented by the manifest.
    pub receipt_count: u64,
    /// Digest of the target migration namespace.
    pub logical_digest: String,
}

/// Stable mapping from one source identity to one Native identity.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationObject {
    /// Object family, for example `document`, `vector-space`, or `lexical-index`.
    pub kind: String,
    /// Canonical source identity in lowercase hexadecimal or UTF-8 name form.
    pub source_identity: String,
    /// Nonzero Native object identity represented as decimal text.
    pub target_id: String,
}

/// One source record to Native document identity mapping.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationDocument {
    /// Exact source binary key as lowercase hexadecimal.
    pub source_key: String,
    /// Stable nonzero Native object identity represented as decimal text.
    pub object_id: String,
}

/// One preserved format-2 receipt and its caller-visible identity.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationReceipt {
    /// Source transaction UUID as lowercase hexadecimal.
    pub transaction_id: String,
    /// Source commit sequence.
    pub commit_sequence: u64,
    /// Source commit digest as lowercase hexadecimal.
    pub commit_digest: String,
    /// Source transaction digest as lowercase hexadecimal.
    pub transaction_digest: String,
    /// Preserved caller identity, equal to `transaction_id`.
    pub idempotency_identity: String,
}

/// One source proof or snapshot anchor carried across migration.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationProofAnchor {
    /// Stable anchor kind.
    pub kind: String,
    /// Source anchor digest as lowercase hexadecimal.
    pub source_digest: String,
    /// Target evidence digest as lowercase hexadecimal.
    pub target_digest: String,
}

/// Complete source-to-target migration evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationManifest {
    /// Wire version.
    pub version: u16,
    /// Wire type discriminator.
    pub kind: String,
    /// Verified source snapshot.
    pub source: MigrationSource,
    /// Validated target identity and counts.
    pub target: MigrationTarget,
    /// Stable Native object mappings.
    pub objects: Vec<MigrationObject>,
    /// Stable record identities.
    pub documents: Vec<MigrationDocument>,
    /// Preserved source receipts.
    pub receipts: Vec<MigrationReceipt>,
    /// Preserved proof and snapshot anchors.
    pub proof_anchors: Vec<MigrationProofAnchor>,
}

fn valid_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes.saturating_mul(2)
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

impl MigrationManifest {
    /// Encodes the canonical compact JSON representation.
    pub fn encode(&self) -> Result<Vec<u8>, MigrationManifestError> {
        self.validate(&MigrationManifestLimits::default())?;
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MigrationManifestLimits::default().max_bytes {
            return Err(MigrationManifestError::Limit {
                field: "bytes",
                maximum: MigrationManifestLimits::default().max_bytes,
            });
        }
        Ok(encoded)
    }

    /// Decodes and validates one bounded manifest.
    pub fn decode(
        encoded: &[u8],
        limits: &MigrationManifestLimits,
    ) -> Result<Self, MigrationManifestError> {
        if encoded.len() > limits.max_bytes {
            return Err(MigrationManifestError::Limit {
                field: "bytes",
                maximum: limits.max_bytes,
            });
        }
        let manifest: Self = serde_json::from_slice(encoded)?;
        manifest.validate(limits)?;
        Ok(manifest)
    }

    /// Validates format, bounds, canonical ordering, and identity continuity.
    pub fn validate(&self, limits: &MigrationManifestLimits) -> Result<(), MigrationManifestError> {
        if self.version != NATIVE_MIGRATION_MANIFEST_VERSION {
            return Err(MigrationManifestError::Invalid(
                "unsupported manifest version",
            ));
        }
        if self.kind != NATIVE_MIGRATION_MANIFEST_KIND {
            return Err(MigrationManifestError::Invalid("unexpected manifest kind"));
        }
        if self.source.disk_format_version != 2 {
            return Err(MigrationManifestError::Invalid(
                "source is not disk format 2",
            ));
        }
        if !valid_hex(&self.source.snapshot_digest, 32)
            || self
                .source
                .checkpoint_digest
                .as_ref()
                .is_some_and(|digest| !valid_hex(digest, 32))
            || !valid_hex(&self.target.logical_digest, 32)
        {
            return Err(MigrationManifestError::Invalid(
                "digest must be 32-byte hexadecimal",
            ));
        }
        if self.target.directory_id.is_empty() || self.target.history_epoch == 0 {
            return Err(MigrationManifestError::Invalid(
                "target lineage is incomplete",
            ));
        }
        if self.documents.len() > limits.max_documents {
            return Err(MigrationManifestError::Limit {
                field: "documents",
                maximum: limits.max_documents,
            });
        }
        if self.objects.len() > limits.max_objects {
            return Err(MigrationManifestError::Limit {
                field: "objects",
                maximum: limits.max_objects,
            });
        }
        if self.receipts.len() > limits.max_receipts {
            return Err(MigrationManifestError::Limit {
                field: "receipts",
                maximum: limits.max_receipts,
            });
        }
        if self.documents.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(MigrationManifestError::Invalid(
                "documents are not canonical",
            ));
        }
        if self.objects.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(MigrationManifestError::Invalid("objects are not canonical"));
        }
        if self.receipts.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(MigrationManifestError::Invalid(
                "receipts are not canonical",
            ));
        }
        for receipt in &self.receipts {
            if !valid_hex(&receipt.transaction_id, 16)
                || !valid_hex(&receipt.commit_digest, 32)
                || !valid_hex(&receipt.transaction_digest, 32)
                || receipt.transaction_id != receipt.idempotency_identity
            {
                return Err(MigrationManifestError::Invalid(
                    "receipt idempotency identity was not preserved",
                ));
            }
        }
        for anchor in &self.proof_anchors {
            if !valid_hex(&anchor.source_digest, 32) || !valid_hex(&anchor.target_digest, 32) {
                return Err(MigrationManifestError::Invalid(
                    "proof anchor digest is not 32-byte hexadecimal",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MigrationManifest, MigrationManifestLimits};

    #[test]
    fn manifest_rejects_wrong_source_and_receipt_identity() {
        let value = serde_json::json!({
            "version": 1,
            "kind": "hyphae-native-migration-manifest",
            "source": {
                "disk_format_version": 1,
                "checkpoint_sequence": 1,
                "checkpoint_digest": null,
                "snapshot_digest": "00".repeat(32),
                "entry_count": 0,
                "vector_space_count": 0,
                "vector_count": 0,
                "lexical_index_count": 0,
                "receipt_count": 0,
                "vector_spaces": [],
                "lexical_indexes": []
            },
            "target": {
                "directory_id": "018f4e9d-3d7a-7b6c-8f12-123456789abc",
                "history_epoch": 1,
                "entry_count": 0,
                "vector_space_count": 0,
                "vector_count": 0,
                "lexical_index_count": 0,
                "receipt_count": 0,
                "logical_digest": "00".repeat(32)
            },
            "objects": [],
            "documents": [],
            "receipts": []
        });
        let encoded = serde_json::to_vec(&value).expect("encode test manifest");
        assert!(MigrationManifest::decode(&encoded, &MigrationManifestLimits::default()).is_err());
    }

    #[test]
    fn manifest_round_trips_canonical_json() -> Result<(), Box<dyn std::error::Error>> {
        let digest = "00".repeat(32);
        let manifest = MigrationManifest {
            version: 1,
            kind: super::NATIVE_MIGRATION_MANIFEST_KIND.to_owned(),
            source: super::MigrationSource {
                disk_format_version: 2,
                checkpoint_sequence: 1,
                checkpoint_digest: None,
                snapshot_digest: digest.clone(),
                entry_count: 0,
                vector_space_count: 0,
                vector_count: 0,
                lexical_index_count: 0,
                receipt_count: 0,
                vector_spaces: Vec::new(),
                lexical_indexes: Vec::new(),
            },
            target: super::MigrationTarget {
                directory_id: "018f4e9d-3d7a-7b6c-8f12-123456789abc".to_owned(),
                history_epoch: 1,
                entry_count: 0,
                vector_space_count: 0,
                vector_count: 0,
                lexical_index_count: 0,
                receipt_count: 0,
                logical_digest: digest,
            },
            objects: Vec::new(),
            documents: Vec::new(),
            receipts: Vec::new(),
            proof_anchors: Vec::new(),
        };
        let encoded = manifest.encode()?;
        assert_eq!(
            MigrationManifest::decode(&encoded, &MigrationManifestLimits::default())?,
            manifest
        );
        Ok(())
    }
}
