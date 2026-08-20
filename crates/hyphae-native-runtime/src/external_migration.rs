// SPDX-License-Identifier: Apache-2.0

//! Offline-verifiable evidence for migrations from external source systems.
//!
//! The external-migration receipt records the declared source identity, the
//! consistency point, every construct classification, every operator waiver,
//! every mapping decision, and the resulting target state, and seals the
//! complete document under one domain-separated BLAKE3 content digest. A
//! verifier that trusts only the receipt bytes can detect any tampering
//! offline; equivalence against the live source and target is established by
//! the importer's verification pass, not by this module.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current external-migration receipt wire version.
pub const EXTERNAL_MIGRATION_RECEIPT_VERSION: u16 = 1;
/// Stable external-migration receipt type discriminator.
pub const EXTERNAL_MIGRATION_RECEIPT_KIND: &str = "hyphae-external-migration-receipt";
/// Domain tag binding the sealed content digest to this receipt version.
pub const EXTERNAL_MIGRATION_DIGEST_DOMAIN: &[u8] = b"hyphae-external-migration-receipt-v1";

/// Bounded receipt decoder policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct ExternalMigrationReceiptLimits {
    /// Maximum encoded receipt bytes.
    pub max_bytes: usize,
    /// Maximum construct classifications.
    pub max_classifications: usize,
    /// Maximum operator waivers.
    pub max_waivers: usize,
    /// Maximum mapping decisions.
    pub max_mappings: usize,
    /// Maximum target keyspaces.
    pub max_keyspaces: usize,
    /// Maximum auxiliary source fields.
    pub max_aux_fields: usize,
}

impl Default for ExternalMigrationReceiptLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_classifications: 1_024,
            max_waivers: 128,
            max_mappings: 1_024,
            max_keyspaces: 4_096,
            max_aux_fields: 128,
        }
    }
}

/// Failure while encoding or validating external-migration evidence.
#[derive(Debug, Error)]
pub enum ExternalMigrationReceiptError {
    /// JSON was malformed or contained an unknown field.
    #[error("invalid external-migration receipt JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The receipt exceeded a configured bound.
    #[error("external-migration receipt exceeds {field} limit {maximum}")]
    Limit {
        /// Bounded field.
        field: &'static str,
        /// Maximum admitted value.
        maximum: usize,
    },
    /// A required value violated the external-migration contract.
    #[error("invalid external-migration receipt: {0}")]
    Invalid(&'static str),
}

/// Fidelity class assigned to one source construct.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FidelityClass {
    /// One-to-one mapping with verifiable value equivalence.
    Exact,
    /// Mapping with a declared, checkable semantic guarantee.
    Equivalent,
    /// Mapping with documented loss; requires an explicit operator waiver.
    DeclaredDegraded,
    /// No mapping exists; the construct aborts the migration unless waived.
    Rejected,
}

/// Declared identity of the external source artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSourceIdentity {
    /// Source kind discriminator, for example `valkey-rdb`.
    pub kind: String,
    /// Source format version, for example the RDB version number.
    pub format_version: u32,
    /// Lowercase BLAKE3 hex digest of the complete source artifact bytes.
    pub source_digest: String,
    /// Exact source artifact length in bytes.
    pub source_bytes: u64,
    /// Lowercase hex of the source's own integrity checksum, when present.
    pub source_checksum: Option<String>,
    /// Auxiliary source metadata as sorted key/value pairs.
    pub aux_fields: Vec<(String, String)>,
    /// Number of logical databases encountered in the source.
    pub database_count: u32,
    /// Number of keys encountered in the source before expiry filtering.
    pub key_count: u64,
}

/// Declared consistency point of the source artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalConsistencyPoint {
    /// Consistency-point kind, for example `rdb-file-point-in-time`.
    pub kind: String,
    /// Honest statement of what the equivalence proof covers.
    pub statement: String,
    /// Measured host-to-source clock skew in microseconds, when applicable.
    pub clock_skew_micros: Option<i64>,
}

/// Classification of one encountered source construct.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConstructClassification {
    /// Stable construct identifier, for example `streams`.
    pub construct: String,
    /// Assigned fidelity class.
    pub class: FidelityClass,
    /// Human-readable classification detail.
    pub detail: String,
    /// Number of source items covered by this classification.
    pub count: u64,
}

/// One explicit operator waiver.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorWaiver {
    /// Waived construct identifier.
    pub construct: String,
    /// Action the waiver authorized, for example `skip` or `degrade`.
    pub action: String,
    /// Number of source items affected by the waiver.
    pub keys_affected: u64,
}

/// One recorded mapping decision.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingDecision {
    /// Construct the decision applies to.
    pub construct: String,
    /// Stable decision identifier.
    pub decision: String,
    /// Human-readable decision detail.
    pub detail: String,
}

/// One catalogued target keyspace produced by the migration.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetKeyspace {
    /// Canonical catalog name.
    pub name: String,
    /// Decimal catalog object identifier.
    pub object_id: String,
    /// Structure family stored in the keyspace.
    pub family: String,
    /// Number of migrated entries in the keyspace.
    pub entry_count: u64,
}

/// Resulting state of the pending Native target directory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalTargetState {
    /// Target directory UUID.
    pub directory_id: String,
    /// Target directory history epoch.
    pub history_epoch: u64,
    /// Catalogued keyspaces in canonical order.
    pub keyspaces: Vec<TargetKeyspace>,
    /// Lowercase BLAKE3 hex digest of the migrated logical content.
    pub logical_digest: String,
}

/// Complete sealed external-migration receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalMigrationReceipt {
    /// Receipt wire version.
    pub version: u16,
    /// Receipt type discriminator.
    pub kind: String,
    /// Declared source identity.
    pub source: ExternalSourceIdentity,
    /// Declared consistency point.
    pub consistency: ExternalConsistencyPoint,
    /// Import wall-clock instant in microseconds, pinned for verification.
    pub import_time_micros: i64,
    /// Every encountered construct classification in canonical order.
    pub classifications: Vec<ConstructClassification>,
    /// Every explicit operator waiver in canonical order.
    pub waivers: Vec<OperatorWaiver>,
    /// Every recorded mapping decision in canonical order.
    pub mappings: Vec<MappingDecision>,
    /// Resulting target state.
    pub target: ExternalTargetState,
    /// Lowercase BLAKE3 hex digest sealing this receipt's content.
    pub content_digest: String,
}

fn valid_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes.saturating_mul(2)
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn strictly_ascending<T: Ord>(rows: &[T]) -> bool {
    rows.windows(2).all(|pair| pair[0] < pair[1])
}

impl ExternalMigrationReceipt {
    /// Computes the sealed content digest over the digest-cleared canonical
    /// JSON representation.
    ///
    /// # Errors
    ///
    /// Returns an error when canonical encoding fails.
    pub fn compute_content_digest(&self) -> Result<String, ExternalMigrationReceiptError> {
        let mut cleared = self.clone();
        cleared.content_digest = String::new();
        let payload = serde_json::to_vec(&cleared)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(EXTERNAL_MIGRATION_DIGEST_DOMAIN);
        hasher.update(
            &u64::try_from(payload.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(&payload);
        let digest = hasher.finalize();
        let mut encoded = String::with_capacity(64);
        for byte in digest.as_bytes() {
            encoded.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            encoded.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
        }
        Ok(encoded)
    }

    /// Seals the receipt by writing its recomputed content digest.
    ///
    /// # Errors
    ///
    /// Returns an error when canonical encoding fails.
    pub fn seal(&mut self) -> Result<(), ExternalMigrationReceiptError> {
        self.content_digest = self.compute_content_digest()?;
        Ok(())
    }

    /// Encodes the canonical compact JSON representation of a sealed receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when validation or canonical encoding fails.
    pub fn encode(&self) -> Result<Vec<u8>, ExternalMigrationReceiptError> {
        let limits = ExternalMigrationReceiptLimits::default();
        self.validate(&limits)?;
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > limits.max_bytes {
            return Err(ExternalMigrationReceiptError::Limit {
                field: "bytes",
                maximum: limits.max_bytes,
            });
        }
        Ok(encoded)
    }

    /// Decodes and validates one bounded sealed receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is malformed, oversized, tampered, or
    /// invalid.
    pub fn decode(
        encoded: &[u8],
        limits: &ExternalMigrationReceiptLimits,
    ) -> Result<Self, ExternalMigrationReceiptError> {
        if encoded.len() > limits.max_bytes {
            return Err(ExternalMigrationReceiptError::Limit {
                field: "bytes",
                maximum: limits.max_bytes,
            });
        }
        let receipt: Self = serde_json::from_slice(encoded)?;
        receipt.validate(limits)?;
        Ok(receipt)
    }

    fn validate_source(
        &self,
        limits: &ExternalMigrationReceiptLimits,
    ) -> Result<(), ExternalMigrationReceiptError> {
        if self.source.kind.is_empty() {
            return Err(ExternalMigrationReceiptError::Invalid(
                "source kind is empty",
            ));
        }
        if !valid_hex(&self.source.source_digest, 32) {
            return Err(ExternalMigrationReceiptError::Invalid(
                "source digest is not 32-byte lowercase hex",
            ));
        }
        if self
            .source
            .source_checksum
            .as_ref()
            .is_some_and(|checksum| checksum.is_empty() || checksum.len() > 64)
        {
            return Err(ExternalMigrationReceiptError::Invalid(
                "source checksum shape is invalid",
            ));
        }
        if self.source.aux_fields.len() > limits.max_aux_fields {
            return Err(ExternalMigrationReceiptError::Limit {
                field: "aux_fields",
                maximum: limits.max_aux_fields,
            });
        }
        if !strictly_ascending(&self.source.aux_fields) {
            return Err(ExternalMigrationReceiptError::Invalid(
                "auxiliary fields are not strictly ascending",
            ));
        }
        if self.consistency.kind.is_empty() || self.consistency.statement.is_empty() {
            return Err(ExternalMigrationReceiptError::Invalid(
                "consistency point is incomplete",
            ));
        }
        Ok(())
    }

    fn validate_bounds(
        &self,
        limits: &ExternalMigrationReceiptLimits,
    ) -> Result<(), ExternalMigrationReceiptError> {
        if self.classifications.is_empty() {
            return Err(ExternalMigrationReceiptError::Invalid(
                "receipt classifies no constructs",
            ));
        }
        if self.classifications.len() > limits.max_classifications {
            return Err(ExternalMigrationReceiptError::Limit {
                field: "classifications",
                maximum: limits.max_classifications,
            });
        }
        if self.waivers.len() > limits.max_waivers {
            return Err(ExternalMigrationReceiptError::Limit {
                field: "waivers",
                maximum: limits.max_waivers,
            });
        }
        if self.mappings.len() > limits.max_mappings {
            return Err(ExternalMigrationReceiptError::Limit {
                field: "mappings",
                maximum: limits.max_mappings,
            });
        }
        if self.target.keyspaces.len() > limits.max_keyspaces {
            return Err(ExternalMigrationReceiptError::Limit {
                field: "keyspaces",
                maximum: limits.max_keyspaces,
            });
        }
        if !strictly_ascending(&self.classifications)
            || !strictly_ascending(&self.waivers)
            || !strictly_ascending(&self.mappings)
            || !strictly_ascending(&self.target.keyspaces)
        {
            return Err(ExternalMigrationReceiptError::Invalid(
                "receipt collections are not in strict canonical order",
            ));
        }
        Ok(())
    }

    fn validate_waiver_coverage(&self) -> Result<(), ExternalMigrationReceiptError> {
        for classification in &self.classifications {
            if classification.construct.is_empty() {
                return Err(ExternalMigrationReceiptError::Invalid(
                    "classification names no construct",
                ));
            }
            let requires_waiver = matches!(
                classification.class,
                FidelityClass::DeclaredDegraded | FidelityClass::Rejected
            );
            if requires_waiver
                && classification.count > 0
                && !self
                    .waivers
                    .iter()
                    .any(|waiver| waiver.construct == classification.construct)
            {
                return Err(ExternalMigrationReceiptError::Invalid(
                    "a degraded or rejected construct has no operator waiver",
                ));
            }
        }
        for waiver in &self.waivers {
            if waiver.construct.is_empty() || waiver.action.is_empty() {
                return Err(ExternalMigrationReceiptError::Invalid(
                    "waiver is incomplete",
                ));
            }
            if !self
                .classifications
                .iter()
                .any(|classification| classification.construct == waiver.construct)
            {
                return Err(ExternalMigrationReceiptError::Invalid(
                    "waiver names an unclassified construct",
                ));
            }
        }
        Ok(())
    }

    /// Validates format, bounds, canonical ordering, waiver coverage, and the
    /// sealed content digest.
    ///
    /// # Errors
    ///
    /// Returns an error when any invariant fails.
    pub fn validate(
        &self,
        limits: &ExternalMigrationReceiptLimits,
    ) -> Result<(), ExternalMigrationReceiptError> {
        if self.version != EXTERNAL_MIGRATION_RECEIPT_VERSION {
            return Err(ExternalMigrationReceiptError::Invalid(
                "receipt version differs",
            ));
        }
        if self.kind != EXTERNAL_MIGRATION_RECEIPT_KIND {
            return Err(ExternalMigrationReceiptError::Invalid(
                "receipt kind differs",
            ));
        }
        self.validate_source(limits)?;
        self.validate_bounds(limits)?;
        self.validate_waiver_coverage()?;
        if self.target.directory_id.is_empty() {
            return Err(ExternalMigrationReceiptError::Invalid(
                "target directory identity is empty",
            ));
        }
        if self.target.history_epoch == 0 {
            return Err(ExternalMigrationReceiptError::Invalid(
                "target history epoch is zero",
            ));
        }
        if !valid_hex(&self.target.logical_digest, 32) {
            return Err(ExternalMigrationReceiptError::Invalid(
                "target logical digest is not 32-byte lowercase hex",
            ));
        }
        let expected = self.compute_content_digest()?;
        if self.content_digest != expected {
            return Err(ExternalMigrationReceiptError::Invalid(
                "content digest differs from the sealed receipt",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConstructClassification, EXTERNAL_MIGRATION_RECEIPT_KIND,
        EXTERNAL_MIGRATION_RECEIPT_VERSION, ExternalConsistencyPoint, ExternalMigrationReceipt,
        ExternalMigrationReceiptError, ExternalMigrationReceiptLimits, ExternalSourceIdentity,
        ExternalTargetState, FidelityClass, MappingDecision, OperatorWaiver, TargetKeyspace,
    };

    fn receipt() -> Result<ExternalMigrationReceipt, ExternalMigrationReceiptError> {
        let mut receipt = ExternalMigrationReceipt {
            version: EXTERNAL_MIGRATION_RECEIPT_VERSION,
            kind: EXTERNAL_MIGRATION_RECEIPT_KIND.to_owned(),
            source: ExternalSourceIdentity {
                kind: "valkey-rdb".to_owned(),
                format_version: 11,
                source_digest: "ab".repeat(32),
                source_bytes: 512,
                source_checksum: Some("00ff00ff00ff00ff".to_owned()),
                aux_fields: vec![("redis-ver".to_owned(), "7.2.5".to_owned())],
                database_count: 2,
                key_count: 9,
            },
            consistency: ExternalConsistencyPoint {
                kind: "rdb-file-point-in-time".to_owned(),
                statement: "the destination corresponds to this RDB file, not to what \
                            clients last observed"
                    .to_owned(),
                clock_skew_micros: None,
            },
            import_time_micros: 1_755_000_000_000_000,
            classifications: vec![
                ConstructClassification {
                    construct: "streams".to_owned(),
                    class: FidelityClass::DeclaredDegraded,
                    detail: "entry order preserved; identifiers remapped; groups dropped"
                        .to_owned(),
                    count: 1,
                },
                ConstructClassification {
                    construct: "strings".to_owned(),
                    class: FidelityClass::Exact,
                    detail: "raw, integer, and LZF encodings".to_owned(),
                    count: 7,
                },
            ],
            waivers: vec![OperatorWaiver {
                construct: "streams".to_owned(),
                action: "degrade".to_owned(),
                keys_affected: 1,
            }],
            mappings: vec![MappingDecision {
                construct: "strings".to_owned(),
                decision: "integer-decode".to_owned(),
                detail: "integer-encoded strings decode to decimal bytes".to_owned(),
            }],
            target: ExternalTargetState {
                directory_id: "0198f1a2-0000-7000-8000-000000000001".to_owned(),
                history_epoch: 1,
                keyspaces: vec![TargetKeyspace {
                    name: "main.public.valkey_db0_strings".to_owned(),
                    object_id: "3".to_owned(),
                    family: "string".to_owned(),
                    entry_count: 7,
                }],
                logical_digest: "cd".repeat(32),
            },
            content_digest: String::new(),
        };
        receipt.seal()?;
        Ok(receipt)
    }

    #[test]
    fn sealed_receipt_round_trips_canonically() -> Result<(), ExternalMigrationReceiptError> {
        let receipt = receipt()?;
        let encoded = receipt.encode()?;
        let decoded =
            ExternalMigrationReceipt::decode(&encoded, &ExternalMigrationReceiptLimits::default())?;
        assert_eq!(decoded, receipt);
        Ok(())
    }

    #[test]
    fn tampered_content_fails_digest_validation() -> Result<(), ExternalMigrationReceiptError> {
        let mut tampered = receipt()?;
        tampered.source.key_count = 10;
        assert!(matches!(
            tampered.validate(&ExternalMigrationReceiptLimits::default()),
            Err(ExternalMigrationReceiptError::Invalid(
                "content digest differs from the sealed receipt"
            ))
        ));
        Ok(())
    }

    #[test]
    fn unknown_fields_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let encoded = receipt()?.encode()?;
        let mut value: serde_json::Value = serde_json::from_slice(&encoded)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("injected".to_owned(), serde_json::Value::Bool(true));
        }
        let reencoded = serde_json::to_vec(&value)?;
        assert!(matches!(
            ExternalMigrationReceipt::decode(
                &reencoded,
                &ExternalMigrationReceiptLimits::default()
            ),
            Err(ExternalMigrationReceiptError::Json(_))
        ));
        Ok(())
    }

    #[test]
    fn degraded_construct_without_waiver_fails_closed() -> Result<(), ExternalMigrationReceiptError>
    {
        let mut receipt = receipt()?;
        receipt.waivers.clear();
        receipt.seal()?;
        assert!(matches!(
            receipt.validate(&ExternalMigrationReceiptLimits::default()),
            Err(ExternalMigrationReceiptError::Invalid(
                "a degraded or rejected construct has no operator waiver"
            ))
        ));
        Ok(())
    }

    #[test]
    fn limits_and_ordering_fail_closed() -> Result<(), ExternalMigrationReceiptError> {
        let mut unordered = receipt()?;
        unordered.classifications.swap(0, 1);
        unordered.seal()?;
        assert!(
            unordered
                .validate(&ExternalMigrationReceiptLimits::default())
                .is_err()
        );

        let limits = ExternalMigrationReceiptLimits {
            max_classifications: 1,
            ..ExternalMigrationReceiptLimits::default()
        };
        assert!(matches!(
            receipt()?.validate(&limits),
            Err(ExternalMigrationReceiptError::Limit {
                field: "classifications",
                ..
            })
        ));
        Ok(())
    }
}
