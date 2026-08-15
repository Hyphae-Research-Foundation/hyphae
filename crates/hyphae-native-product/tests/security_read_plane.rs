// SPDX-License-Identifier: AGPL-3.0-only

//! Managed, instance-scoped security read-plane contract.

use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_product::proof::NativeProofGenerationLimits;
use hyphae_native_product::{
    BuiltInRole, CustomRoleGrant, NativeProduct, ProductAuthorization, ProductError,
    ProductErrorCode, ProductOperation, ProductPermission, ProductPrincipal, ProductRequestContext,
    ProductResponse, ProductScope, ProductSession, ProductSessionId, SecurityAssignmentListRequest,
    SecurityAuditReadRequest, SecurityKeyListRequest, SecurityPrincipalListRequest,
    SecurityRoleListRequest,
};
use hyphae_native_types::ObjectId;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct SecurityReadFixture {
    product: NativeProduct,
    directory: PathBuf,
    key_paths: Vec<PathBuf>,
    secrets: Vec<String>,
}

impl SecurityReadFixture {
    fn create(name: &str) -> Result<Self, Box<dyn Error>> {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyphae-security-read-{name}-{}-{suffix}",
            std::process::id()
        ));
        let owner_key = directory.with_extension("owner-key");
        let _ignored = fs::remove_dir_all(&directory);
        let _ignored = fs::remove_file(&owner_key);

        let mut product = NativeProduct::create(&directory)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_key, 1)?;
        let owner_secret = fs::read_to_string(&owner_key)?;
        Ok(Self {
            product,
            directory,
            key_paths: vec![owner_key],
            secrets: vec![owner_secret],
        })
    }

    fn owner_session(&self, session_id: u128) -> Result<ProductSession, Box<dyn Error>> {
        let authority = self.product.authenticate_api_key(&self.secrets[0], 0)?;
        Ok(ProductSession::new_authenticated(
            ProductSessionId::new(session_id).ok_or("zero owner session")?,
            authority,
        ))
    }

    fn permission_session(
        &mut self,
        label: &str,
        permission: ProductPermission,
        session_id: u128,
    ) -> Result<ProductSession, Box<dyn Error>> {
        let owner = self.product.authenticate_api_key(&self.secrets[0], 0)?;
        let principal = self.product.create_security_principal(&owner, label, 10)?;
        let owner = self.product.authenticate_api_key(&self.secrets[0], 0)?;
        let role = self.product.create_custom_security_role(
            &owner,
            &format!("{label} role"),
            [CustomRoleGrant::new(permission, ProductScope::Instance)
                .ok_or("invalid instance grant")?],
            11,
        )?;
        let owner = self.product.authenticate_api_key(&self.secrets[0], 0)?;
        self.product.assign_custom_security_role(
            &owner,
            principal.principal_id,
            role.role_id,
            12,
        )?;
        let owner = self.product.authenticate_api_key(&self.secrets[0], 0)?;
        let key_path = self.directory.with_extension(format!("{label}-key"));
        let _ignored = fs::remove_file(&key_path);
        self.product.issue_scoped_api_key_to_file(
            &owner,
            principal.principal_id,
            label,
            [],
            [role.role_id],
            ProductAuthorization::from_permissions([permission]),
            [ProductScope::Instance],
            None,
            &key_path,
            13,
        )?;
        self.session_from_key(key_path, session_id)
    }

    fn scoped_auditor_session(
        &mut self,
        label: &str,
        scope: ProductScope,
        session_id: u128,
    ) -> Result<ProductSession, Box<dyn Error>> {
        let owner = self.product.authenticate_api_key(&self.secrets[0], 0)?;
        let principal = self.product.create_security_principal(&owner, label, 20)?;
        let owner = self.product.authenticate_api_key(&self.secrets[0], 0)?;
        self.product.assign_built_in_role(
            &owner,
            principal.principal_id,
            BuiltInRole::Auditor,
            scope,
            21,
        )?;
        let owner = self.product.authenticate_api_key(&self.secrets[0], 0)?;
        let key_path = self.directory.with_extension(format!("{label}-key"));
        let _ignored = fs::remove_file(&key_path);
        self.product.issue_scoped_api_key_to_file(
            &owner,
            principal.principal_id,
            label,
            [BuiltInRole::Auditor],
            [],
            BuiltInRole::Auditor.authorization(),
            [scope],
            None,
            &key_path,
            22,
        )?;
        self.session_from_key(key_path, session_id)
    }

    fn session_from_key(
        &mut self,
        key_path: PathBuf,
        session_id: u128,
    ) -> Result<ProductSession, Box<dyn Error>> {
        let secret = fs::read_to_string(&key_path)?;
        let authority = self.product.authenticate_api_key(&secret, 0)?;
        self.key_paths.push(key_path);
        self.secrets.push(secret);
        Ok(ProductSession::new_authenticated(
            ProductSessionId::new(session_id).ok_or("zero managed session")?,
            authority,
        ))
    }

    fn operations() -> Result<Vec<ProductOperation>, Box<dyn Error>> {
        Ok(vec![
            ProductOperation::SecurityStatus,
            ProductOperation::SecurityPrincipalList(SecurityPrincipalListRequest::new(None, 1)?),
            ProductOperation::SecurityRoleList(SecurityRoleListRequest::new(None, 1)?),
            ProductOperation::SecurityAssignmentList(SecurityAssignmentListRequest::new(None, 1)?),
            ProductOperation::SecurityKeyList(SecurityKeyListRequest::new(None, 1)?),
            ProductOperation::SecurityAuditRead(SecurityAuditReadRequest::new(None, 1)?),
        ])
    }

    fn assert_redacted(&self, rendered: &str) {
        for secret in &self.secrets {
            assert!(!rendered.contains(secret));
            let secret_fragment = secret.rsplit('_').next().unwrap_or(secret);
            assert!(!rendered.contains(secret_fragment));
        }
    }
}

impl Drop for SecurityReadFixture {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.directory);
        for key_path in &self.key_paths {
            let _ignored = fs::remove_file(key_path);
        }
    }
}

fn context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    ProductRequestContext::new(
        request_id,
        session.id(),
        100,
        session.principal().clone(),
        session.authorization(),
    )
    .with_authorization_epoch(session.authorization_epoch())
}

fn dispatch(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
    operation: ProductOperation,
) -> Result<ProductResponse, Box<ProductError>> {
    let request_context = context(session, request_id);
    product
        .dispatch(session, &request_context, operation)
        .map_err(Box::new)
}

fn assert_denied(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
    operation: ProductOperation,
) -> Result<(), Box<dyn Error>> {
    let Err(error) = dispatch(product, session, request_id, operation) else {
        return Err("instance-scoped read unexpectedly succeeded".into());
    };
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    Ok(())
}

#[test]
fn exact_instance_permissions_partition_the_managed_read_plane() -> Result<(), Box<dyn Error>> {
    let mut fixture = SecurityReadFixture::create("exact-permissions")?;
    let mut security_reader =
        fixture.permission_session("security-reader", ProductPermission::SecurityRead, 10)?;
    let mut audit_reader =
        fixture.permission_session("audit-reader", ProductPermission::AuditRead, 11)?;

    let status = dispatch(
        &mut fixture.product,
        &mut security_reader,
        10,
        ProductOperation::SecurityStatus,
    )?;
    assert!(matches!(status, ProductResponse::SecurityStatus(_)));
    for (request_id, operation) in SecurityReadFixture::operations()?
        .into_iter()
        .enumerate()
        .skip(1)
        .take(4)
    {
        let response = dispatch(
            &mut fixture.product,
            &mut security_reader,
            20 + u128::try_from(request_id)?,
            operation,
        )?;
        assert!(matches!(
            response,
            ProductResponse::SecurityPrincipalPage(_)
                | ProductResponse::SecurityRolePage(_)
                | ProductResponse::SecurityAssignmentPage(_)
                | ProductResponse::SecurityKeyPage(_)
        ));
    }
    assert_denied(
        &mut fixture.product,
        &mut security_reader,
        30,
        ProductOperation::SecurityAuditRead(SecurityAuditReadRequest::new(None, 1)?),
    )?;
    let audit = dispatch(
        &mut fixture.product,
        &mut audit_reader,
        40,
        ProductOperation::SecurityAuditRead(SecurityAuditReadRequest::new(None, 1)?),
    )?;
    assert!(matches!(audit, ProductResponse::SecurityAuditPage(_)));
    assert_denied(
        &mut fixture.product,
        &mut audit_reader,
        41,
        ProductOperation::SecurityStatus,
    )?;

    Ok(())
}

fn assert_principal_and_role_pagination(
    fixture: &mut SecurityReadFixture,
    security_reader: &mut ProductSession,
) -> Result<(), Box<dyn Error>> {
    let principal_first = dispatch(
        &mut fixture.product,
        security_reader,
        100,
        ProductOperation::SecurityPrincipalList(SecurityPrincipalListRequest::new(None, 1)?),
    )?;
    let ProductResponse::SecurityPrincipalPage(principal_first) = principal_first else {
        return Err("principal list returned the wrong response".into());
    };
    let principal_cursor = principal_first
        .next_cursor
        .ok_or("missing principal cursor")?;
    assert_eq!(
        principal_cursor.authorization_epoch(),
        principal_first.authorization_epoch()
    );
    let principal_second = dispatch(
        &mut fixture.product,
        security_reader,
        101,
        ProductOperation::SecurityPrincipalList(SecurityPrincipalListRequest::new(
            Some(principal_cursor),
            1,
        )?),
    )?;
    let ProductResponse::SecurityPrincipalPage(principal_second) = principal_second else {
        return Err("principal continuation returned the wrong response".into());
    };
    assert_eq!(principal_first.items.len(), 1);
    assert_eq!(principal_second.items.len(), 1);
    assert_ne!(
        principal_first.items[0].id(),
        principal_second.items[0].id()
    );

    let role_first = dispatch(
        &mut fixture.product,
        security_reader,
        102,
        ProductOperation::SecurityRoleList(SecurityRoleListRequest::new(None, 1)?),
    )?;
    let ProductResponse::SecurityRolePage(role_first) = role_first else {
        return Err("role list returned the wrong response".into());
    };
    let role_second = dispatch(
        &mut fixture.product,
        security_reader,
        103,
        ProductOperation::SecurityRoleList(SecurityRoleListRequest::new(
            role_first.next_cursor,
            1,
        )?),
    )?;
    let ProductResponse::SecurityRolePage(role_second) = role_second else {
        return Err("role continuation returned the wrong response".into());
    };
    assert_eq!(role_first.items.len(), 1);
    assert_eq!(role_second.items.len(), 1);
    fixture.assert_redacted(&format!(
        "{principal_first:?}{principal_second:?}{role_first:?}{role_second:?}"
    ));
    Ok(())
}

fn assert_assignment_and_key_pagination(
    fixture: &mut SecurityReadFixture,
    security_reader: &mut ProductSession,
) -> Result<(), Box<dyn Error>> {
    let assignment_first = dispatch(
        &mut fixture.product,
        security_reader,
        104,
        ProductOperation::SecurityAssignmentList(SecurityAssignmentListRequest::new(None, 1)?),
    )?;
    let ProductResponse::SecurityAssignmentPage(assignment_first) = assignment_first else {
        return Err("assignment list returned the wrong response".into());
    };
    let assignment_second = dispatch(
        &mut fixture.product,
        security_reader,
        105,
        ProductOperation::SecurityAssignmentList(SecurityAssignmentListRequest::new(
            assignment_first.next_cursor,
            1,
        )?),
    )?;
    let ProductResponse::SecurityAssignmentPage(assignment_second) = assignment_second else {
        return Err("assignment continuation returned the wrong response".into());
    };
    assert_eq!(assignment_first.items.len(), 1);
    assert_eq!(assignment_second.items.len(), 1);

    let key_first = dispatch(
        &mut fixture.product,
        security_reader,
        106,
        ProductOperation::SecurityKeyList(SecurityKeyListRequest::new(None, 1)?),
    )?;
    let ProductResponse::SecurityKeyPage(key_first) = key_first else {
        return Err("key list returned the wrong response".into());
    };
    let key_second = dispatch(
        &mut fixture.product,
        security_reader,
        107,
        ProductOperation::SecurityKeyList(SecurityKeyListRequest::new(key_first.next_cursor, 1)?),
    )?;
    let ProductResponse::SecurityKeyPage(key_second) = key_second else {
        return Err("key continuation returned the wrong response".into());
    };
    assert_eq!(key_first.items.len(), 1);
    assert_eq!(key_second.items.len(), 1);
    fixture.assert_redacted(&format!(
        "{assignment_first:?}{assignment_second:?}{key_first:?}{key_second:?}"
    ));
    Ok(())
}

fn assert_audit_pagination(
    fixture: &mut SecurityReadFixture,
    audit_reader: &mut ProductSession,
) -> Result<(), Box<dyn Error>> {
    let audit_first = dispatch(
        &mut fixture.product,
        audit_reader,
        108,
        ProductOperation::SecurityAuditRead(SecurityAuditReadRequest::new(None, 1)?),
    )?;
    let ProductResponse::SecurityAuditPage(audit_first) = audit_first else {
        return Err("audit list returned the wrong response".into());
    };
    let audit_second = dispatch(
        &mut fixture.product,
        audit_reader,
        109,
        ProductOperation::SecurityAuditRead(SecurityAuditReadRequest::new(
            audit_first.next_cursor,
            1,
        )?),
    )?;
    let ProductResponse::SecurityAuditPage(audit_second) = audit_second else {
        return Err("audit continuation returned the wrong response".into());
    };
    assert_eq!(audit_first.events.len(), 1);
    assert_eq!(audit_second.events.len(), 1);
    fixture.assert_redacted(&format!("{audit_first:?}{audit_second:?}"));
    Ok(())
}

#[test]
fn security_pages_are_paginated_epoch_bound_and_redacted() -> Result<(), Box<dyn Error>> {
    let mut fixture = SecurityReadFixture::create("pagination-redaction")?;
    let mut security_reader =
        fixture.permission_session("page-security-reader", ProductPermission::SecurityRead, 20)?;
    let mut audit_reader =
        fixture.permission_session("page-audit-reader", ProductPermission::AuditRead, 21)?;

    assert_principal_and_role_pagination(&mut fixture, &mut security_reader)?;
    assert_assignment_and_key_pagination(&mut fixture, &mut security_reader)?;
    assert_audit_pagination(&mut fixture, &mut audit_reader)?;
    Ok(())
}

#[test]
fn unmanaged_and_catalog_scoped_sessions_cannot_enter_the_read_plane() -> Result<(), Box<dyn Error>>
{
    let mut fixture = SecurityReadFixture::create("managed-instance-only")?;
    let mut unmanaged = ProductSession::new(
        ProductSessionId::new(30).ok_or("zero unmanaged session")?,
        ProductPrincipal::new("unmanaged-test").ok_or("invalid unmanaged principal")?,
        ProductAuthorization::ALL,
    );
    for (offset, operation) in SecurityReadFixture::operations()?.into_iter().enumerate() {
        assert_denied(
            &mut fixture.product,
            &mut unmanaged,
            200 + u128::try_from(offset)?,
            operation,
        )?;
    }

    let object = ObjectId::new(7_001)?;
    for (scope_index, scope) in [
        ProductScope::CatalogObject(object),
        ProductScope::CatalogSubtree(object),
    ]
    .into_iter()
    .enumerate()
    {
        let mut scoped = fixture.scoped_auditor_session(
            &format!("scoped-auditor-{scope_index}"),
            scope,
            31 + u128::try_from(scope_index)?,
        )?;
        for (operation_index, operation) in
            SecurityReadFixture::operations()?.into_iter().enumerate()
        {
            assert_denied(
                &mut fixture.product,
                &mut scoped,
                300 + u128::try_from(scope_index * 10 + operation_index)?,
                operation,
            )?;
        }
    }
    Ok(())
}

#[test]
fn security_read_plane_operations_are_not_semantic_proof_inputs() -> Result<(), Box<dyn Error>> {
    let mut fixture = SecurityReadFixture::create("prove-rejection")?;
    let mut owner = fixture.owner_session(40)?;
    for (offset, operation) in SecurityReadFixture::operations()?.into_iter().enumerate() {
        let Err(error) = dispatch(
            &mut fixture.product,
            &mut owner,
            400 + u128::try_from(offset)?,
            ProductOperation::Prove {
                operation: Box::new(operation),
                limits: NativeProofGenerationLimits::default(),
            },
        ) else {
            return Err("security read-plane operation unexpectedly produced a proof".into());
        };
        assert_eq!(error.code(), ProductErrorCode::InvalidRequest);
    }
    Ok(())
}
