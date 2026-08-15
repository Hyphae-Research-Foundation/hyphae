// SPDX-License-Identifier: AGPL-3.0-only

//! Managed, idempotent, secret-free security write-plane contract.

use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_product::proof::NativeProofGenerationLimits;
use hyphae_native_product::{
    BuiltInRole, CustomRoleGrant, NativeProduct, ProductAuthorization, ProductDurability,
    ProductError, ProductErrorCode, ProductOperation, ProductPermission, ProductPrincipal,
    ProductRequestContext, ProductResponse, ProductScope, ProductSession, ProductSessionId,
    SecurityPrincipalListRequest,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct SecurityWriteFixture {
    product: NativeProduct,
    directory: PathBuf,
    owner_key: PathBuf,
    owner_secret: String,
}

impl SecurityWriteFixture {
    fn create(name: &str) -> Result<Self, Box<dyn Error>> {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyphae-security-write-{name}-{}-{suffix}",
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
            owner_key,
            owner_secret,
        })
    }

    fn owner_session(&self, id: u128) -> Result<ProductSession, Box<dyn Error>> {
        let authority = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        Ok(ProductSession::new_authenticated(
            ProductSessionId::new(id).ok_or("zero owner session")?,
            authority,
        ))
    }
}

impl Drop for SecurityWriteFixture {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.directory);
        let _ignored = fs::remove_file(&self.owner_key);
    }
}

fn context(
    session: &ProductSession,
    request_id: u128,
    idempotency_token: Option<u128>,
) -> ProductRequestContext {
    let context = ProductRequestContext::new(
        request_id,
        session.id(),
        100,
        session.principal().clone(),
        session.authorization(),
    )
    .with_authorization_epoch(session.authorization_epoch());
    match idempotency_token {
        Some(token) => context.with_idempotency_token(token),
        None => context,
    }
}

fn dispatch(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
    idempotency_token: Option<u128>,
    operation: ProductOperation,
) -> Result<ProductResponse, Box<ProductError>> {
    let request = context(session, request_id, idempotency_token);
    product
        .dispatch(session, &request, operation)
        .map_err(Box::new)
}

fn principal_enabled(
    product: &mut NativeProduct,
    owner: &mut ProductSession,
    principal_id: hyphae_native_product::SecurityId,
) -> Result<bool, Box<dyn Error>> {
    let response = dispatch(
        product,
        owner,
        900,
        None,
        ProductOperation::SecurityPrincipalList(SecurityPrincipalListRequest::new(None, 1000)?),
    )?;
    let ProductResponse::SecurityPrincipalPage(page) = response else {
        return Err("principal list returned the wrong response".into());
    };
    page.items
        .iter()
        .find(|principal| principal.id() == principal_id)
        .map(hyphae_native_product::SecurityPrincipalSummary::enabled)
        .ok_or_else(|| "created principal is absent".into())
}

#[test]
fn security_writes_are_strict_idempotent_and_reversible() -> Result<(), Box<dyn Error>> {
    let mut fixture = SecurityWriteFixture::create("lifecycle")?;
    let mut owner = fixture.owner_session(10)?;
    let create = ProductOperation::SecurityPrincipalCreate {
        display_name: "Analyst".to_owned(),
    };
    let first = dispatch(
        &mut fixture.product,
        &mut owner,
        1,
        Some(101),
        create.clone(),
    )?;
    let replay = dispatch(&mut fixture.product, &mut owner, 2, Some(101), create)?;
    assert_eq!(first, replay);
    let ProductResponse::SecurityPrincipalMutated(principal) = first else {
        return Err("principal creation returned the wrong response".into());
    };
    assert_eq!(principal.commit.durability, ProductDurability::Strict);
    assert!(!principal_enabled(
        &mut fixture.product,
        &mut owner,
        principal.principal_id
    )?);

    let enabled = dispatch(
        &mut fixture.product,
        &mut owner,
        3,
        Some(102),
        ProductOperation::SecurityPrincipalSetEnabled {
            principal_id: principal.principal_id,
            enabled: true,
        },
    )?;
    assert!(matches!(enabled, ProductResponse::SecurityMutated(_)));
    assert!(principal_enabled(
        &mut fixture.product,
        &mut owner,
        principal.principal_id
    )?);

    let grant = CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
        .ok_or("invalid instance grant")?;
    let role = dispatch(
        &mut fixture.product,
        &mut owner,
        4,
        Some(103),
        ProductOperation::SecurityCustomRoleCreate {
            display_name: "Analyst reader".to_owned(),
            grants: vec![grant],
        },
    )?;
    let ProductResponse::SecurityCustomRoleMutated(role) = role else {
        return Err("custom-role creation returned the wrong response".into());
    };
    let assignment = dispatch(
        &mut fixture.product,
        &mut owner,
        5,
        Some(104),
        ProductOperation::SecurityCustomAssignmentCreate {
            principal_id: principal.principal_id,
            role_id: role.role_id,
        },
    )?;
    let ProductResponse::SecurityAssignmentMutated(assignment) = assignment else {
        return Err("custom assignment returned the wrong response".into());
    };
    let revoked = dispatch(
        &mut fixture.product,
        &mut owner,
        6,
        Some(105),
        ProductOperation::SecurityAssignmentRevoke {
            assignment_id: assignment.assignment_id,
        },
    )?;
    assert!(matches!(revoked, ProductResponse::SecurityMutated(_)));
    Ok(())
}

#[test]
fn security_write_admission_is_managed_token_bound_and_fail_closed() -> Result<(), Box<dyn Error>> {
    let mut fixture = SecurityWriteFixture::create("admission")?;
    let mut owner = fixture.owner_session(20)?;
    let operation = ProductOperation::SecurityPrincipalCreate {
        display_name: "No token".to_owned(),
    };
    let Err(missing) = dispatch(
        &mut fixture.product,
        &mut owner,
        10,
        None,
        operation.clone(),
    ) else {
        return Err("owner mutation without an idempotency token succeeded".into());
    };
    assert_eq!(missing.code(), ProductErrorCode::InvalidRequest);

    let mut unmanaged = ProductSession::new(
        ProductSessionId::new(21).ok_or("zero unmanaged session")?,
        ProductPrincipal::new("unmanaged-security-writer").ok_or("invalid principal")?,
        ProductAuthorization::ALL,
    );
    let Err(unmanaged_error) = dispatch(
        &mut fixture.product,
        &mut unmanaged,
        11,
        Some(201),
        operation.clone(),
    ) else {
        return Err("unmanaged ALL entered the security write plane".into());
    };
    assert_eq!(
        unmanaged_error.code(),
        ProductErrorCode::AuthorizationDenied
    );

    let _created = dispatch(&mut fixture.product, &mut owner, 12, Some(202), operation)?;
    let Err(conflict) = dispatch(
        &mut fixture.product,
        &mut owner,
        13,
        Some(202),
        ProductOperation::SecurityPrincipalCreate {
            display_name: "Different request".to_owned(),
        },
    ) else {
        return Err("one idempotency token identified two requests".into());
    };
    assert_eq!(conflict.code(), ProductErrorCode::IdempotencyConflict);
    Ok(())
}

#[test]
fn generic_owner_assignment_and_proof_wrapping_are_rejected() -> Result<(), Box<dyn Error>> {
    let mut fixture = SecurityWriteFixture::create("owner-proof")?;
    let mut owner = fixture.owner_session(30)?;
    let principal = dispatch(
        &mut fixture.product,
        &mut owner,
        20,
        Some(301),
        ProductOperation::SecurityPrincipalCreate {
            display_name: "Candidate".to_owned(),
        },
    )?;
    let ProductResponse::SecurityPrincipalMutated(principal) = principal else {
        return Err("principal creation returned the wrong response".into());
    };
    let Err(owner_grant) = dispatch(
        &mut fixture.product,
        &mut owner,
        21,
        Some(302),
        ProductOperation::SecurityBuiltInAssignmentCreate {
            principal_id: principal.principal_id,
            role: BuiltInRole::Owner,
            scope: ProductScope::Instance,
        },
    ) else {
        return Err("generic assignment created an owner".into());
    };
    assert!(matches!(
        owner_grant.code(),
        ProductErrorCode::AuthorizationDenied | ProductErrorCode::InvalidRequest
    ));

    let Err(proof) = dispatch(
        &mut fixture.product,
        &mut owner,
        22,
        Some(303),
        ProductOperation::Prove {
            operation: Box::new(ProductOperation::SecurityPrincipalCreate {
                display_name: "Proven mutation".to_owned(),
            }),
            limits: NativeProofGenerationLimits::default(),
        },
    ) else {
        return Err("security write became a proof input".into());
    };
    assert_eq!(proof.code(), ProductErrorCode::InvalidRequest);
    Ok(())
}
