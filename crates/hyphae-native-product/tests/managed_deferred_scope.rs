// SPDX-License-Identifier: Apache-2.0

//! Managed scope contracts for work retained beyond one request.

use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_catalog::{
    CatalogName, CatalogObject, ObjectHeader, QualifiedName, StructureDefinition,
    StructureOwnership,
};
use hyphae_native_product::{
    BuiltInRole, CustomRoleGrant, NativeProduct, ProductAuthorization, ProductDurabilityPolicy,
    ProductErrorCode, ProductExplicitTransactionStatus, ProductOperation, ProductPermission,
    ProductPreparedHandle, ProductPrincipal, ProductRequestContext, ProductResponse, ProductScope,
    ProductSession, ProductSessionId, ProductStructureKey, ProductStructureMutation,
    ProductStructureReadRequest, ProductStructureReadResult, ProductTransactionHandle,
    ProductTransactionSearchMutation, ProductTransactionSqlMutation,
    ProductTransactionVectorMutation, ProductValue, ProductVector, StructureKind,
};
use hyphae_native_runtime::{HnswConfig, NativeDatabase, SqlResult, VectorMetric};
use hyphae_native_types::{DurabilityClass, EngineKind, ObjectId};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

const STRUCTURE_ID: u128 = 10_001;
const SEARCH_ID: u128 = 10_002;
const VECTOR_ID: u128 = 10_003;

struct ManagedDeferredFixture {
    product: NativeProduct,
    directory: PathBuf,
    owner_key_path: PathBuf,
    issued_key_paths: Vec<PathBuf>,
    owner_secret: String,
    target_table: ObjectId,
    sibling_table: ObjectId,
    structure: ObjectId,
    search: ObjectId,
    vector: ObjectId,
}

struct ManagedSession {
    session: ProductSession,
    key_id: hyphae_native_product::ApiKeyId,
}

impl ManagedDeferredFixture {
    fn create(name: &str) -> Result<Self, Box<dyn Error>> {
        let directory = std::env::temp_dir().join(format!(
            "hyphae-managed-deferred-{name}-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
        ));
        let owner_key_path = directory.with_extension("owner-key");
        let _ignored = fs::remove_dir_all(&directory);
        let _ignored = fs::remove_file(&owner_key_path);

        let mut runtime = NativeDatabase::create(&directory)?;
        let mut seed = runtime.begin(0, DurabilityClass::Memory)?;
        let target_table = command_object_id(&seed.execute_sql(
            "CREATE TABLE target_rows (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
            &[],
        )?)?;
        let sibling_table = command_object_id(&seed.execute_sql(
            "CREATE TABLE sibling_rows (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
            &[],
        )?)?;
        seed.execute_sql(
            "INSERT INTO target_rows (id, body) VALUES (?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(1),
                hyphae_native_runtime::SqlValue::Text("target".to_owned()),
            ],
        )?;
        seed.execute_sql(
            "INSERT INTO sibling_rows (id, body) VALUES (?, ?)",
            &[
                hyphae_native_runtime::SqlValue::Signed(1),
                hyphae_native_runtime::SqlValue::Text("sibling".to_owned()),
            ],
        )?;

        let structure = ObjectId::new(STRUCTURE_ID)?;
        seed.create_catalog_object_v2(hyphae_native_product::LogicalCatalogObject::from_legacy(
            string_keyspace(structure)?,
        ))?;
        let search = ObjectId::new(SEARCH_ID)?;
        seed.create_search_index(search, "deferred_search")?;
        let vector = ObjectId::new(VECTOR_ID)?;
        seed.create_vector_index(
            vector,
            "deferred_vectors",
            2,
            VectorMetric::Cosine,
            HnswConfig::new(4, 16, 8, 32, 7)?,
        )?;
        seed.commit()?;
        drop(runtime);

        let mut product = NativeProduct::open_with_preview_default_scalar_migration(&directory)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_key_path, 1)?;
        let owner_secret = fs::read_to_string(&owner_key_path)?;
        Ok(Self {
            product,
            directory,
            owner_key_path,
            issued_key_paths: Vec::new(),
            owner_secret,
            target_table,
            sibling_table,
            structure,
            search,
            vector,
        })
    }

    fn managed_session(
        &mut self,
        label: &str,
        scopes: &[ProductScope],
        session_id: u128,
    ) -> Result<ManagedSession, Box<dyn Error>> {
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let principal = self.product.create_security_principal(&owner, label, 2)?;
        for (offset, scope) in scopes.iter().copied().enumerate() {
            let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
            self.product.assign_built_in_role(
                &owner,
                principal.principal_id,
                BuiltInRole::Developer,
                scope,
                3 + i64::try_from(offset)?,
            )?;
        }
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product
            .set_security_principal_enabled(&owner, principal.principal_id, true, 19)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let key_path = self.directory.with_extension(format!("{label}-key"));
        let _ignored = fs::remove_file(&key_path);
        self.product.issue_scoped_api_key_to_file(
            &owner,
            principal.principal_id,
            label,
            [BuiltInRole::Developer],
            [],
            BuiltInRole::Developer.authorization(),
            scopes.iter().copied(),
            None,
            &key_path,
            20,
        )?;
        let secret = fs::read_to_string(&key_path)?;
        let authority = self.product.authenticate_api_key(&secret, 0)?;
        let key_id = authority.key_id();
        self.issued_key_paths.push(key_path);
        Ok(ManagedSession {
            session: ProductSession::new_authenticated(
                ProductSessionId::new(session_id).ok_or("zero session ID")?,
                authority,
            ),
            key_id,
        })
    }

    fn heterogeneous_transaction_session(
        &mut self,
        label: &str,
        session_id: u128,
    ) -> Result<ManagedSession, Box<dyn Error>> {
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let principal = self.product.create_security_principal(&owner, label, 40)?;

        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let structure_writer = self.product.create_custom_security_role(
            &owner,
            "deferred structure writer",
            [CustomRoleGrant::new(
                ProductPermission::DataWrite,
                ProductScope::CatalogObject(self.structure),
            )
            .ok_or("invalid structure writer grant")?],
            41,
        )?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product.assign_custom_security_role(
            &owner,
            principal.principal_id,
            structure_writer.role_id,
            42,
        )?;

        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let search_writer = self.product.create_custom_security_role(
            &owner,
            "deferred search writer",
            [
                CustomRoleGrant::new(
                    ProductPermission::CatalogRead,
                    ProductScope::CatalogObject(self.search),
                )
                .ok_or("invalid search catalog grant")?,
                CustomRoleGrant::new(
                    ProductPermission::DataWrite,
                    ProductScope::CatalogObject(self.search),
                )
                .ok_or("invalid search writer grant")?,
            ],
            43,
        )?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product.assign_custom_security_role(
            &owner,
            principal.principal_id,
            search_writer.role_id,
            44,
        )?;

        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product
            .set_security_principal_enabled(&owner, principal.principal_id, true, 45)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let key_path = self.directory.with_extension(format!("{label}-key"));
        let _ignored = fs::remove_file(&key_path);
        self.product.issue_scoped_api_key_to_file(
            &owner,
            principal.principal_id,
            label,
            [],
            [structure_writer.role_id, search_writer.role_id],
            ProductAuthorization::from_permissions([
                ProductPermission::CatalogRead,
                ProductPermission::DataWrite,
            ]),
            [
                ProductScope::CatalogObject(self.structure),
                ProductScope::CatalogObject(self.search),
            ],
            None,
            &key_path,
            46,
        )?;
        let secret = fs::read_to_string(&key_path)?;
        let authority = self.product.authenticate_api_key(&secret, 0)?;
        let structure_authorization = authority
            .scoped_authorization()
            .iter()
            .find(|grant| grant.scope == ProductScope::CatalogObject(self.structure))
            .ok_or("missing structure authorization")?
            .authorization;
        assert!(structure_authorization.allows(ProductPermission::DataWrite));
        assert!(!structure_authorization.allows(ProductPermission::CatalogRead));
        let search_authorization = authority
            .scoped_authorization()
            .iter()
            .find(|grant| grant.scope == ProductScope::CatalogObject(self.search))
            .ok_or("missing search authorization")?
            .authorization;
        assert!(search_authorization.allows(ProductPermission::CatalogRead));
        assert!(search_authorization.allows(ProductPermission::DataWrite));
        let key_id = authority.key_id();
        self.issued_key_paths.push(key_path);
        Ok(ManagedSession {
            session: ProductSession::new_authenticated(
                ProductSessionId::new(session_id).ok_or("zero session ID")?,
                authority,
            ),
            key_id,
        })
    }
}

impl Drop for ManagedDeferredFixture {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.directory);
        let _ignored = fs::remove_file(&self.owner_key_path);
        for path in &self.issued_key_paths {
            let _ignored = fs::remove_file(path);
        }
    }
}

fn command_object_id(result: &SqlResult) -> Result<ObjectId, Box<dyn Error>> {
    match result {
        SqlResult::Command {
            object_id: Some(object_id),
            ..
        } => Ok(*object_id),
        _ => Err("SQL catalog mutation did not return an object ID".into()),
    }
}

fn string_keyspace(id: ObjectId) -> Result<CatalogObject, Box<dyn Error>> {
    Ok(CatalogObject::Structure(StructureDefinition {
        header: ObjectHeader {
            id,
            owner: EngineKind::Structure,
            name: QualifiedName::new(
                CatalogName::unquoted("main")?,
                CatalogName::unquoted("scope_tests")?,
                CatalogName::unquoted("deferred_strings")?,
            ),
        },
        kind: StructureKind::String,
        key_type: hyphae_native_types::LogicalType::Binary,
        value_type: hyphae_native_types::LogicalType::Binary,
        ownership: StructureOwnership::Canonical,
        ttl_enabled: true,
    }))
}

fn context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    let mut context = ProductRequestContext::new(
        request_id,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    )
    .with_authorization_epoch(session.authorization_epoch());
    context.durability = ProductDurabilityPolicy::MEMORY;
    context
}

fn prepare(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
    table: &str,
) -> Result<ProductPreparedHandle, Box<dyn Error>> {
    let response = product.dispatch(
        session,
        &context(session, request_id),
        ProductOperation::PrepareSql {
            statement: format!("SELECT body FROM {table} WHERE id = ?"),
        },
    )?;
    let ProductResponse::PreparedSql { handle, .. } = response else {
        return Err("prepare did not return a handle".into());
    };
    Ok(handle)
}

fn assert_prepared_lifecycle(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
) -> Result<(), Box<dyn Error>> {
    let handle = prepare(product, session, request_id, "target_rows")?;
    let response = product.dispatch(
        session,
        &context(session, request_id + 1),
        ProductOperation::ExecutePrepared {
            handle,
            parameters: vec![ProductValue::Signed(1)],
        },
    )?;
    assert!(matches!(response, ProductResponse::Sql { .. }));
    assert_eq!(
        product.dispatch(
            session,
            &context(session, request_id + 2),
            ProductOperation::DeallocatePrepared { handle },
        )?,
        ProductResponse::Deallocated
    );
    Ok(())
}

fn assert_sibling_prepare_denied(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
) -> Result<(), Box<dyn Error>> {
    let Err(error) = product.dispatch(
        session,
        &context(session, request_id),
        ProductOperation::PrepareSql {
            statement: "SELECT body FROM sibling_rows WHERE id = ?".to_owned(),
        },
    ) else {
        return Err("sibling prepared plan was retained".into());
    };
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    Ok(())
}

fn begin_transaction(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
) -> Result<ProductTransactionHandle, Box<dyn Error>> {
    let response = product.dispatch(
        session,
        &context(session, request_id),
        ProductOperation::TransactionBegin,
    )?;
    let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
        handle,
        ..
    }) = response
    else {
        return Err("transaction did not begin".into());
    };
    Ok(handle)
}

fn stage_every_object(
    fixture: &mut ManagedDeferredFixture,
    session: &mut ProductSession,
    handle: ProductTransactionHandle,
    request_id: u128,
    suffix: &str,
) -> Result<(), Box<dyn Error>> {
    let operations = [
        ProductOperation::TransactionStageStructure {
            handle,
            mutation: ProductStructureMutation::StringSet {
                key: ProductStructureKey {
                    keyspace: fixture.structure,
                    key: format!("structure-{suffix}").into_bytes(),
                },
                value: b"staged".to_vec(),
                expires_at_micros: None,
            },
        },
        ProductOperation::TransactionStageSearch {
            handle,
            mutation: ProductTransactionSearchMutation::Index {
                index: fixture.search,
                document_id: format!("document-{suffix}").into_bytes(),
                text: "deferred authorization".to_owned(),
            },
        },
        ProductOperation::TransactionStageVector {
            handle,
            mutation: ProductTransactionVectorMutation::Upsert {
                index: fixture.vector,
                object_id: ObjectId::new(if suffix == "commit" { 20_001 } else { 20_002 })?,
                vector: ProductVector::new([1.0, 0.0])?,
            },
        },
    ];
    for (offset, operation) in operations.into_iter().enumerate() {
        let response = fixture.product.dispatch(
            session,
            &context(session, request_id + u128::try_from(offset)?),
            operation,
        )?;
        assert!(matches!(response, ProductResponse::TransactionStaged(_)));
    }
    Ok(())
}

#[test]
fn managed_object_scope_reauthorizes_prepared_lifecycle_and_denies_sibling()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedDeferredFixture::create("prepared-object")?;
    assert_ne!(fixture.target_table, fixture.sibling_table);
    let mut managed = fixture.managed_session(
        "prepared-object",
        &[ProductScope::CatalogObject(fixture.target_table)],
        1,
    )?;

    assert_prepared_lifecycle(&mut fixture.product, &mut managed.session, 1)?;
    assert_sibling_prepare_denied(&mut fixture.product, &mut managed.session, 4)?;
    Ok(())
}

#[test]
fn managed_subtree_scope_reauthorizes_prepared_lifecycle_and_denies_sibling_tree()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedDeferredFixture::create("prepared-subtree")?;
    assert_ne!(fixture.target_table, fixture.sibling_table);
    let mut managed = fixture.managed_session(
        "prepared-subtree",
        &[ProductScope::CatalogSubtree(fixture.target_table)],
        2,
    )?;

    assert_prepared_lifecycle(&mut fixture.product, &mut managed.session, 1)?;
    assert_sibling_prepare_denied(&mut fixture.product, &mut managed.session, 4)?;
    Ok(())
}

#[test]
fn managed_transaction_reauthorizes_staged_union_and_rolls_back_after_revocation()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedDeferredFixture::create("transaction-union")?;
    let scopes = [
        ProductScope::CatalogObject(fixture.structure),
        ProductScope::CatalogObject(fixture.search),
        ProductScope::CatalogObject(fixture.vector),
    ];

    let mut admitted = fixture.managed_session("union-admitted", &scopes, 3)?;
    let admitted_handle = begin_transaction(&mut fixture.product, &mut admitted.session, 1)?;
    stage_every_object(
        &mut fixture,
        &mut admitted.session,
        admitted_handle,
        2,
        "commit",
    )?;
    let commit_context = context(&admitted.session, 5);
    let response = fixture.product.dispatch(
        &mut admitted.session,
        &commit_context,
        ProductOperation::TransactionCommit {
            handle: admitted_handle,
        },
    )?;
    assert!(matches!(response, ProductResponse::TransactionCommitted(_)));

    let mut revoked = fixture.managed_session("union-revoked", &scopes, 4)?;
    let revoked_handle = begin_transaction(&mut fixture.product, &mut revoked.session, 10)?;
    stage_every_object(
        &mut fixture,
        &mut revoked.session,
        revoked_handle,
        11,
        "rollback",
    )?;
    let owner = fixture
        .product
        .authenticate_api_key(&fixture.owner_secret, 0)?;
    fixture.product.revoke_api_key(&owner, revoked.key_id, 30)?;
    let commit_context = context(&revoked.session, 14);
    let Err(error) = fixture.product.dispatch(
        &mut revoked.session,
        &commit_context,
        ProductOperation::TransactionCommit {
            handle: revoked_handle,
        },
    ) else {
        return Err("revoked authority committed the staged union".into());
    };
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);

    let mut local = ProductSession::new(
        ProductSessionId::new(5).ok_or("zero session ID")?,
        ProductPrincipal::new("rollback-observer").ok_or("invalid principal")?,
        ProductAuthorization::ALL,
    );
    let read_context = context(&local, 20);
    let response = fixture.product.dispatch(
        &mut local,
        &read_context,
        ProductOperation::StructureRead(ProductStructureReadRequest::StringGet {
            key: ProductStructureKey {
                keyspace: fixture.structure,
                key: b"structure-rollback".to_vec(),
            },
        }),
    )?;
    let ProductResponse::StructureRead(read) = response else {
        return Err("rollback observation did not return a structure read".into());
    };
    assert_eq!(read.value, ProductStructureReadResult::Value(None));
    Ok(())
}

#[test]
fn managed_transaction_preserves_heterogeneous_permissions_per_object() -> Result<(), Box<dyn Error>>
{
    let mut fixture = ManagedDeferredFixture::create("heterogeneous-transaction")?;
    let mut managed = fixture.heterogeneous_transaction_session("heterogeneous", 6)?;
    let handle = begin_transaction(&mut fixture.product, &mut managed.session, 1)?;

    for (request_id, operation) in [
        (
            2,
            ProductOperation::TransactionStageStructure {
                handle,
                mutation: ProductStructureMutation::StringSet {
                    key: ProductStructureKey {
                        keyspace: fixture.structure,
                        key: b"heterogeneous".to_vec(),
                    },
                    value: b"committed".to_vec(),
                    expires_at_micros: None,
                },
            },
        ),
        (
            3,
            ProductOperation::TransactionStageSearch {
                handle,
                mutation: ProductTransactionSearchMutation::Index {
                    index: fixture.search,
                    document_id: b"heterogeneous".to_vec(),
                    text: "permission object pair".to_owned(),
                },
            },
        ),
    ] {
        let request_context = context(&managed.session, request_id);
        let response =
            fixture
                .product
                .dispatch(&mut managed.session, &request_context, operation)?;
        assert!(matches!(response, ProductResponse::TransactionStaged(_)));
    }

    let commit_context = context(&managed.session, 4);
    let response = fixture.product.dispatch(
        &mut managed.session,
        &commit_context,
        ProductOperation::TransactionCommit { handle },
    )?;
    let ProductResponse::TransactionCommitted(receipt) = response else {
        return Err("heterogeneous transaction did not commit".into());
    };
    assert_eq!(receipt.staged_operations, 2);
    Ok(())
}

#[test]
fn managed_transaction_sql_stage_retains_bound_target_through_commit_and_revocation()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedDeferredFixture::create("sql-stage-union")?;
    let scopes = [ProductScope::CatalogObject(fixture.target_table)];

    let mut admitted = fixture.managed_session("sql-stage-admitted", &scopes, 7)?;
    let handle = begin_transaction(&mut fixture.product, &mut admitted.session, 1)?;
    let request = context(&admitted.session, 2);
    let response = fixture.product.dispatch(
        &mut admitted.session,
        &request,
        ProductOperation::TransactionStageSql {
            handle,
            mutation: ProductTransactionSqlMutation {
                statement: "UPDATE target_rows SET body = ? WHERE id = ?".to_owned(),
                parameters: vec![
                    ProductValue::Text("staged".to_owned()),
                    ProductValue::Signed(1),
                ],
            },
        },
    )?;
    assert!(matches!(response, ProductResponse::TransactionStaged(_)));
    let request = context(&admitted.session, 3);
    let response = fixture.product.dispatch(
        &mut admitted.session,
        &request,
        ProductOperation::TransactionCommit { handle },
    )?;
    assert!(matches!(response, ProductResponse::TransactionCommitted(_)));

    let mut revoked = fixture.managed_session("sql-stage-revoked", &scopes, 8)?;
    let handle = begin_transaction(&mut fixture.product, &mut revoked.session, 10)?;
    let request = context(&revoked.session, 11);
    fixture.product.dispatch(
        &mut revoked.session,
        &request,
        ProductOperation::TransactionStageSql {
            handle,
            mutation: ProductTransactionSqlMutation {
                statement: "UPDATE target_rows SET body = ? WHERE id = ?".to_owned(),
                parameters: vec![
                    ProductValue::Text("revoked".to_owned()),
                    ProductValue::Signed(1),
                ],
            },
        },
    )?;
    let owner = fixture
        .product
        .authenticate_api_key(&fixture.owner_secret, 0)?;
    fixture.product.revoke_api_key(&owner, revoked.key_id, 30)?;
    let request = context(&revoked.session, 12);
    let Err(error) = fixture.product.dispatch(
        &mut revoked.session,
        &request,
        ProductOperation::TransactionCommit { handle },
    ) else {
        return Err("revoked SQL transaction committed".into());
    };
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    Ok(())
}
