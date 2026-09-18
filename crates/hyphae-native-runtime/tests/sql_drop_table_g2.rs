// SPDX-License-Identifier: Apache-2.0

//! Strict DROP TABLE RESTRICT vertical.

use hyphae_native_catalog::{
    CatalogName, CatalogObjectV2, DefinitionVersion, KeyspaceDefinition, KeyspaceEvictionPolicy,
    KeyspaceMemoryClass, KeyspaceTtlPolicy, LogicalCatalogObject, ObjectHeaderV2, QualifiedName,
    StructureKind, StructureOwnership,
};
use hyphae_native_runtime::{NativeDatabase, NativeRuntimeError, SqlError, SqlResult, SqlValue};
use hyphae_native_types::{DurabilityClass, EngineKind, LogicalType, ObjectId};

fn logical_header(
    id: ObjectId,
    owner: EngineKind,
    name: &str,
    parent: Option<ObjectId>,
) -> Result<ObjectHeaderV2, Box<dyn std::error::Error>> {
    Ok(ObjectHeaderV2 {
        id,
        owner,
        name: QualifiedName::new(
            CatalogName::unquoted("main")?,
            CatalogName::unquoted("public")?,
            CatalogName::unquoted(name)?,
        ),
        parent,
        definition_version: DefinitionVersion::FIRST,
    })
}

#[test]
fn drop_table_is_restrictive_invalidates_plans_and_survives_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!("hyphae-drop-table-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut tx = database.begin_sql(1, DurabilityClass::Strict)?;
    let SqlResult::Command {
        object_id: Some(table),
        ..
    } = tx.execute_sql(
        "CREATE TABLE people (id BIGINT PRIMARY KEY, email TEXT)",
        &[],
    )?
    else {
        return Err("CREATE TABLE did not return an object ID".into());
    };
    tx.execute_sql("CREATE INDEX people_email ON people (email)", &[])?;
    tx.execute_sql(
        "INSERT INTO people (id, email) VALUES (1, 'a@example.com')",
        &[],
    )?;
    tx.commit()?;
    let prepared = database.prepare_sql_latest("SELECT email FROM people WHERE id = 1")?;
    let mut blocked = database.begin_sql(2, DurabilityClass::Memory)?;
    let mutation_count = blocked.mutation_count();
    assert!(matches!(
        blocked.execute_sql("DROP TABLE people", &[]),
        Err(SqlError::Runtime(
            NativeRuntimeError::CatalogDependencyConflict { object }
        )) if object == table
    ));
    assert_eq!(blocked.mutation_count(), mutation_count);
    assert_eq!(
        blocked.execute_sql("SELECT id FROM people WHERE email = 'a@example.com'", &[],)?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: vec![vec![hyphae_native_runtime::SqlValue::Signed(1)]],
        }
    );
    blocked.rollback();
    let mut ddl = database.begin_sql(3, DurabilityClass::Strict)?;
    ddl.execute_sql("DROP INDEX people_email", &[])?;
    ddl.execute_sql("DROP TABLE people", &[])?;
    ddl.commit()?;
    let mut recreate = database.begin_sql(4, DurabilityClass::Strict)?;
    let SqlResult::Command {
        object_id: Some(recreated_id),
        ..
    } = recreate.execute_sql("CREATE TABLE people (id BIGINT PRIMARY KEY)", &[])?
    else {
        return Err("CREATE TABLE did not return an object ID".into());
    };
    assert!(recreated_id.get() > 2);
    recreate.commit()?;
    assert!(matches!(
        database.execute_prepared_latest(&prepared, &[]),
        Err(SqlError::CatalogChanged)
    ));
    assert!(
        database
            .prepare_sql_latest("SELECT email FROM people WHERE id = 1")
            .is_err()
    );
    drop(database);
    let reopened = NativeDatabase::open(&temporary)?;
    assert!(
        reopened
            .prepare_sql_latest("SELECT id FROM people WHERE id = 1")
            .is_ok()
    );
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn self_referencing_foreign_key_does_not_block_owning_table_drop()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary =
        std::env::temp_dir().join(format!("hyphae-drop-table-self-fk-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    seed.execute_sql(
        "CREATE TABLE nodes (id BIGINT PRIMARY KEY, parent_id BIGINT, \
         FOREIGN KEY (parent_id) REFERENCES nodes (id))",
        &[],
    )?;
    seed.execute_sql("INSERT INTO nodes (id, parent_id) VALUES (1, 1)", &[])?;
    seed.commit()?;

    let mut drop_table = database.begin_sql(2, DurabilityClass::Strict)?;
    drop_table.execute_sql("DROP TABLE nodes", &[])?;
    drop_table.commit()?;
    drop(database);

    let reopened = NativeDatabase::open(&temporary)?;
    assert!(
        reopened
            .prepare_sql_latest("SELECT id FROM nodes WHERE id = 1")
            .is_err()
    );
    drop(reopened);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn logical_relation_schema_dependency_blocks_drop_without_mutating_batch()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-drop-table-relation-schema-{}",
        std::process::id()
    ));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Strict)?;
    let SqlResult::Command {
        object_id: Some(table),
        ..
    } = transaction.execute_sql("CREATE TABLE records (id BIGINT PRIMARY KEY)", &[])?
    else {
        return Err("CREATE TABLE did not return an object ID".into());
    };
    let database_id = transaction.next_catalog_object_id()?;
    transaction.create_catalog_object_v2(LogicalCatalogObject::V2(CatalogObjectV2::Database(
        logical_header(database_id, EngineKind::Kernel, "logical_database", None)?,
    )))?;
    let schema_id = transaction.next_catalog_object_id()?;
    transaction.create_catalog_object_v2(LogicalCatalogObject::V2(CatalogObjectV2::Schema(
        logical_header(
            schema_id,
            EngineKind::Kernel,
            "logical_schema",
            Some(database_id),
        )?,
    )))?;
    let keyspace_id = transaction.next_catalog_object_id()?;
    transaction.create_catalog_object_v2(LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(
        KeyspaceDefinition {
            header: logical_header(
                keyspace_id,
                EngineKind::Structure,
                "record_values",
                Some(schema_id),
            )?,
            kind: StructureKind::String,
            key_type: LogicalType::Binary,
            value_type: LogicalType::Binary,
            ownership: StructureOwnership::Canonical,
            ttl_policy: KeyspaceTtlPolicy::Disabled,
            default_ttl_millis: None,
            memory_class: KeyspaceMemoryClass::Durable,
            eviction: KeyspaceEvictionPolicy::None,
            relation_schema: Some(table),
        },
    )))?;

    let mutation_count = transaction.mutation_count();
    assert!(matches!(
        transaction.execute_sql("DROP TABLE records", &[]),
        Err(SqlError::Runtime(
            NativeRuntimeError::CatalogDependencyConflict { object }
        )) if object == table
    ));
    assert_eq!(transaction.mutation_count(), mutation_count);
    assert!(transaction.logical_catalog_object(keyspace_id).is_some());
    transaction.execute_sql("INSERT INTO records (id) VALUES (1)", &[])?;
    transaction.commit()?;
    drop(database);

    let mut reopened = NativeDatabase::open(&temporary)?;
    let mut read = reopened.begin_sql(2, DurabilityClass::Memory)?;
    assert_eq!(
        read.execute_sql("SELECT id FROM records WHERE id = 1", &[])?,
        SqlResult::Rows {
            columns: vec!["id".to_owned()],
            rows: vec![vec![SqlValue::Signed(1)]],
        }
    );
    read.rollback();
    drop(reopened);
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
