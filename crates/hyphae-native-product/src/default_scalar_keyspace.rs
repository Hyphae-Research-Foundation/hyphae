// SPDX-License-Identifier: Apache-2.0

//! Durable catalog binding for the wire-compatible default scalar keyspace.

use hyphae_native_catalog::{
    CatalogName, CatalogObjectV2, DefinitionVersion, KeyspaceDefinition, KeyspaceEvictionPolicy,
    KeyspaceMemoryClass, KeyspaceTtlPolicy, LogicalCatalogObject, ObjectHeaderV2, QualifiedName,
    StructureKind, StructureOwnership,
};
use hyphae_native_runtime::CommitBoundary;
use hyphae_native_types::{EngineKind, LogicalType, ObjectId};

use crate::{NativeProduct, ProductDurability, ProductError, ProductErrorCode};

const BINDING_MAGIC: &[u8; 8] = b"HYPDKB01";
const BINDING_STORAGE_KEY: &[u8] = b"\0hyphae.product.default-keyspace.v1\0binding";
const BINDING_BYTES: usize = BINDING_MAGIC.len() + 24 + 16;
const INTERNAL_DATABASE: &str = "hyphae_internal";
const INTERNAL_SCHEMA: &str = "system";
const DATABASE_OBJECT_NAME: &str = "database";
const SCHEMA_OBJECT_NAME: &str = "schema";
const KEYSPACE_OBJECT_NAME: &str = "default_scalar";

impl NativeProduct {
    pub(crate) fn initialize_default_scalar_keyspace(
        &mut self,
        access_control_bootstrapped: bool,
    ) -> Result<(), ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        match snapshot.structure_get_internal(BINDING_STORAGE_KEY) {
            Some(encoded) => {
                self.default_scalar_keyspace_id = Some(validate_binding(&snapshot, encoded)?);
                Ok(())
            }
            None if access_control_bootstrapped => Err(corruption()),
            None => Err(ProductError::from_code(ProductErrorCode::UpgradeRequired)),
        }
    }

    pub(crate) fn initialize_upgrade_default_scalar_keyspace(
        &mut self,
    ) -> Result<(), ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        if let Some(encoded) = snapshot.structure_get_internal(BINDING_STORAGE_KEY) {
            self.default_scalar_keyspace_id = Some(validate_binding(&snapshot, encoded)?);
        }
        Ok(())
    }

    /// Explicitly provisions the missing pre-binding default keyspace.
    ///
    /// # Errors
    ///
    /// Returns corruption for a malformed existing binding, or a durability
    /// error if the strict migration commit fails.
    pub fn upgrade_default_scalar_keyspace_binding(&mut self) -> Result<bool, ProductError> {
        if self.default_scalar_keyspace_id.is_some() {
            self.initialize_default_scalar_keyspace(false)?;
            return Ok(false);
        }
        self.provision_default_scalar_keyspace(None)?;
        Ok(true)
    }

    pub(crate) fn initialize_pending_default_scalar_keyspace(
        &mut self,
        access_control_bootstrapped: bool,
    ) -> Result<(), ProductError> {
        let snapshot = self.snapshot_bounded(0)?;
        match snapshot.structure_get_internal(BINDING_STORAGE_KEY) {
            Some(encoded) => {
                self.default_scalar_keyspace_id = Some(validate_binding(&snapshot, encoded)?);
                Ok(())
            }
            None if access_control_bootstrapped => Err(corruption()),
            None => Ok(()),
        }
    }

    pub(crate) fn ensure_default_scalar_keyspace(&mut self) -> Result<ObjectId, ProductError> {
        if let Some(id) = self.default_scalar_keyspace_id {
            let snapshot = self.snapshot_bounded(0)?;
            let encoded = snapshot
                .structure_get_internal(BINDING_STORAGE_KEY)
                .ok_or_else(corruption)?;
            let validated = validate_binding(&snapshot, encoded)?;
            if validated != id {
                return Err(corruption());
            }
            return Ok(id);
        }
        self.provision_default_scalar_keyspace(None)
    }

    pub(crate) fn default_scalar_keyspace_id(&self) -> Result<ObjectId, ProductError> {
        self.default_scalar_keyspace_id.ok_or_else(corruption)
    }

    pub(crate) fn has_default_scalar_binding(&self) -> Result<bool, ProductError> {
        Ok(self
            .snapshot_bounded(0)?
            .structure_get_internal(BINDING_STORAGE_KEY)
            .is_some())
    }

    fn provision_default_scalar_keyspace(
        &mut self,
        interruption: Option<CommitBoundary>,
    ) -> Result<ObjectId, ProductError> {
        let lineage = self.database.directory_identity().lineage().encode();
        let mut transaction = self.database.begin(0, ProductDurability::Strict.into())?;
        if transaction.get(BINDING_STORAGE_KEY).is_some() {
            return Err(corruption());
        }

        let database = transaction.next_catalog_object_id()?;
        transaction.create_catalog_object_v2(database_definition(database)?)?;
        let schema = transaction.next_catalog_object_id()?;
        transaction.create_catalog_object_v2(schema_definition(schema, database)?)?;
        let keyspace = transaction.next_catalog_object_id()?;
        transaction.create_catalog_object_v2(keyspace_definition(keyspace, schema)?)?;
        transaction.set(
            BINDING_STORAGE_KEY.to_vec(),
            encode_binding(lineage, keyspace),
            None,
        )?;

        let receipt = match interruption {
            Some(boundary) => transaction.commit_with_interruption(boundary),
            None => transaction.commit(),
        }?;
        self.observe_commit(&receipt);
        self.default_scalar_keyspace_id = Some(keyspace);
        Ok(keyspace)
    }
}

fn database_definition(id: ObjectId) -> Result<LogicalCatalogObject, ProductError> {
    Ok(LogicalCatalogObject::V2(CatalogObjectV2::Database(
        expected_header(id, EngineKind::Kernel, DATABASE_OBJECT_NAME, None)?,
    )))
}

fn schema_definition(
    id: ObjectId,
    database: ObjectId,
) -> Result<LogicalCatalogObject, ProductError> {
    Ok(LogicalCatalogObject::V2(CatalogObjectV2::Schema(
        expected_header(id, EngineKind::Kernel, SCHEMA_OBJECT_NAME, Some(database))?,
    )))
}

fn keyspace_definition(
    id: ObjectId,
    schema: ObjectId,
) -> Result<LogicalCatalogObject, ProductError> {
    Ok(LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(
        KeyspaceDefinition {
            header: expected_header(
                id,
                EngineKind::Structure,
                KEYSPACE_OBJECT_NAME,
                Some(schema),
            )?,
            kind: StructureKind::String,
            key_type: LogicalType::Binary,
            value_type: LogicalType::Binary,
            ownership: StructureOwnership::Canonical,
            ttl_policy: KeyspaceTtlPolicy::PerValue,
            default_ttl_millis: None,
            memory_class: KeyspaceMemoryClass::Durable,
            eviction: KeyspaceEvictionPolicy::None,
            relation_schema: None,
        },
    )))
}

fn expected_header(
    id: ObjectId,
    owner: EngineKind,
    object: &str,
    parent: Option<ObjectId>,
) -> Result<ObjectHeaderV2, ProductError> {
    Ok(ObjectHeaderV2 {
        id,
        owner,
        name: QualifiedName::new(
            CatalogName::unquoted(INTERNAL_DATABASE).map_err(|_| internal())?,
            CatalogName::unquoted(INTERNAL_SCHEMA).map_err(|_| internal())?,
            CatalogName::unquoted(object).map_err(|_| internal())?,
        ),
        parent,
        definition_version: DefinitionVersion::FIRST,
    })
}

fn encode_binding(lineage: [u8; 24], keyspace: ObjectId) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(BINDING_BYTES);
    encoded.extend_from_slice(BINDING_MAGIC);
    encoded.extend_from_slice(&lineage);
    encoded.extend_from_slice(&keyspace.get().to_le_bytes());
    encoded
}

fn decode_binding(encoded: &[u8], lineage: [u8; 24]) -> Result<ObjectId, ProductError> {
    if encoded.len() != BINDING_BYTES
        || &encoded[..BINDING_MAGIC.len()] != BINDING_MAGIC
        || encoded[BINDING_MAGIC.len()..BINDING_MAGIC.len() + 24] != lineage
    {
        return Err(corruption());
    }
    let mut id = [0_u8; 16];
    id.copy_from_slice(&encoded[BINDING_MAGIC.len() + 24..]);
    ObjectId::new(u128::from_le_bytes(id)).map_err(|_| corruption())
}

fn validate_binding(
    snapshot: &crate::ProductSnapshot,
    encoded: &[u8],
) -> Result<ObjectId, ProductError> {
    let keyspace = decode_binding(encoded, snapshot.identity().directory_lineage)?;
    let logical_keyspace = snapshot
        .inner
        .logical_catalog_object(keyspace)
        .ok_or_else(corruption)?;
    let schema = logical_keyspace.parent().ok_or_else(corruption)?;
    if logical_keyspace != &keyspace_definition(keyspace, schema)? {
        return Err(corruption());
    }

    let logical_schema = snapshot
        .inner
        .logical_catalog_object(schema)
        .ok_or_else(corruption)?;
    let database = logical_schema.parent().ok_or_else(corruption)?;
    if logical_schema != &schema_definition(schema, database)? {
        return Err(corruption());
    }

    let logical_database = snapshot
        .inner
        .logical_catalog_object(database)
        .ok_or_else(corruption)?;
    if logical_database != &database_definition(database)? {
        return Err(corruption());
    }
    Ok(keyspace)
}

fn corruption() -> ProductError {
    ProductError::from_code(ProductErrorCode::Corruption)
}

fn internal() -> ProductError {
    ProductError::from_code(ProductErrorCode::Internal)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use hyphae_native_catalog::{CatalogName, CatalogObjectV2, LogicalCatalogObject};
    use hyphae_native_runtime::{CommitBoundary, NativeDatabase, NativeRuntimeError};
    use hyphae_native_types::DurabilityClass;

    use super::{BINDING_MAGIC, BINDING_STORAGE_KEY, NativeProduct};
    use crate::ProductErrorCode;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hyphae-default-scalar-{name}-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn binding_reopens_with_the_same_id_and_preserves_existing_scalar_data()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary("reopen-existing");
        let _ignored = fs::remove_dir_all(&path);
        let mut runtime = NativeDatabase::create(&path)?;
        let mut seed = runtime.begin(0, DurabilityClass::Strict)?;
        seed.set(b"existing".to_vec(), b"value".to_vec(), None)?;
        seed.commit()?;
        drop(runtime);

        let error = NativeProduct::open(&path).expect_err("preview directory opened implicitly");
        assert_eq!(error.code(), ProductErrorCode::UpgradeRequired);
        let product = NativeProduct::open_with_preview_default_scalar_migration(&path)?;
        let id = product.default_scalar_keyspace_id()?;
        assert_eq!(
            product.snapshot_bounded(0)?.structure_get(b"existing"),
            Some(b"value".as_slice())
        );
        drop(product);

        let reopened = NativeProduct::open(&path)?;
        assert_eq!(reopened.default_scalar_keyspace_id()?, id);
        assert_eq!(
            reopened.snapshot_bounded(0)?.structure_get(b"existing"),
            Some(b"value".as_slice())
        );
        drop(reopened);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn malformed_bindings_fail_closed_on_reopen() -> Result<(), Box<dyn std::error::Error>> {
        for (label, mode, corrupt) in [
            ("truncated", "raw", BINDING_MAGIC.to_vec()),
            ("trailing", "valid-prefix", {
                let mut value = vec![0_u8; super::BINDING_BYTES + 1];
                value[..8].copy_from_slice(BINDING_MAGIC);
                value
            }),
            ("zero", "valid-prefix", {
                let mut value = vec![0_u8; super::BINDING_BYTES];
                value[..8].copy_from_slice(BINDING_MAGIC);
                value
            }),
            ("corrupt", "valid-prefix", {
                let mut value = vec![0_u8; super::BINDING_BYTES];
                value[..8].copy_from_slice(b"NOTDKB01");
                value
            }),
        ] {
            let path = temporary(label);
            let _ignored = fs::remove_dir_all(&path);
            let mut product = NativeProduct::create(&path)?;
            let lineage = product.database.directory_identity().lineage().encode();
            let id = product.default_scalar_keyspace_id()?;
            let mut transaction = product.database.begin(0, DurabilityClass::Strict)?;
            let mut corrupt = corrupt;
            if mode == "valid-prefix" {
                corrupt[8..32].copy_from_slice(&lineage);
            }
            if matches!(label, "trailing" | "corrupt") {
                corrupt[32..48].copy_from_slice(&id.get().to_le_bytes());
            }
            transaction.set(BINDING_STORAGE_KEY.to_vec(), corrupt, None)?;
            transaction.commit()?;
            drop(product);

            let error = NativeProduct::open(&path).expect_err("malformed binding reopened");
            assert_eq!(error.code(), ProductErrorCode::Corruption, "{label}");
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    #[test]
    fn explicit_upgrade_rejects_a_malformed_existing_binding()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary("upgrade-corrupt");
        let _ignored = fs::remove_dir_all(&path);
        let mut product = NativeProduct::create(&path)?;
        let mut transaction = product.database.begin(0, DurabilityClass::Strict)?;
        transaction.set(
            BINDING_STORAGE_KEY.to_vec(),
            b"corrupt-existing-binding".to_vec(),
            None,
        )?;
        transaction.commit()?;
        drop(product);

        let error = NativeProduct::open_for_upgrade(&path)
            .expect_err("explicit upgrade accepted a corrupt binding");
        assert_eq!(error.code(), ProductErrorCode::Corruption);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn binding_rejects_definition_and_lineage_corruption() -> Result<(), Box<dyn std::error::Error>>
    {
        for label in ["definition", "lineage"] {
            let path = temporary(label);
            let _ignored = fs::remove_dir_all(&path);
            let mut product = NativeProduct::create(&path)?;
            let id = product.default_scalar_keyspace_id()?;
            let snapshot = product.snapshot_bounded(0)?;
            let LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(mut definition)) = snapshot
                .inner
                .logical_catalog_object(id)
                .ok_or("keyspace is missing")?
                .clone()
            else {
                return Err("bound object is not a keyspace".into());
            };
            drop(snapshot);
            let lineage = product.database.directory_identity().lineage().encode();
            let mut transaction = product.database.begin(0, DurabilityClass::Strict)?;
            if label == "definition" {
                let decoy = transaction.next_catalog_object_id()?;
                definition.header.id = decoy;
                definition.header.name.object = CatalogName::unquoted("default_scalar_decoy")?;
                transaction.create_catalog_object_v2(LogicalCatalogObject::V2(
                    CatalogObjectV2::Keyspace(definition),
                ))?;
                transaction.set(
                    BINDING_STORAGE_KEY.to_vec(),
                    super::encode_binding(lineage, decoy),
                    None,
                )?;
            } else {
                let mut corrupt = super::encode_binding(lineage, id);
                corrupt[8] ^= 1;
                transaction.set(BINDING_STORAGE_KEY.to_vec(), corrupt, None)?;
            }
            transaction.commit()?;
            drop(product);

            let error = NativeProduct::open(&path).expect_err("corrupt binding reopened");
            assert_eq!(error.code(), ProductErrorCode::Corruption);
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    #[test]
    fn bootstrapped_directory_without_binding_fails_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = temporary("missing-managed");
        let owner = path.with_extension("owner");
        let _ignored = fs::remove_dir_all(&path);
        let _ignored = fs::remove_file(&owner);
        let mut product = NativeProduct::create(&path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner, 1)?;
        let mut transaction = product.database.begin(0, DurabilityClass::Strict)?;
        assert!(transaction.delete_structure(BINDING_STORAGE_KEY.to_vec())?);
        transaction.commit()?;
        drop(product);

        let error = NativeProduct::open(&path).expect_err("missing managed binding reopened");
        assert_eq!(error.code(), ProductErrorCode::Corruption);
        fs::remove_file(owner)?;
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn preview_migration_is_explicit_and_rejected_after_bootstrap()
    -> Result<(), Box<dyn std::error::Error>> {
        let preview_path = temporary("explicit-preview");
        let _ignored = fs::remove_dir_all(&preview_path);
        drop(NativeDatabase::create(&preview_path)?);
        let open_error = NativeProduct::open(&preview_path).expect_err("preview opened implicitly");
        assert_eq!(open_error.code(), ProductErrorCode::UpgradeRequired);
        let migrated = NativeProduct::open_with_preview_default_scalar_migration(&preview_path)?;
        let id = migrated.default_scalar_keyspace_id()?;
        drop(migrated);
        assert_eq!(
            NativeProduct::open(&preview_path)?.default_scalar_keyspace_id()?,
            id
        );
        fs::remove_dir_all(preview_path)?;

        let managed_path = temporary("managed-preview-rejection");
        let owner = managed_path.with_extension("owner");
        let _ignored = fs::remove_dir_all(&managed_path);
        let _ignored = fs::remove_file(&owner);
        let mut managed = NativeProduct::create(&managed_path)?;
        managed.bootstrap_access_control_to_file("Owner", "owner", &owner, 1)?;
        let mut transaction = managed.database.begin(0, DurabilityClass::Strict)?;
        assert!(transaction.delete_structure(BINDING_STORAGE_KEY.to_vec())?);
        transaction.commit()?;
        drop(managed);
        let error = NativeProduct::open_with_preview_default_scalar_migration(&managed_path)
            .expect_err("managed directory was migrated");
        assert_eq!(error.code(), ProductErrorCode::Corruption);
        fs::remove_file(owner)?;
        fs::remove_dir_all(managed_path)?;
        Ok(())
    }

    #[test]
    fn interrupted_provisioning_recovers_all_or_none() -> Result<(), Box<dyn std::error::Error>> {
        for boundary in [
            CommitBoundary::BlobStaged,
            CommitBoundary::BlobPromoted,
            CommitBoundary::PageAppended,
            CommitBoundary::PageSynchronized,
            CommitBoundary::WalAppended,
            CommitBoundary::WalSynchronized,
            CommitBoundary::RootPublished,
        ] {
            let path = temporary(&format!("crash-{boundary:?}"));
            let _ignored = fs::remove_dir_all(&path);
            let mut product = NativeProduct::create_pending(&path)?;
            let error = product
                .provision_default_scalar_keyspace(Some(boundary))
                .expect_err("interrupted provisioning completed");
            assert_eq!(
                error,
                crate::ProductError::from(NativeRuntimeError::InjectedCrash(boundary))
            );
            drop(product);

            let reopened = NativeProduct::open_pending(&path)?;
            let snapshot = reopened.snapshot_bounded(0)?;
            let binding = snapshot.structure_get_internal(BINDING_STORAGE_KEY);
            if let Some(encoded) = binding {
                let id = super::validate_binding(&snapshot, encoded)?;
                assert_eq!(reopened.default_scalar_keyspace_id()?, id);
            } else {
                assert!(reopened.default_scalar_keyspace_id.is_none());
                let catalog = reopened.catalog_snapshot()?;
                for name in [
                    super::expected_header(
                        hyphae_native_types::ObjectId::new(1)?,
                        hyphae_native_types::EngineKind::Kernel,
                        super::DATABASE_OBJECT_NAME,
                        None,
                    )?
                    .name,
                    super::expected_header(
                        hyphae_native_types::ObjectId::new(1)?,
                        hyphae_native_types::EngineKind::Kernel,
                        super::SCHEMA_OBJECT_NAME,
                        Some(hyphae_native_types::ObjectId::new(2)?),
                    )?
                    .name,
                    super::expected_header(
                        hyphae_native_types::ObjectId::new(1)?,
                        hyphae_native_types::EngineKind::Structure,
                        super::KEYSPACE_OBJECT_NAME,
                        Some(hyphae_native_types::ObjectId::new(2)?),
                    )?
                    .name,
                ] {
                    assert_eq!(reopened.catalog_resolve(&catalog, &name)?, None);
                }
            }
            drop(reopened);
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }
}
