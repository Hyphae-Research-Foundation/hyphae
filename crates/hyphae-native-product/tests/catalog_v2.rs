// SPDX-License-Identifier: AGPL-3.0-only

//! Focused logical catalog V2 persistence and bounded product API coverage.

use std::{fs, path::PathBuf};

use hyphae_native_catalog::{
    CatalogName, CatalogObjectV2, DefinitionVersion, DependencyDirection, LogicalCatalogObject,
    ObjectHeaderV2, QualifiedName,
};
use hyphae_native_product::{
    CatalogDependencyRequest, CatalogListRequest, NativeProduct, ProductDurability,
    ProductErrorCategory, ProductErrorCode,
};
use hyphae_native_types::{EngineKind, ObjectId};

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

#[test]
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
    let first = product.catalog_list(
        &snapshot,
        CatalogListRequest {
            parent: None,
            kind: None,
            cursor: None,
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
            parent: None,
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
    assert_eq!(product.capabilities().catalog_tree_format_version, 6);

    drop(product);
    fs::remove_dir_all(path)?;
    Ok(())
}
