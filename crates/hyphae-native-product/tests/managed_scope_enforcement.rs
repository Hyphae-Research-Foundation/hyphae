// SPDX-License-Identifier: Apache-2.0

//! Managed API-key scope admission across operations carrying explicit object IDs.

use std::{collections::BTreeMap, error::Error, fs, path::PathBuf};

use hyphae_native_catalog::{
    CatalogName, CatalogObjectKind, CatalogObjectV2, DefinitionVersion, DependencyDirection,
    LogicalCatalogObject, ObjectHeaderV2, QualifiedName,
};
use hyphae_native_product::proof::NativeProofGenerationLimits;
use hyphae_native_product::{
    BoundedSearchQuery, BuiltInRole, CatalogCursor, CatalogDependencyRequest, CatalogListRequest,
    CatalogVisibleListFilter, CatalogVisibleListRequest, NativeProduct, ProductAuthorization,
    ProductDocument, ProductDurability, ProductError, ProductErrorCode, ProductLexicalBranch,
    ProductOperation, ProductPermission, ProductPrincipal, ProductRequestContext, ProductResponse,
    ProductScope, ProductSearchDocumentDelete, ProductSearchDocumentUpdate, ProductSearchFilter,
    ProductSearchIngestBatch, ProductSearchRequest, ProductSession, ProductSessionId,
    ProductSetAlgebraOperation, ProductSqlResult, ProductStructureKey, ProductStructureMutation,
    ProductStructureReadRequest, ProductValue,
};
use hyphae_native_runtime::CatalogPageStop;
use hyphae_native_types::{EngineKind, ObjectId};

const DATABASE_A: u128 = 100;
const TARGET_OBJECT: u128 = 101;
const DATABASE_B: u128 = 200;
const SIBLING_OBJECT: u128 = 201;

struct ManagedFixture {
    product: NativeProduct,
    directory: PathBuf,
    owner_key: PathBuf,
    issued_keys: Vec<PathBuf>,
    owner_secret: String,
}

impl ManagedFixture {
    fn create(name: &str) -> Result<Self, Box<dyn Error>> {
        let directory = std::env::temp_dir().join(format!(
            "hyphae-managed-scope-{name}-{}",
            std::process::id()
        ));
        let owner_key = directory.with_extension("owner-key");
        let _ignored = fs::remove_dir_all(&directory);
        let _ignored = fs::remove_file(&owner_key);
        let mut product = NativeProduct::create(&directory)?;
        for object in catalog_tree()? {
            product.create_catalog_object_v2(object, ProductDurability::Strict)?;
        }
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_key, 1)?;
        let owner_secret = fs::read_to_string(&owner_key)?;
        Ok(Self {
            product,
            directory,
            owner_key,
            issued_keys: Vec::new(),
            owner_secret,
        })
    }

    fn developer_session(
        &mut self,
        label: &str,
        scope: ProductScope,
        session_id: u128,
    ) -> Result<ProductSession, Box<dyn Error>> {
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let principal = self.product.create_security_principal(&owner, label, 10)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product.assign_built_in_role(
            &owner,
            principal.principal_id,
            BuiltInRole::Developer,
            scope,
            11,
        )?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product
            .set_security_principal_enabled(&owner, principal.principal_id, true, 12)?;
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
            [scope],
            None,
            &key_path,
            13,
        )?;
        let secret = fs::read_to_string(&key_path)?;
        let authority = self.product.authenticate_api_key(&secret, 0)?;
        self.issued_keys.push(key_path);
        Ok(ProductSession::new_authenticated(
            ProductSessionId::new(session_id).ok_or("zero session ID")?,
            authority,
        ))
    }

    fn narrowed_developer_session(
        &mut self,
        label: &str,
        ceiling: ProductScope,
        session_id: u128,
    ) -> Result<ProductSession, Box<dyn Error>> {
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let principal = self.product.create_security_principal(&owner, label, 10)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product.assign_built_in_role(
            &owner,
            principal.principal_id,
            BuiltInRole::Developer,
            ProductScope::Instance,
            11,
        )?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product
            .set_security_principal_enabled(&owner, principal.principal_id, true, 12)?;
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
            [ceiling],
            None,
            &key_path,
            13,
        )?;
        let secret = fs::read_to_string(&key_path)?;
        let authority = self.product.authenticate_api_key(&secret, 0)?;
        self.issued_keys.push(key_path);
        Ok(ProductSession::new_authenticated(
            ProductSessionId::new(session_id).ok_or("zero session ID")?,
            authority,
        ))
    }

    fn create_sql_tables(&mut self) -> Result<(ObjectId, ObjectId), Box<dyn Error>> {
        let mut session = ProductSession::new(
            ProductSessionId::new(90).ok_or("zero fixture session ID")?,
            ProductPrincipal::new("scope fixture").ok_or("invalid fixture principal")?,
            ProductAuthorization::ALL,
        );
        let target = create_sql_table(&mut self.product, &mut session, 90, "scope_target_rows")?;
        let sibling = create_sql_table(&mut self.product, &mut session, 91, "scope_sibling_rows")?;
        Ok((target, sibling))
    }

    fn default_scalar_keyspace_id(&self) -> Result<ObjectId, Box<dyn Error>> {
        let name = QualifiedName::new(
            CatalogName::unquoted("hyphae_internal")?,
            CatalogName::unquoted("system")?,
            CatalogName::unquoted("default_scalar")?,
        );
        self.product
            .catalog_resolve(&self.product.catalog_snapshot()?, &name)?
            .map(|object| object.id())
            .ok_or_else(|| "default scalar keyspace is missing".into())
    }

    fn scoped_session(
        &mut self,
        label: &str,
        permissions: &[ProductPermission],
        scopes: &[ProductScope],
        session_id: u128,
    ) -> Result<ProductSession, Box<dyn Error>> {
        let grants = permissions
            .iter()
            .flat_map(|permission| {
                scopes.iter().map(|scope| {
                    hyphae_native_product::CustomRoleGrant::new(*permission, *scope)
                        .ok_or("invalid scoped SQL grant")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.grant_session(label, &grants, permissions, scopes, session_id)
    }

    fn grant_session(
        &mut self,
        label: &str,
        grants: &[hyphae_native_product::CustomRoleGrant],
        permissions: &[ProductPermission],
        scopes: &[ProductScope],
        session_id: u128,
    ) -> Result<ProductSession, Box<dyn Error>> {
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let principal = self.product.create_security_principal(&owner, label, 50)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let role =
            self.product
                .create_custom_security_role(&owner, label, grants.iter().copied(), 51)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product.assign_custom_security_role(
            &owner,
            principal.principal_id,
            role.role_id,
            52,
        )?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product
            .set_security_principal_enabled(&owner, principal.principal_id, true, 53)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let key_path = self.directory.with_extension(format!("{label}-key"));
        let _ignored = fs::remove_file(&key_path);
        self.product.issue_scoped_api_key_to_file(
            &owner,
            principal.principal_id,
            label,
            [],
            [role.role_id],
            ProductAuthorization::from_permissions(permissions.iter().copied()),
            scopes.iter().copied(),
            None,
            &key_path,
            54,
        )?;
        let secret = fs::read_to_string(&key_path)?;
        let authority = self.product.authenticate_api_key(&secret, 0)?;
        self.issued_keys.push(key_path);
        Ok(ProductSession::new_authenticated(
            ProductSessionId::new(session_id).ok_or("zero session ID")?,
            authority,
        ))
    }
}

impl Drop for ManagedFixture {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.directory);
        let _ignored = fs::remove_file(&self.owner_key);
        for path in &self.issued_keys {
            let _ignored = fs::remove_file(path);
        }
    }
}

fn catalog_tree() -> Result<Vec<LogicalCatalogObject>, Box<dyn Error>> {
    Ok(vec![
        logical_database(DATABASE_A, "database_a")?,
        logical_schema(TARGET_OBJECT, "schema_a", DATABASE_A)?,
        logical_database(DATABASE_B, "database_b")?,
        logical_schema(SIBLING_OBJECT, "schema_b", DATABASE_B)?,
    ])
}

fn logical_database(id: u128, name: &str) -> Result<LogicalCatalogObject, Box<dyn Error>> {
    Ok(LogicalCatalogObject::V2(CatalogObjectV2::Database(header(
        id, name, None,
    )?)))
}

fn logical_schema(
    id: u128,
    name: &str,
    parent: u128,
) -> Result<LogicalCatalogObject, Box<dyn Error>> {
    Ok(LogicalCatalogObject::V2(CatalogObjectV2::Schema(header(
        id,
        name,
        Some(parent),
    )?)))
}

fn header(id: u128, name: &str, parent: Option<u128>) -> Result<ObjectHeaderV2, Box<dyn Error>> {
    Ok(ObjectHeaderV2 {
        id: ObjectId::new(id)?,
        owner: EngineKind::Kernel,
        name: QualifiedName::new(
            CatalogName::unquoted("main")?,
            CatalogName::unquoted("scope_tests")?,
            CatalogName::unquoted(name)?,
        ),
        parent: parent.map(ObjectId::new).transpose()?,
        definition_version: DefinitionVersion::FIRST,
    })
}

fn context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    )
    .with_authorization_epoch(session.authorization_epoch())
}

fn create_sql_table(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
    table: &str,
) -> Result<ObjectId, Box<dyn Error>> {
    let response = product.dispatch(
        session,
        &context(session, request_id),
        ProductOperation::ExecuteSql {
            statement: format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, body TEXT NOT NULL)"),
            parameters: Vec::new(),
        },
    )?;
    let ProductResponse::Sql {
        result:
            ProductSqlResult::Command {
                object_id: Some(object_id),
                ..
            },
        ..
    } = response
    else {
        return Err("SQL catalog mutation did not return an object ID".into());
    };
    Ok(object_id)
}

fn operations_for(
    object: ObjectId,
    suffix: &str,
) -> Result<Vec<(&'static str, ProductOperation)>, Box<dyn Error>> {
    let structure_key = ProductStructureKey {
        keyspace: object,
        key: format!("set-{suffix}").into_bytes(),
    };
    let document = ProductDocument {
        object_id: object,
        text: "scope admission".into(),
        doc_values: BTreeMap::new(),
        vectors: BTreeMap::new(),
    };
    let mut operations = vec![
        (
            "catalog object",
            ProductOperation::CatalogObject { id: object },
        ),
        (
            "catalog describe",
            ProductOperation::CatalogDescribe { id: object },
        ),
        (
            "catalog create parent",
            ProductOperation::CatalogCreate {
                object: logical_schema(
                    if suffix == "target" { 901 } else { 902 },
                    &format!("created_{suffix}"),
                    object.get(),
                )?,
            },
        ),
        (
            "structure keyspace mutation",
            ProductOperation::StructureMutate {
                mutations: vec![ProductStructureMutation::SetAdd {
                    key: structure_key,
                    member: b"member".to_vec(),
                }],
            },
        ),
        (
            "structure set algebra",
            ProductOperation::StructureRead(ProductStructureReadRequest::SetAlgebra {
                keyspace: object,
                operation: ProductSetAlgebraOperation::Union,
                keys: vec![format!("set-{suffix}").into_bytes()],
                output_member_limit: 8,
                visit_limit: 8,
            }),
        ),
    ];
    operations.extend(search_operations(object, document));
    Ok(operations)
}

fn search_operations(
    object: ObjectId,
    document: ProductDocument,
) -> Vec<(&'static str, ProductOperation)> {
    vec![
        (
            "raw search",
            ProductOperation::Search {
                index: object,
                query: BoundedSearchQuery::Term("scope".into()),
                limit: 8,
            },
        ),
        (
            "search collection",
            ProductOperation::SearchCollection {
                collection: object,
                request: search_request(),
            },
        ),
        (
            "search ingest",
            ProductOperation::SearchIngest {
                collection: object,
                batch: ProductSearchIngestBatch {
                    idempotency_id: object.get(),
                    documents: vec![document.clone()],
                },
            },
        ),
        (
            "search update",
            ProductOperation::SearchDocumentUpdate {
                collection: object,
                update: ProductSearchDocumentUpdate {
                    idempotency_id: object.get() + 10,
                    document,
                },
            },
        ),
        (
            "search delete",
            ProductOperation::SearchDocumentDelete {
                collection: object,
                delete: ProductSearchDocumentDelete {
                    idempotency_id: object.get() + 20,
                    object_id: object,
                },
            },
        ),
        (
            "proof generation",
            ProductOperation::Prove {
                operation: Box::new(ProductOperation::CatalogDescribe { id: object }),
                limits: NativeProofGenerationLimits::default(),
            },
        ),
    ]
}

fn search_request() -> ProductSearchRequest {
    ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: "scope".into(),
            candidate_limit: 8,
            weight: 1,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        aggregations: Vec::new(),
        limit: 8,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
    }
}

fn record_expected_admission(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: &mut u128,
    label: &str,
    operation: ProductOperation,
    failures: &mut Vec<String>,
) {
    let request = context(session, *request_id);
    *request_id += 1;
    if let Err(error) = product.dispatch(session, &request, operation)
        && error.code() == ProductErrorCode::AuthorizationDenied
    {
        failures.push(format!("{label}: unexpectedly denied"));
    }
}

fn record_expected_denial(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: &mut u128,
    label: &str,
    operation: ProductOperation,
    failures: &mut Vec<String>,
) {
    let request = context(session, *request_id);
    *request_id += 1;
    match product.dispatch(session, &request, operation) {
        Err(error) if error.code() == ProductErrorCode::AuthorizationDenied => {}
        Err(error) => failures.push(format!(
            "{label}: reached execution instead of denial ({:?})",
            error.code()
        )),
        Ok(_) => failures.push(format!("{label}: unexpectedly succeeded")),
    }
}

fn assert_no_scope_failures(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "managed scope admission mismatches:\n{}",
        failures.join("\n")
    );
}

fn catalog_children(parent: ObjectId) -> ProductOperation {
    ProductOperation::CatalogList(CatalogListRequest {
        parent: Some(parent),
        kind: None,
        cursor: None,
        item_limit: 8,
        visit_limit: 8,
        byte_limit: 4_096,
    })
}

fn catalog_visible(
    cursor: Option<hyphae_native_product::CatalogVisibleCursor>,
) -> ProductOperation {
    ProductOperation::CatalogVisibleList(CatalogVisibleListRequest {
        filter: CatalogVisibleListFilter {
            parent: None,
            kind: None,
        },
        cursor,
        item_limit: 2,
        visit_limit: 8,
        byte_limit: 4_096,
    })
}

fn dispatch_error(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
    operation: ProductOperation,
) -> Result<ProductError, Box<dyn Error>> {
    match product.dispatch(session, &context(session, request_id), operation) {
        Err(error) => Ok(error),
        Ok(_) => Err("operation unexpectedly succeeded".into()),
    }
}

fn assert_uniform_authorization_errors(errors: &[ProductError]) -> Result<(), Box<dyn Error>> {
    let Some(first) = errors.first() else {
        return Err("at least one error is required".into());
    };
    for error in errors {
        assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
        assert_eq!(error.category(), first.category());
        assert_eq!(error.retry(), first.retry());
        assert_eq!(error.message(), first.message());
        assert_eq!(error.object_id(), None);
        assert_eq!(error.source_span(), None);
        assert_eq!(error.details(), first.details());
    }
    Ok(())
}

fn self_issue(
    fixture: &mut ManagedFixture,
    session: &mut ProductSession,
    request_id: u128,
    scopes: Vec<ProductScope>,
) -> Result<ProductResponse, Box<ProductError>> {
    let principal_id = session
        .principal()
        .identity()
        .parse()
        .map_err(|_| Box::new(ProductError::from_code(ProductErrorCode::InvalidRequest)))?;
    let mut request = context(session, request_id).with_idempotency_token(request_id);
    request.durability = hyphae_native_product::ProductDurabilityPolicy::STRICT;
    fixture
        .product
        .dispatch(
            session,
            &request,
            ProductOperation::SecurityApiKeyIssueSelfStart {
                principal_id,
                label: format!("self-scope-{request_id}"),
                roles: vec![BuiltInRole::Developer],
                custom_roles: Vec::new(),
                permission_ceiling: BuiltInRole::Developer.authorization(),
                scope_ceiling: scopes,
                expires_at_micros: None,
            },
        )
        .map_err(Box::new)
}

#[test]
fn self_issue_scope_hierarchy_uses_stable_catalog_ancestry() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("self-issue-hierarchy")?;
    let parent = ObjectId::new(DATABASE_A)?;
    let child = ObjectId::new(TARGET_OBJECT)?;
    let sibling = ObjectId::new(SIBLING_OBJECT)?;
    let mut session = fixture.narrowed_developer_session(
        "self issue subtree",
        ProductScope::CatalogSubtree(parent),
        301,
    )?;

    for (request_id, scopes) in [
        (1, vec![ProductScope::CatalogObject(child)]),
        (2, vec![ProductScope::CatalogSubtree(child)]),
        (
            3,
            vec![
                ProductScope::CatalogObject(parent),
                ProductScope::CatalogObject(child),
            ],
        ),
    ] {
        let response = self_issue(&mut fixture, &mut session, request_id, scopes)?;
        let ProductResponse::SecurityApiKeyStarted(_started) = response else {
            return Err("self issue returned the wrong response".into());
        };
    }

    let mut errors = Vec::new();
    for (request_id, scopes) in [
        (10, vec![ProductScope::CatalogObject(sibling)]),
        (11, vec![ProductScope::CatalogSubtree(sibling)]),
        (12, vec![ProductScope::Instance]),
        (
            13,
            vec![
                ProductScope::CatalogObject(child),
                ProductScope::CatalogObject(sibling),
            ],
        ),
    ] {
        let Err(error) = self_issue(&mut fixture, &mut session, request_id, scopes) else {
            return Err("out-of-scope self issue succeeded".into());
        };
        errors.push(*error);
    }
    assert_uniform_authorization_errors(&errors)?;
    Ok(())
}

#[test]
fn managed_instance_scope_admits_target_and_sibling_operations() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("instance")?;
    let mut session = fixture.developer_session("instance developer", ProductScope::Instance, 1)?;
    let mut request_id = 1;
    let mut failures = Vec::new();
    for (side, object) in [
        ("target", ObjectId::new(TARGET_OBJECT)?),
        ("sibling", ObjectId::new(SIBLING_OBJECT)?),
    ] {
        for (operation_name, operation) in operations_for(object, side)? {
            record_expected_admission(
                &mut fixture.product,
                &mut session,
                &mut request_id,
                &format!("instance {side} {operation_name}"),
                operation,
                &mut failures,
            );
        }
    }
    assert_no_scope_failures(&failures);
    Ok(())
}

#[test]
fn managed_object_scope_admits_exact_object_and_denies_sibling() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("object")?;
    let target = ObjectId::new(TARGET_OBJECT)?;
    let sibling = ObjectId::new(SIBLING_OBJECT)?;
    let mut session =
        fixture.developer_session("object developer", ProductScope::CatalogObject(target), 2)?;
    let mut request_id = 1;
    let mut failures = Vec::new();
    for (operation_name, operation) in operations_for(target, "target")? {
        record_expected_admission(
            &mut fixture.product,
            &mut session,
            &mut request_id,
            &format!("object exact {operation_name}"),
            operation,
            &mut failures,
        );
    }
    for (operation_name, operation) in operations_for(sibling, "sibling")? {
        record_expected_denial(
            &mut fixture.product,
            &mut session,
            &mut request_id,
            &format!("object sibling {operation_name}"),
            operation,
            &mut failures,
        );
    }
    assert_no_scope_failures(&failures);
    Ok(())
}

#[test]
fn managed_subtree_scope_admits_descendant_and_denies_sibling_tree() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("subtree")?;
    let target = ObjectId::new(TARGET_OBJECT)?;
    let sibling = ObjectId::new(SIBLING_OBJECT)?;
    let mut session = fixture.developer_session(
        "subtree developer",
        ProductScope::CatalogSubtree(ObjectId::new(DATABASE_A)?),
        3,
    )?;
    let mut request_id = 1;
    let mut failures = Vec::new();
    for (operation_name, operation) in operations_for(target, "target")? {
        record_expected_admission(
            &mut fixture.product,
            &mut session,
            &mut request_id,
            &format!("subtree descendant {operation_name}"),
            operation,
            &mut failures,
        );
    }
    for (operation_name, operation) in operations_for(sibling, "sibling")? {
        record_expected_denial(
            &mut fixture.product,
            &mut session,
            &mut request_id,
            &format!("subtree sibling {operation_name}"),
            operation,
            &mut failures,
        );
    }
    assert_no_scope_failures(&failures);
    Ok(())
}

#[test]
fn catalog_list_requires_instance_authority_until_cursors_are_scope_opaque()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("catalog-list-children")?;
    let database = ObjectId::new(DATABASE_A)?;
    let child = ObjectId::new(TARGET_OBJECT)?;

    for (request_id, label, scope, session_id) in [
        (
            1,
            "exact parent list",
            ProductScope::CatalogObject(database),
            10,
        ),
        (
            2,
            "subtree child list",
            ProductScope::CatalogSubtree(database),
            11,
        ),
    ] {
        let mut session = fixture.developer_session(label, scope, session_id)?;
        let error = dispatch_error(
            &mut fixture.product,
            &mut session,
            request_id,
            catalog_children(database),
        )?;
        assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    }

    let mut instance =
        fixture.developer_session("instance child list", ProductScope::Instance, 12)?;
    let request = context(&instance, 3);
    let response = fixture
        .product
        .dispatch(&mut instance, &request, catalog_children(database))?;
    let ProductResponse::CatalogPage(page) = response else {
        return Err("catalog list returned the wrong response".into());
    };
    assert!(page.items.iter().any(|item| item.id == child));
    assert!(page.items.iter().all(|item| item.parent == Some(database)));
    Ok(())
}

#[test]
fn visible_catalog_exact_and_subtree_hide_siblings_and_hidden_parents() -> Result<(), Box<dyn Error>>
{
    let mut fixture = ManagedFixture::create("visible-catalog-scopes")?;
    let database = ObjectId::new(DATABASE_A)?;
    let child = ObjectId::new(TARGET_OBJECT)?;
    let sibling = ObjectId::new(SIBLING_OBJECT)?;

    let mut exact =
        fixture.developer_session("visible exact", ProductScope::CatalogObject(child), 201)?;
    let request = context(&exact, 1);
    let response = fixture
        .product
        .dispatch(&mut exact, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("visible exact returned wrong response".into());
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, child);
    assert_eq!(page.items[0].parent, None);

    let mut subtree = fixture.developer_session(
        "visible subtree",
        ProductScope::CatalogSubtree(database),
        202,
    )?;
    let request = context(&subtree, 2);
    let response = fixture
        .product
        .dispatch(&mut subtree, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("visible subtree returned wrong response".into());
    };
    let ids = page.items.iter().map(|item| item.id).collect::<Vec<_>>();
    assert!(ids.contains(&database));
    assert!(ids.contains(&child));
    assert!(!ids.contains(&sibling));
    assert_eq!(
        page.items
            .iter()
            .find(|item| item.id == child)
            .and_then(|item| item.parent),
        Some(database)
    );
    Ok(())
}

#[test]
fn visible_catalog_intersects_instance_grant_with_exact_key_ceiling() -> Result<(), Box<dyn Error>>
{
    let mut fixture = ManagedFixture::create("visible-catalog-exact-ceiling")?;
    let child = ObjectId::new(TARGET_OBJECT)?;
    let sibling = ObjectId::new(SIBLING_OBJECT)?;
    let grant = hyphae_native_product::CustomRoleGrant::new(
        ProductPermission::CatalogRead,
        ProductScope::Instance,
    )
    .ok_or("invalid instance catalog grant")?;
    let mut session = fixture.grant_session(
        "visible exact ceiling",
        &[grant],
        &[ProductPermission::CatalogRead],
        &[ProductScope::CatalogObject(child)],
        220,
    )?;

    let request = context(&session, 1);
    let response = fixture
        .product
        .dispatch(&mut session, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("visible exact ceiling returned wrong response".into());
    };
    assert_eq!(
        page.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        [child]
    );
    assert!(!page.items.iter().any(|item| item.id == sibling));
    assert!(page.cursor.is_none());
    Ok(())
}

#[test]
fn visible_catalog_cursor_rejects_cross_key_filter_tamper_trailing_and_oversize()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("visible-catalog-cursor")?;
    let database = ObjectId::new(DATABASE_A)?;
    let mut first = fixture.developer_session(
        "visible first key",
        ProductScope::CatalogSubtree(database),
        203,
    )?;
    let request = context(&first, 1);
    let response = fixture
        .product
        .dispatch(&mut first, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("visible cursor returned wrong response".into());
    };
    let cursor = page.cursor.ok_or("missing visible cursor")?;

    let mut changed_filter = CatalogVisibleListRequest {
        filter: CatalogVisibleListFilter {
            parent: Some(database),
            kind: None,
        },
        cursor: Some(cursor.clone()),
        item_limit: 2,
        visit_limit: 8,
        byte_limit: 4_096,
    };
    let error = dispatch_error(
        &mut fixture.product,
        &mut first,
        2,
        ProductOperation::CatalogVisibleList(changed_filter.clone()),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);

    let mut tampered = cursor.as_bytes().to_vec();
    tampered[20] ^= 1;
    changed_filter.filter.parent = None;
    changed_filter.cursor = Some(hyphae_native_product::CatalogVisibleCursor::new(tampered)?);
    let error = dispatch_error(
        &mut fixture.product,
        &mut first,
        3,
        ProductOperation::CatalogVisibleList(changed_filter.clone()),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);

    let mut wrong_family = cursor.as_bytes().to_vec();
    wrong_family[136] ^= 1;
    changed_filter.cursor = Some(hyphae_native_product::CatalogVisibleCursor::new(
        wrong_family,
    )?);
    let error = dispatch_error(
        &mut fixture.product,
        &mut first,
        4,
        ProductOperation::CatalogVisibleList(changed_filter.clone()),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);

    let mut truncated = cursor.as_bytes().to_vec();
    truncated.pop();
    changed_filter.cursor = Some(hyphae_native_product::CatalogVisibleCursor::new(truncated)?);
    let error = dispatch_error(
        &mut fixture.product,
        &mut first,
        5,
        ProductOperation::CatalogVisibleList(changed_filter.clone()),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);

    let mut trailing = cursor.as_bytes().to_vec();
    trailing.push(0);
    changed_filter.cursor = Some(hyphae_native_product::CatalogVisibleCursor::new(trailing)?);
    let error = dispatch_error(
        &mut fixture.product,
        &mut first,
        6,
        ProductOperation::CatalogVisibleList(changed_filter.clone()),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);

    let mut second = fixture.developer_session(
        "visible second key",
        ProductScope::CatalogSubtree(database),
        204,
    )?;
    changed_filter.cursor = Some(cursor);
    let error = dispatch_error(
        &mut fixture.product,
        &mut second,
        7,
        ProductOperation::CatalogVisibleList(changed_filter.clone()),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);
    changed_filter.cursor = Some(hyphae_native_product::CatalogVisibleCursor::new(vec![
        1;
        257
    ])?);
    let error = dispatch_error(
        &mut fixture.product,
        &mut first,
        8,
        ProductOperation::CatalogVisibleList(changed_filter),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);
    Ok(())
}

#[test]
fn visible_catalog_scope_survives_rename_but_not_drop_recreate() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("visible-catalog-lifecycle")?;
    let (target, _sibling) = fixture.create_sql_tables()?;
    let mut scoped = fixture.developer_session(
        "visible lifecycle exact",
        ProductScope::CatalogObject(target),
        211,
    )?;
    let mut owner = ProductSession::new(
        ProductSessionId::new(212).ok_or("zero lifecycle owner session ID")?,
        ProductPrincipal::new("visible lifecycle owner").ok_or("invalid lifecycle owner")?,
        ProductAuthorization::ALL,
    );

    let request = context(&owner, 1);
    fixture.product.dispatch(
        &mut owner,
        &request,
        ProductOperation::ExecuteSql {
            statement: "ALTER TABLE scope_target_rows RENAME TO scope_renamed_rows".to_owned(),
            parameters: Vec::new(),
        },
    )?;
    let request = context(&scoped, 2);
    let response = fixture
        .product
        .dispatch(&mut scoped, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("renamed visible catalog returned wrong response".into());
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, target);
    assert_eq!(page.items[0].name.object.display(), "scope_renamed_rows");

    let request = context(&owner, 3);
    fixture.product.dispatch(
        &mut owner,
        &request,
        ProductOperation::ExecuteSql {
            statement: "DROP TABLE scope_renamed_rows".to_owned(),
            parameters: Vec::new(),
        },
    )?;
    let replacement = create_sql_table(&mut fixture.product, &mut owner, 4, "scope_renamed_rows")?;
    assert_ne!(replacement, target);
    let request = context(&scoped, 5);
    let response = fixture
        .product
        .dispatch(&mut scoped, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("recreated visible catalog returned wrong response".into());
    };
    assert!(page.items.is_empty());
    Ok(())
}

#[test]
fn visible_catalog_overlapping_scopes_dedupe_in_object_id_order() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("visible-catalog-overlap")?;
    let database = ObjectId::new(DATABASE_A)?;
    let child = ObjectId::new(TARGET_OBJECT)?;
    let grants = [
        hyphae_native_product::CustomRoleGrant::new(
            ProductPermission::CatalogRead,
            ProductScope::CatalogSubtree(database),
        )
        .ok_or("invalid subtree grant")?,
        hyphae_native_product::CustomRoleGrant::new(
            ProductPermission::CatalogRead,
            ProductScope::CatalogObject(child),
        )
        .ok_or("invalid object grant")?,
    ];
    let mut session = fixture.grant_session(
        "visible overlap",
        &grants,
        &[ProductPermission::CatalogRead],
        &[
            ProductScope::CatalogSubtree(database),
            ProductScope::CatalogObject(child),
        ],
        205,
    )?;
    let request = context(&session, 1);
    let response = fixture
        .product
        .dispatch(&mut session, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("visible overlap returned wrong response".into());
    };
    let ids = page.items.iter().map(|item| item.id).collect::<Vec<_>>();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(ids.iter().filter(|id| **id == child).count(), 1);
    Ok(())
}

#[test]
fn visible_catalog_filtered_empty_page_advances_only_visible_work() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("visible-catalog-empty")?;
    let child = ObjectId::new(TARGET_OBJECT)?;
    let mut session = fixture.developer_session(
        "visible empty exact",
        ProductScope::CatalogObject(child),
        206,
    )?;
    let request = context(&session, 1);
    let response = fixture.product.dispatch(
        &mut session,
        &request,
        ProductOperation::CatalogVisibleList(CatalogVisibleListRequest {
            filter: CatalogVisibleListFilter {
                parent: None,
                kind: Some(CatalogObjectKind::Database),
            },
            cursor: None,
            item_limit: 2,
            visit_limit: 1,
            byte_limit: 4_096,
        }),
    )?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("visible filtered page returned wrong response".into());
    };
    assert!(page.items.is_empty());
    assert!(page.cursor.is_none());
    Ok(())
}

#[test]
fn visible_catalog_cursor_conflicts_after_snapshot_change() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("visible-catalog-snapshot-conflict")?;
    let database = ObjectId::new(DATABASE_A)?;
    let mut session = fixture.developer_session(
        "visible snapshot cursor",
        ProductScope::CatalogSubtree(database),
        207,
    )?;
    let request = context(&session, 1);
    let response = fixture
        .product
        .dispatch(&mut session, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("visible cursor returned wrong response".into());
    };
    let cursor = page.cursor.ok_or("missing visible snapshot cursor")?;

    let mut owner = ProductSession::new(
        ProductSessionId::new(208).ok_or("zero owner session ID")?,
        ProductPrincipal::new("visible catalog mutation owner").ok_or("invalid owner")?,
        ProductAuthorization::ALL,
    );
    let owner_request = context(&owner, 2);
    fixture.product.dispatch(
        &mut owner,
        &owner_request,
        ProductOperation::CatalogCreate {
            object: logical_schema(903, "snapshot_change", DATABASE_A)?,
        },
    )?;

    let error = dispatch_error(
        &mut fixture.product,
        &mut session,
        3,
        catalog_visible(Some(cursor)),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);
    Ok(())
}

#[test]
fn visible_catalog_cursor_conflicts_after_authorization_epoch_change() -> Result<(), Box<dyn Error>>
{
    let mut fixture = ManagedFixture::create("visible-catalog-epoch-conflict")?;
    let database = ObjectId::new(DATABASE_A)?;
    let mut session = fixture.developer_session(
        "visible epoch cursor",
        ProductScope::CatalogSubtree(database),
        209,
    )?;
    let request = context(&session, 1);
    let response = fixture
        .product
        .dispatch(&mut session, &request, catalog_visible(None))?;
    let ProductResponse::CatalogVisiblePage(page) = response else {
        return Err("visible epoch cursor returned wrong response".into());
    };
    let cursor = page.cursor.ok_or("missing visible epoch cursor")?;

    let owner = fixture
        .product
        .authenticate_api_key(&fixture.owner_secret, 0)?;
    fixture
        .product
        .create_security_principal(&owner, "advance cursor epoch", 20)?;
    let error = dispatch_error(
        &mut fixture.product,
        &mut session,
        2,
        catalog_visible(Some(cursor)),
    )?;
    assert_eq!(error.code(), ProductErrorCode::CatalogConflict);
    Ok(())
}

#[test]
fn managed_catalog_dependencies_remains_instance_only() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("catalog-dependencies-instance-only")?;
    let child = ObjectId::new(TARGET_OBJECT)?;
    let mut scoped = fixture.developer_session(
        "scoped dependency reader",
        ProductScope::CatalogObject(child),
        210,
    )?;
    let error = dispatch_error(
        &mut fixture.product,
        &mut scoped,
        1,
        ProductOperation::CatalogDependencies(CatalogDependencyRequest {
            object: child,
            direction: DependencyDirection::Outgoing,
            cursor: None,
            item_limit: 8,
            visit_limit: 8,
            byte_limit: 4_096,
        }),
    )?;
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    Ok(())
}

#[test]
fn scoped_prepare_sql_masks_outside_missing_and_malformed_bind_results()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("prepare-sql-oracle")?;
    let (target_table, _sibling_table) = fixture.create_sql_tables()?;
    let statements = [
        "SELECT body FROM scope_sibling_rows WHERE id = ?",
        "SELECT body FROM scope_missing_rows WHERE id = ?",
        "NOT SQL",
    ];

    let mut scoped = fixture.developer_session(
        "scoped sql reader",
        ProductScope::CatalogObject(target_table),
        20,
    )?;
    let mut direct_errors = Vec::new();
    let mut proven_errors = Vec::new();
    for (offset, statement) in statements.iter().enumerate() {
        let offset = u128::try_from(offset)?;
        direct_errors.push(dispatch_error(
            &mut fixture.product,
            &mut scoped,
            10 + offset,
            ProductOperation::PrepareSql {
                statement: (*statement).to_owned(),
            },
        )?);
        proven_errors.push(dispatch_error(
            &mut fixture.product,
            &mut scoped,
            20 + offset,
            ProductOperation::Prove {
                operation: Box::new(ProductOperation::PrepareSql {
                    statement: (*statement).to_owned(),
                }),
                limits: NativeProofGenerationLimits::default(),
            },
        )?);
    }
    assert_uniform_authorization_errors(&direct_errors)?;
    assert_uniform_authorization_errors(&proven_errors)?;

    let mut instance =
        fixture.developer_session("instance sql reader", ProductScope::Instance, 21)?;
    let request = context(&instance, 30);
    let response = fixture.product.dispatch(
        &mut instance,
        &request,
        ProductOperation::PrepareSql {
            statement: statements[0].to_owned(),
        },
    )?;
    assert!(matches!(response, ProductResponse::PreparedSql { .. }));
    let missing = dispatch_error(
        &mut fixture.product,
        &mut instance,
        31,
        ProductOperation::PrepareSql {
            statement: statements[1].to_owned(),
        },
    )?;
    assert_eq!(missing.code(), ProductErrorCode::SqlUnknownObject);
    let malformed = dispatch_error(
        &mut fixture.product,
        &mut instance,
        32,
        ProductOperation::PrepareSql {
            statement: statements[2].to_owned(),
        },
    )?;
    assert_eq!(malformed.code(), ProductErrorCode::SqlInvalidSyntax);
    Ok(())
}

#[test]
fn scoped_execute_sql_masks_outside_missing_and_malformed_bind_results()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("execute-sql-oracle")?;
    let (target_table, _sibling_table) = fixture.create_sql_tables()?;
    let cases = [
        (
            "outside-existing",
            "SELECT body FROM scope_sibling_rows WHERE id = ?",
            vec![ProductValue::Signed(1)],
        ),
        (
            "missing",
            "SELECT body FROM scope_missing_rows WHERE id = ?",
            vec![ProductValue::Signed(1)],
        ),
        ("malformed", "NOT SQL", Vec::new()),
    ];

    let mut scoped = fixture.developer_session(
        "scoped sql executor",
        ProductScope::CatalogObject(target_table),
        22,
    )?;
    let mut errors = Vec::new();
    for (offset, (_label, statement, parameters)) in cases.iter().enumerate() {
        let offset = u128::try_from(offset)?;
        errors.push(dispatch_error(
            &mut fixture.product,
            &mut scoped,
            40 + offset,
            ProductOperation::ExecuteSql {
                statement: (*statement).to_owned(),
                parameters: parameters.clone(),
            },
        )?);
    }

    let leaked = cases
        .iter()
        .zip(&errors)
        .filter(|(_case, error)| error.code() != ProductErrorCode::AuthorizationDenied)
        .map(|((label, _statement, _parameters), error)| format!("{label}={:?}", error.code()))
        .collect::<Vec<_>>();
    assert!(
        leaked.is_empty(),
        "scoped ExecuteSql exposed distinguishable bind results: {}",
        leaked.join(", ")
    );
    assert_uniform_authorization_errors(&errors)?;
    Ok(())
}

#[test]
fn managed_default_scalar_operations_use_the_durable_exact_object_scope()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("default-scalar-scope")?;
    let keyspace = fixture.default_scalar_keyspace_id()?;
    let unrelated = ObjectId::new(TARGET_OBJECT)?;
    let operations = || {
        [
            ProductOperation::StructureGet {
                key: b"scoped-scalar".to_vec(),
            },
            ProductOperation::StructureSet {
                key: b"scoped-scalar".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: Some(100),
            },
            ProductOperation::StructureTtl {
                key: b"scoped-scalar".to_vec(),
            },
        ]
    };

    let mut exact = fixture.scoped_session(
        "exact scalar scope",
        &[ProductPermission::DataRead, ProductPermission::DataWrite],
        &[ProductScope::CatalogObject(keyspace)],
        87,
    )?;
    for (offset, operation) in operations().into_iter().enumerate() {
        let request = context(&exact, 120 + u128::try_from(offset)?);
        let result = fixture.product.dispatch(&mut exact, &request, operation);
        assert!(
            result.is_ok(),
            "exact default keyspace scope failed: {result:?}"
        );
    }

    let mut outside = fixture.scoped_session(
        "unrelated scalar scope",
        &[ProductPermission::DataRead, ProductPermission::DataWrite],
        &[ProductScope::CatalogObject(unrelated)],
        88,
    )?;
    for (offset, operation) in operations().into_iter().enumerate() {
        let error = dispatch_error(
            &mut fixture.product,
            &mut outside,
            130 + u128::try_from(offset)?,
            operation,
        )?;
        assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    }
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the end-to-end scope test keeps setup and all exact-object assertions together"
)]
fn managed_execute_sql_binds_exact_dml_indexes_and_join_objects() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("execute-sql-bound-objects")?;
    let (target, sibling) = fixture.create_sql_tables()?;
    let mut owner = ProductSession::new(
        ProductSessionId::new(80).ok_or("zero owner session ID")?,
        ProductPrincipal::new("sql fixture owner").ok_or("invalid owner")?,
        ProductAuthorization::ALL,
    );
    let mut target_only = fixture.scoped_session(
        "target SQL writer",
        &[ProductPermission::CatalogRead, ProductPermission::DataWrite],
        &[ProductScope::CatalogObject(target)],
        81,
    )?;
    let request = context(&target_only, 80);
    let response = fixture.product.dispatch(
        &mut target_only,
        &request,
        ProductOperation::ExecuteSql {
            statement: "INSERT INTO scope_target_rows (id, body) VALUES (?, ?)".to_owned(),
            parameters: vec![
                ProductValue::Signed(1),
                ProductValue::Text("joined".to_owned()),
            ],
        },
    )?;
    assert!(matches!(response, ProductResponse::Sql { .. }));

    let mut target_prover = fixture.scoped_session(
        "target SQL prover",
        &[
            ProductPermission::CatalogRead,
            ProductPermission::DataRead,
            ProductPermission::ProofGenerate,
        ],
        &[ProductScope::CatalogObject(target)],
        85,
    )?;
    let request = context(&target_prover, 79);
    let response = fixture.product.dispatch(
        &mut target_prover,
        &request,
        ProductOperation::Prove {
            operation: Box::new(ProductOperation::ExecuteSql {
                statement: "SELECT body FROM scope_target_rows WHERE id = ?".to_owned(),
                parameters: vec![ProductValue::Signed(1)],
            }),
            limits: NativeProofGenerationLimits::default(),
        },
    )?;
    assert!(matches!(response, ProductResponse::Proven { .. }));

    let mut index_ids = Vec::new();
    for (request_id, statement) in [
        (
            81,
            "CREATE INDEX scope_target_body ON scope_target_rows (body)",
        ),
        (
            82,
            "CREATE UNIQUE INDEX scope_sibling_body ON scope_sibling_rows (body)",
        ),
    ] {
        let request = context(&owner, request_id);
        let response = fixture.product.dispatch(
            &mut owner,
            &request,
            ProductOperation::ExecuteSql {
                statement: statement.to_owned(),
                parameters: Vec::new(),
            },
        )?;
        let ProductResponse::Sql {
            result:
                ProductSqlResult::Command {
                    object_id: Some(index),
                    ..
                },
            ..
        } = response
        else {
            return Err("index creation returned no identity".into());
        };
        index_ids.push(index);
    }

    let denied = dispatch_error(
        &mut fixture.product,
        &mut target_only,
        90,
        ProductOperation::ExecuteSql {
            statement: "UPDATE scope_target_rows SET body = ? WHERE id = ?".to_owned(),
            parameters: vec![
                ProductValue::Text("changed".to_owned()),
                ProductValue::Signed(1),
            ],
        },
    )?;
    assert_eq!(denied.code(), ProductErrorCode::AuthorizationDenied);

    let mut missing_sibling_index = fixture.scoped_session(
        "incomplete joined SQL reader",
        &[ProductPermission::CatalogRead, ProductPermission::DataRead],
        &[
            ProductScope::CatalogObject(target),
            ProductScope::CatalogObject(sibling),
        ],
        84,
    )?;
    let error = dispatch_error(
        &mut fixture.product,
        &mut missing_sibling_index,
        92,
        ProductOperation::ExecuteSql {
            statement: "SELECT scope_target_rows.id, scope_sibling_rows.body FROM scope_target_rows INNER JOIN scope_sibling_rows ON scope_target_rows.body = scope_sibling_rows.body WHERE scope_target_rows.id = ?".to_owned(),
            parameters: vec![ProductValue::Signed(1)],
        },
    )?;
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    assert_eq!(index_ids.len(), 2);
    Ok(())
}

#[test]
fn managed_explain_requires_instance_observe_and_object_catalog_read() -> Result<(), Box<dyn Error>>
{
    let mut fixture = ManagedFixture::create("explain-split-scope")?;
    let (target, sibling) = fixture.create_sql_tables()?;
    let grants = [
        hyphae_native_product::CustomRoleGrant::new(
            ProductPermission::Observe,
            ProductScope::Instance,
        )
        .ok_or("invalid observe grant")?,
        hyphae_native_product::CustomRoleGrant::new(
            ProductPermission::CatalogRead,
            ProductScope::CatalogObject(target),
        )
        .ok_or("invalid catalog grant")?,
    ];
    let mut target_explainer = fixture.grant_session(
        "target SQL explainer",
        &grants,
        &[ProductPermission::Observe, ProductPermission::CatalogRead],
        &[ProductScope::Instance],
        83,
    )?;
    let request = context(&target_explainer, 100);
    let response = fixture.product.dispatch(
        &mut target_explainer,
        &request,
        ProductOperation::AdminExplainSql {
            statement: "SELECT body FROM scope_target_rows WHERE id = ?".to_owned(),
        },
    )?;
    assert!(matches!(response, ProductResponse::Explain(_)));
    let error = dispatch_error(
        &mut fixture.product,
        &mut target_explainer,
        101,
        ProductOperation::AdminExplainSql {
            statement: "SELECT body FROM scope_sibling_rows WHERE id = ?".to_owned(),
        },
    )?;
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    assert_ne!(target, sibling);
    Ok(())
}

#[test]
fn scoped_explain_masks_outside_missing_and_malformed_bind_results() -> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("explain-sql-oracle")?;
    let (target, _sibling) = fixture.create_sql_tables()?;
    let grants = [
        hyphae_native_product::CustomRoleGrant::new(
            ProductPermission::Observe,
            ProductScope::Instance,
        )
        .ok_or("invalid observe grant")?,
        hyphae_native_product::CustomRoleGrant::new(
            ProductPermission::CatalogRead,
            ProductScope::CatalogObject(target),
        )
        .ok_or("invalid catalog grant")?,
    ];
    let mut scoped = fixture.grant_session(
        "scoped SQL explain oracle",
        &grants,
        &[ProductPermission::Observe, ProductPermission::CatalogRead],
        &[ProductScope::Instance],
        86,
    )?;
    let mut errors = Vec::new();
    for (offset, statement) in [
        "SELECT body FROM scope_sibling_rows WHERE id = ?",
        "SELECT body FROM scope_missing_rows WHERE id = ?",
        "NOT SQL",
    ]
    .iter()
    .enumerate()
    {
        errors.push(dispatch_error(
            &mut fixture.product,
            &mut scoped,
            110 + u128::try_from(offset)?,
            ProductOperation::AdminExplainSql {
                statement: (*statement).to_owned(),
            },
        )?);
    }
    assert_uniform_authorization_errors(&errors)
}

#[test]
fn catalog_list_empty_filtered_page_cannot_leak_out_of_scope_traversal_metadata()
-> Result<(), Box<dyn Error>> {
    let mut fixture = ManagedFixture::create("catalog-list-empty-page")?;
    let database = ObjectId::new(DATABASE_A)?;
    let child = ObjectId::new(TARGET_OBJECT)?;
    let mut exact_parent = fixture.developer_session(
        "exact parent empty list",
        ProductScope::CatalogObject(database),
        30,
    )?;
    let snapshot = fixture.product.catalog_snapshot()?;
    let request = context(&exact_parent, 1);
    let result = fixture.product.dispatch(
        &mut exact_parent,
        &request,
        ProductOperation::CatalogList(CatalogListRequest {
            parent: Some(database),
            kind: Some(CatalogObjectKind::Database),
            cursor: Some(CatalogCursor::new(snapshot.identity(), database)),
            item_limit: 8,
            visit_limit: 1,
            byte_limit: 4_096,
        }),
    );
    match result {
        Err(error) => {
            assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
            Ok(())
        }
        Ok(ProductResponse::CatalogPage(page)) => {
            assert!(page.items.is_empty());
            assert_eq!(page.visited, 1);
            assert_eq!(page.stop, CatalogPageStop::VisitLimit);
            assert_eq!(page.cursor.map(|cursor| cursor.after()), Some(child));
            Err(format!(
                "authorized empty page leaked out-of-scope traversal: after={child:?}, visited={}, stop={:?}",
                page.visited, page.stop
            )
            .into())
        }
        Ok(_) => Err("catalog list returned the wrong response".into()),
    }
}
