// SPDX-License-Identifier: Apache-2.0

//! Focused logical catalog V2 persistence and bounded product API coverage.

use std::{fs, path::PathBuf};

use hyphae_native_catalog::{
    CatalogName, CatalogObjectV2, DefinitionVersion, DependencyDirection,
    EmbeddingArtifactManifestDigest, EmbeddingPipelineVersion, EmbeddingProfileDefinition,
    IncrementalVectorLifecycle, LogicalCatalogObject, NamedVectorDefinition, ObjectHeaderV2,
    QWEN3_EMBEDDING_QUERY_INSTRUCTION, QualifiedName, SearchCollectionDefinitionV2, VectorMetric,
    VectorSearchPolicy,
};
use hyphae_native_product::{
    BackupRequest, CatalogDependencyRequest, CatalogListRequest, NativeProduct, ProductDurability,
    ProductErrorCategory, ProductErrorCode, ProgressControl, RestoreRequest, restore,
};
use hyphae_native_types::{EngineKind, FieldId, ObjectId, VectorElement, VectorType};

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyphae-native-product-catalog-{name}-{}",
        std::process::id()
    ))
}

fn header(
    id: u128,
    name: &str,
    parent: Option<u128>,
) -> Result<ObjectHeaderV2, Box<dyn std::error::Error>> {
    Ok(ObjectHeaderV2 {
        id: ObjectId::new(id)?,
        owner: EngineKind::Kernel,
        name: QualifiedName::new(
            CatalogName::unquoted("main")?,
            CatalogName::unquoted("public")?,
            CatalogName::unquoted(name)?,
        ),
        parent: parent.map(ObjectId::new).transpose()?,
        definition_version: DefinitionVersion::FIRST,
    })
}

fn qwen_objects() -> Result<Vec<LogicalCatalogObject>, Box<dyn std::error::Error>> {
    let database =
        LogicalCatalogObject::V2(CatalogObjectV2::Database(header(10, "database", None)?));
    let schema = LogicalCatalogObject::V2(CatalogObjectV2::Schema(header(11, "schema", Some(10))?));
    let search_header = |id, name| -> Result<_, Box<dyn std::error::Error>> {
        Ok(ObjectHeaderV2 {
            id: ObjectId::new(id)?,
            owner: EngineKind::Search,
            name: QualifiedName::new(
                CatalogName::unquoted("main")?,
                CatalogName::unquoted("public")?,
                CatalogName::unquoted(name)?,
            ),
            parent: Some(ObjectId::new(11)?),
            definition_version: DefinitionVersion::FIRST,
        })
    };
    let profile = LogicalCatalogObject::V2(CatalogObjectV2::EmbeddingProfile(
        EmbeddingProfileDefinition {
            header: search_header(12, "qwen3_embedding_0_6b")?,
            artifact_manifest_digest: EmbeddingArtifactManifestDigest::new([7; 32])?,
            artifact_manifest_byte_length: 8_323,
            pipeline_version: EmbeddingPipelineVersion::Qwen3EmbeddingV1,
            vector_type: VectorType::new(VectorElement::Float32, 384)?,
            max_input_tokens: 256,
            query_instruction: QWEN3_EMBEDDING_QUERY_INSTRUCTION.to_owned(),
        },
    ));
    let collection = LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
        SearchCollectionDefinitionV2 {
            header: search_header(13, "documents")?,
            fields: Vec::new(),
            vectors: vec![NamedVectorDefinition {
                id: FieldId::new(1)?,
                name: CatalogName::unquoted("embedding")?,
                vector_type: VectorType::new(VectorElement::Float32, 384)?,
                metric: VectorMetric::Cosine,
                policy: VectorSearchPolicy::Exact,
                lifecycle: IncrementalVectorLifecycle {
                    delta_max_entries: 64,
                    consolidate_after_deltas: 4,
                    retain_generations: 2,
                },
                embedding_profile: Some(ObjectId::new(12)?),
            }],
            bm25: None,
        },
    ));
    Ok(vec![database, schema, profile, collection])
}

#[test]
#[allow(clippy::too_many_lines)]
fn product_catalog_pages_bind_cursor_to_snapshot_and_apply_limits()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temporary("bounded");
    let _ = fs::remove_dir_all(&path);
    let mut product = NativeProduct::create(&path)?;
    let database =
        LogicalCatalogObject::V2(CatalogObjectV2::Database(header(10, "database", None)?));
    let schema = LogicalCatalogObject::V2(CatalogObjectV2::Schema(header(11, "schema", Some(10))?));
    product.create_catalog_object_v2(database, ProductDurability::Strict)?;
    product.create_catalog_object_v2(schema.clone(), ProductDurability::Strict)?;

    let snapshot = product.catalog_snapshot()?;
    let internal_keyspace = product
        .catalog_resolve(
            &snapshot,
            &QualifiedName::new(
                CatalogName::unquoted("hyphae_internal")?,
                CatalogName::unquoted("system")?,
                CatalogName::unquoted("default_scalar")?,
            ),
        )?
        .ok_or("default scalar keyspace is missing")?
        .id();
    let first = product.catalog_list(
        &snapshot,
        CatalogListRequest {
            parent: None,
            kind: None,
            cursor: Some(hyphae_native_product::CatalogCursor::new(
                snapshot.identity(),
                internal_keyspace,
            )),
            item_limit: 1,
            visit_limit: 2,
            byte_limit: 4_096,
        },
    )?;
    assert_eq!(first.items.len(), 1);
    let cursor = first.cursor.ok_or("missing catalog cursor")?;
    let next = product.catalog_list(
        &snapshot,
        CatalogListRequest {
            parent: Some(ObjectId::new(10)?),
            kind: None,
            cursor: Some(cursor),
            item_limit: 2,
            visit_limit: 2,
            byte_limit: 4_096,
        },
    )?;
    assert_eq!(next.items.len(), 1);
    assert_eq!(
        product.catalog_describe(&snapshot, ObjectId::new(11)?)?,
        Some(schema.clone())
    );
    assert_eq!(
        product.catalog_resolve(&snapshot, schema.name())?,
        Some(schema)
    );

    let outgoing = product.catalog_dependencies(
        &snapshot,
        CatalogDependencyRequest {
            object: ObjectId::new(11)?,
            direction: DependencyDirection::Outgoing,
            cursor: None,
            item_limit: 1,
            visit_limit: 1,
            byte_limit: 33,
        },
    )?;
    assert_eq!(outgoing.items[0].prerequisite, ObjectId::new(10)?);

    let future = LogicalCatalogObject::V2(CatalogObjectV2::Schema(header(12, "future", Some(10))?));
    product.create_catalog_object_v2(future, ProductDurability::Strict)?;
    let newer = product.catalog_snapshot()?;
    let error = product
        .catalog_list(
            &newer,
            CatalogListRequest {
                parent: None,
                kind: None,
                cursor: Some(cursor),
                item_limit: 2,
                visit_limit: 2,
                byte_limit: 4_096,
            },
        )
        .err()
        .ok_or("foreign snapshot cursor unexpectedly accepted")?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);
    assert_eq!(error.category(), ProductErrorCategory::Conflict);

    let limit = product
        .catalog_list(
            &newer,
            CatalogListRequest {
                parent: None,
                kind: None,
                cursor: None,
                item_limit: 0,
                visit_limit: 1,
                byte_limit: 1,
            },
        )
        .err()
        .ok_or("zero catalog limit unexpectedly accepted")?;
    assert_eq!(limit.code(), ProductErrorCode::LimitExceeded);
    assert_eq!(limit.category(), ProductErrorCategory::Limit);
    assert_eq!(product.capabilities().catalog_tree_format_version, 7);

    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}

#[test]
fn qwen_profile_binding_survives_product_reopen_and_backup_restore()
-> Result<(), Box<dyn std::error::Error>> {
    let root = temporary("qwen-backup");
    let source = root.join("source");
    let backup = root.join("backup");
    let restored = root.join("restored");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root)?;

    let objects = qwen_objects()?;
    let mut product = NativeProduct::create(&source)?;
    product.create_catalog_objects_v2(objects.clone(), ProductDurability::Strict)?;
    drop(product);

    let mut reopened = NativeProduct::open(&source)?;
    let snapshot = reopened.catalog_snapshot()?;
    assert_eq!(
        reopened.catalog_describe(&snapshot, ObjectId::new(12)?)?,
        Some(objects[2].clone())
    );
    let dependencies = reopened.catalog_dependencies(
        &snapshot,
        CatalogDependencyRequest {
            object: ObjectId::new(12)?,
            direction: DependencyDirection::Incoming,
            cursor: None,
            item_limit: 1,
            visit_limit: 1,
            byte_limit: 64,
        },
    )?;
    assert_eq!(dependencies.items.len(), 1);
    assert_eq!(dependencies.items[0].dependent, ObjectId::new(13)?);
    reopened
        .administration()
        .backup(&BackupRequest::new(&backup)?, |_| ProgressControl::Continue)?;
    drop(reopened);

    restore(&RestoreRequest::new(&backup, &restored)?, |_| {
        ProgressControl::Continue
    })?;
    let restored_product = NativeProduct::open(&restored)?;
    let snapshot = restored_product.catalog_snapshot()?;
    assert_eq!(
        restored_product.catalog_describe(&snapshot, ObjectId::new(12)?)?,
        Some(objects[2].clone())
    );
    assert_eq!(
        restored_product.catalog_describe(&snapshot, ObjectId::new(13)?)?,
        Some(objects[3].clone())
    );
    drop(restored_product);
    fs::remove_dir_all(root)?;
    Ok(())
}
