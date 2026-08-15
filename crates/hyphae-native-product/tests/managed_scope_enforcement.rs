// SPDX-License-Identifier: AGPL-3.0-only

//! Managed API-key scope admission across operations carrying explicit object IDs.

use std::{collections::BTreeMap, error::Error, fs, path::PathBuf};

use hyphae_native_catalog::{
    CatalogName, CatalogObjectKind, CatalogObjectV2, DefinitionVersion, DependencyDirection,
    LogicalCatalogObject, ObjectHeaderV2, QualifiedName,
};
use hyphae_native_product::proof::NativeProofGenerationLimits;
use hyphae_native_product::{
    BoundedSearchQuery, BuiltInRole, CatalogCursor, CatalogDependencyRequest, CatalogListRequest,
    NativeProduct, ProductAuthorization, ProductDocument, ProductDurability, ProductError,
    ProductErrorCode, ProductLexicalBranch, ProductOperation, ProductPrincipal,
    ProductRequestContext, ProductResponse, ProductScope, ProductSearchDocumentDelete,
    ProductSearchDocumentUpdate, ProductSearchFilter, ProductSearchIngestBatch,
    ProductSearchRequest, ProductSession, ProductSessionId, ProductSetAlgebraOperation,
    ProductSqlResult, ProductStructureKey, ProductStructureMutation, ProductStructureReadRequest,
    ProductValue,
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
            12,
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
            "catalog dependencies",
            ProductOperation::CatalogDependencies(CatalogDependencyRequest {
                object,
                direction: DependencyDirection::Outgoing,
                cursor: None,
                item_limit: 8,
                visit_limit: 8,
                byte_limit: 4_096,
            }),
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
