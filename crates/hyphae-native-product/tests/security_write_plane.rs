// SPDX-License-Identifier: Apache-2.0

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
    role_keys: Vec<PathBuf>,
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
            role_keys: Vec::new(),
        })
    }

    fn owner_session(&self, id: u128) -> Result<ProductSession, Box<dyn Error>> {
        let authority = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        Ok(ProductSession::new_authenticated(
            ProductSessionId::new(id).ok_or("zero owner session")?,
            authority,
        ))
    }

    fn role_session(
        &mut self,
        role: BuiltInRole,
        id: u128,
    ) -> Result<ProductSession, Box<dyn Error>> {
        if role == BuiltInRole::Owner {
            return self.owner_session(id);
        }
        let actor_name = format!("{} actor", role.as_str());
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let principal = self
            .product
            .create_security_principal(&owner, &actor_name, 10)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product
            .set_security_principal_enabled(&owner, principal.principal_id, true, 11)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product.assign_built_in_role(
            &owner,
            principal.principal_id,
            role,
            ProductScope::Instance,
            12,
        )?;
        let key_path = self
            .directory
            .with_extension(format!("{}-key", role.as_str()));
        let _ignored = fs::remove_file(&key_path);
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product.issue_api_key_to_file(
            &owner,
            principal.principal_id,
            role.as_str(),
            [role],
            role.authorization(),
            None,
            &key_path,
            13,
        )?;
        let secret = fs::read_to_string(&key_path)?;
        let authority = self.product.authenticate_api_key(&secret, 0)?;
        self.role_keys.push(key_path);
        Ok(ProductSession::new_authenticated(
            ProductSessionId::new(id).ok_or("zero role session")?,
            authority,
        ))
    }

    fn valid_write_operations(
        &mut self,
    ) -> Result<Vec<(&'static str, ProductOperation)>, Box<dyn Error>> {
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let principal =
            self.product
                .create_security_principal(&owner, "Denied-operation target", 20)?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        self.product
            .set_security_principal_enabled(&owner, principal.principal_id, true, 21)?;
        let grant = CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
            .ok_or("invalid role grant")?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let custom_role = self.product.create_custom_security_role(
            &owner,
            "Denied-operation role",
            [grant],
            22,
        )?;
        let owner = self.product.authenticate_api_key(&self.owner_secret, 0)?;
        let assignment = self.product.assign_built_in_role(
            &owner,
            principal.principal_id,
            BuiltInRole::Reader,
            ProductScope::Instance,
            23,
        )?;
        Ok(vec![
            (
                "principal create",
                ProductOperation::SecurityPrincipalCreate {
                    display_name: "Denied principal".to_owned(),
                },
            ),
            (
                "principal set enabled",
                ProductOperation::SecurityPrincipalSetEnabled {
                    principal_id: principal.principal_id,
                    enabled: false,
                },
            ),
            (
                "custom role create",
                ProductOperation::SecurityCustomRoleCreate {
                    display_name: "Denied role".to_owned(),
                    grants: vec![grant],
                },
            ),
            (
                "built-in assignment create",
                ProductOperation::SecurityBuiltInAssignmentCreate {
                    principal_id: principal.principal_id,
                    role: BuiltInRole::Writer,
                    scope: ProductScope::Instance,
                },
            ),
            (
                "custom assignment create",
                ProductOperation::SecurityCustomAssignmentCreate {
                    principal_id: principal.principal_id,
                    role_id: custom_role.role_id,
                },
            ),
            (
                "assignment revoke",
                ProductOperation::SecurityAssignmentRevoke {
                    assignment_id: assignment.assignment_id,
                },
            ),
        ])
    }
}

impl Drop for SecurityWriteFixture {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.directory);
        let _ignored = fs::remove_file(&self.owner_key);
        for path in &self.role_keys {
            let _ignored = fs::remove_file(path);
        }
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

const SECURITY_WRITE_ALLOWED_ROLES: [BuiltInRole; 2] = [BuiltInRole::Admin, BuiltInRole::Owner];
const SECURITY_WRITE_DENIED_ROLES: [BuiltInRole; 5] = [
    BuiltInRole::Auditor,
    BuiltInRole::Developer,
    BuiltInRole::Operator,
    BuiltInRole::Reader,
    BuiltInRole::Writer,
];

fn assert_security_writes_allowed(role: BuiltInRole) -> Result<(), Box<dyn Error>> {
    let mut fixture = SecurityWriteFixture::create(&format!("{}-allow", role.as_str()))?;
    let mut actor = fixture.role_session(role, 100)?;
    let principal = dispatch(
        &mut fixture.product,
        &mut actor,
        100,
        Some(1_001),
        ProductOperation::SecurityPrincipalCreate {
            display_name: "Role-matrix target".to_owned(),
        },
    )?;
    let ProductResponse::SecurityPrincipalMutated(principal) = principal else {
        return Err(format!("{} could not create a principal", role.as_str()).into());
    };
    assert!(matches!(
        dispatch(
            &mut fixture.product,
            &mut actor,
            101,
            Some(1_002),
            ProductOperation::SecurityPrincipalSetEnabled {
                principal_id: principal.principal_id,
                enabled: true,
            },
        )?,
        ProductResponse::SecurityMutated(_)
    ));
    let grant = CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
        .ok_or("invalid role-matrix grant")?;
    let custom_role = dispatch(
        &mut fixture.product,
        &mut actor,
        102,
        Some(1_003),
        ProductOperation::SecurityCustomRoleCreate {
            display_name: "Role-matrix custom role".to_owned(),
            grants: vec![grant],
        },
    )?;
    let ProductResponse::SecurityCustomRoleMutated(custom_role) = custom_role else {
        return Err(format!("{} could not create a custom role", role.as_str()).into());
    };
    let assignment = dispatch(
        &mut fixture.product,
        &mut actor,
        103,
        Some(1_004),
        ProductOperation::SecurityBuiltInAssignmentCreate {
            principal_id: principal.principal_id,
            role: BuiltInRole::Reader,
            scope: ProductScope::Instance,
        },
    )?;
    let ProductResponse::SecurityAssignmentMutated(assignment) = assignment else {
        return Err(format!("{} could not create an assignment", role.as_str()).into());
    };
    assert!(matches!(
        dispatch(
            &mut fixture.product,
            &mut actor,
            104,
            Some(1_005),
            ProductOperation::SecurityCustomAssignmentCreate {
                principal_id: principal.principal_id,
                role_id: custom_role.role_id,
            },
        )?,
        ProductResponse::SecurityAssignmentMutated(_)
    ));
    assert!(matches!(
        dispatch(
            &mut fixture.product,
            &mut actor,
            105,
            Some(1_006),
            ProductOperation::SecurityAssignmentRevoke {
                assignment_id: assignment.assignment_id,
            },
        )?,
        ProductResponse::SecurityMutated(_)
    ));
    Ok(())
}

fn assert_security_writes_denied(role: BuiltInRole) -> Result<(), Box<dyn Error>> {
    let mut fixture = SecurityWriteFixture::create(&format!("{}-deny", role.as_str()))?;
    let operations = fixture.valid_write_operations()?;
    let mut actor = fixture.role_session(role, 200)?;
    for (index, (operation_name, operation)) in operations.into_iter().enumerate() {
        let Err(error) = dispatch(
            &mut fixture.product,
            &mut actor,
            200 + u128::try_from(index)?,
            Some(2_001 + u128::try_from(index)?),
            operation,
        ) else {
            return Err(format!("{} unexpectedly executed {operation_name}", role.as_str()).into());
        };
        assert_eq!(
            error.code(),
            ProductErrorCode::AuthorizationDenied,
            "{} received the wrong denial for {operation_name}",
            role.as_str()
        );
    }
    Ok(())
}

#[test]
fn built_in_role_matrix_partitions_every_managed_security_write() -> Result<(), Box<dyn Error>> {
    for role in SECURITY_WRITE_ALLOWED_ROLES {
        assert_security_writes_allowed(role)?;
    }
    for role in SECURITY_WRITE_DENIED_ROLES {
        assert_security_writes_denied(role)?;
    }
    Ok(())
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
