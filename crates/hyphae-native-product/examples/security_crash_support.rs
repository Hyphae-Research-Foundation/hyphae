// SPDX-License-Identifier: Apache-2.0

//! Shared exhaustive crash/recovery matrix support for security mutations.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_product::{
    ApiKeyConfirmationDigest, ApiKeyId, AuthorizationEpoch, BuiltInRole, CustomRoleGrant,
    LegacyBearerState, NativeProduct, ProductError, ProductErrorCode, ProductOperation,
    ProductPermission, ProductRequestContext, ProductResponse, ProductScope, ProductSession,
    ProductSessionId, SecurityAuditAction, SecurityId, SecurityKeyListRequest,
};
use hyphae_native_runtime::CommitBoundary;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const TOKEN: u128 = 0x5ec0_0001;
const SETUP_TOKEN: u128 = 0x5ec0_1000;
const LEGACY_BEARER: &[u8] = b"security-crash-legacy-bearer-0123456789abcdef";

#[allow(dead_code, clippy::too_many_lines)]
const BOUNDARIES: [CommitBoundary; 7] = [
    CommitBoundary::BlobStaged,
    CommitBoundary::BlobPromoted,
    CommitBoundary::PageAppended,
    CommitBoundary::PageSynchronized,
    CommitBoundary::WalAppended,
    CommitBoundary::WalSynchronized,
    CommitBoundary::RootPublished,
];

#[derive(Clone, Copy, Debug)]
pub(crate) enum ManagedCase {
    PrincipalCreate,
    PrincipalSetEnabled,
    CustomRoleCreate,
    BuiltInAssignmentCreate,
    CustomAssignmentCreate,
    AssignmentRevoke,
    KeyIssueSelfStart,
    KeyIssueStart,
    KeyIssueSelfActivate,
    KeyIssueActivate,
    KeyRotateSelfStart,
    KeyRotateStart,
    KeyRotateSelfActivate,
    KeyRotateActivate,
    KeyIssueSelfAbort,
    KeyIssueAbort,
    KeyRotateSelfAbort,
    KeyRotateAbort,
    KeyRevokeSelf,
    KeyRevoke,
    LegacyBearerRevoke,
}

#[allow(dead_code)]
pub(crate) const MANAGED_CASES: [ManagedCase; 21] = [
    ManagedCase::PrincipalCreate,
    ManagedCase::PrincipalSetEnabled,
    ManagedCase::CustomRoleCreate,
    ManagedCase::BuiltInAssignmentCreate,
    ManagedCase::CustomAssignmentCreate,
    ManagedCase::AssignmentRevoke,
    ManagedCase::KeyIssueSelfStart,
    ManagedCase::KeyIssueStart,
    ManagedCase::KeyIssueSelfActivate,
    ManagedCase::KeyIssueActivate,
    ManagedCase::KeyRotateSelfStart,
    ManagedCase::KeyRotateStart,
    ManagedCase::KeyRotateSelfActivate,
    ManagedCase::KeyRotateActivate,
    ManagedCase::KeyIssueSelfAbort,
    ManagedCase::KeyIssueAbort,
    ManagedCase::KeyRotateSelfAbort,
    ManagedCase::KeyRotateAbort,
    ManagedCase::KeyRevokeSelf,
    ManagedCase::KeyRevoke,
    ManagedCase::LegacyBearerRevoke,
];

#[derive(Clone, Copy, Debug)]
pub(crate) enum OfflineCase {
    OwnerRecoveryStart,
    OwnerRecoveryActivate,
    OwnerRecoveryAbort,
    LegacyBearerMigrate,
    LegacyBearerActivate,
}

#[allow(dead_code)]
pub(crate) const OFFLINE_CASES: [OfflineCase; 5] = [
    OfflineCase::OwnerRecoveryStart,
    OfflineCase::OwnerRecoveryActivate,
    OfflineCase::OwnerRecoveryAbort,
    OfflineCase::LegacyBearerMigrate,
    OfflineCase::LegacyBearerActivate,
];

struct TestDirectory {
    root: PathBuf,
    data: PathBuf,
    owner_key: PathBuf,
    actor_key: PathBuf,
    extra_keys: Vec<PathBuf>,
}

impl TestDirectory {
    fn create(label: &str) -> Result<Self, Box<dyn Error>> {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "hyphae-security-crash-{label}-{}-{suffix}",
            std::process::id()
        ));
        let data = root.join("data");
        let owner_key = root.join("owner.key");
        let actor_key = root.join("actor.key");
        let _ignored = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            data,
            owner_key,
            actor_key,
            extra_keys: Vec::new(),
        })
    }

    fn extra_key(&mut self, label: &str) -> PathBuf {
        let path = self.root.join(format!("{label}.key"));
        self.extra_keys.push(path.clone());
        path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.root);
    }
}

struct ManagedPrepared {
    directory: TestDirectory,
    product: NativeProduct,
    owner_secret: String,
    actor_secret: String,
    operation: ProductOperation,
    conflicting: ProductOperation,
    target_key: Option<ApiKeyId>,
    baseline_epoch: AuthorizationEpoch,
    baseline_audits: usize,
    baseline_status: hyphae_native_product::AccessControlStatus,
    known_secrets: Vec<String>,
}

#[allow(dead_code)]
pub(crate) fn every_public_security_mutation_recovers_at_every_real_commit_boundary()
-> Result<(), Box<dyn Error>> {
    assert_eq!(MANAGED_CASES.len() * BOUNDARIES.len(), 147);
    for case in MANAGED_CASES {
        for boundary in BOUNDARIES {
            assert_managed_case(case, boundary)?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn every_offline_security_transition_recovers_at_every_real_commit_boundary()
-> Result<(), Box<dyn Error>> {
    assert_eq!(OFFLINE_CASES.len() * BOUNDARIES.len(), 35);
    for case in OFFLINE_CASES {
        for boundary in BOUNDARIES {
            assert_offline_case(case, boundary)?;
        }
    }
    Ok(())
}

#[allow(dead_code, clippy::too_many_lines)]
pub(crate) fn self_terminal_mutations_replay_after_the_actor_key_is_retired()
-> Result<(), Box<dyn Error>> {
    let mut directory = TestDirectory::create("self-terminal-replay")?;
    let mut product = NativeProduct::create(&directory.data)?;
    product.bootstrap_access_control_to_file("Owner", "owner", &directory.owner_key, 1)?;
    let owner_secret = fs::read_to_string(&directory.owner_key)?;
    let owner = product.authenticate_api_key(&owner_secret, 0)?;
    let rotated = product.start_api_key_rotation_idempotent(
        &owner,
        owner.key_id(),
        "zero-overlap",
        0,
        None,
        0x701,
        2,
        true,
    )?;
    let successor = rotated.secret.take().ok_or("missing successor secret")?;
    let activated = product.activate_api_key_rotation_idempotent(
        &owner,
        rotated.key_id,
        successor.confirmation_digest(),
        0x702,
        3,
        true,
    )?;
    let replay = product.activate_api_key_rotation_idempotent(
        &owner,
        rotated.key_id,
        successor.confirmation_digest(),
        0x702,
        4,
        true,
    )?;
    assert_eq!(replay, activated);
    assert_eq!(replay.predecessor_key_id, Some(owner.key_id()));
    let mut old_session = ProductSession::new_authenticated(
        ProductSessionId::new(0x704).ok_or("zero old session")?,
        owner.clone(),
    );
    let old_context = ProductRequestContext::new(
        0x704,
        old_session.id(),
        4,
        old_session.principal().clone(),
        old_session.authorization(),
    )
    .with_authorization_epoch(old_session.authorization_epoch())
    .with_idempotency_token(0x702);
    let ProductResponse::SecurityApiKeyActivated(dispatched_replay) = product.dispatch(
        &mut old_session,
        &old_context,
        ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id: rotated.key_id,
            confirmation_digest: successor.confirmation_digest(),
        },
    )?
    else {
        return Err("dispatch replay returned the wrong activation response".into());
    };
    assert_eq!(dispatched_replay, activated);

    let self_key_path = directory.extra_key("self-terminal-revoke");
    let successor_actor = product.authenticate_api_key(successor.expose_secret(), 0)?;
    product.issue_api_key_to_file(
        &successor_actor,
        successor_actor.principal_id(),
        "self-terminal-revoke",
        [BuiltInRole::Owner],
        BuiltInRole::Owner.authorization(),
        None,
        &self_key_path,
        5,
    )?;
    let self_secret = fs::read_to_string(self_key_path)?;
    let self_actor = product.authenticate_api_key(&self_secret, 0)?;
    let self_key_id = self_actor.key_id();
    let revoked =
        product.revoke_api_key_idempotent(&self_actor, self_actor.key_id(), 0x703, 6, true)?;
    let revoke_replay =
        product.revoke_api_key_idempotent(&self_actor, self_actor.key_id(), 0x703, 7, true)?;
    assert_eq!(revoke_replay, revoked);
    let mut revoked_session = ProductSession::new_authenticated(
        ProductSessionId::new(0x705).ok_or("zero revoked session")?,
        self_actor,
    );
    let revoked_context = ProductRequestContext::new(
        0x705,
        revoked_session.id(),
        7,
        revoked_session.principal().clone(),
        revoked_session.authorization(),
    )
    .with_authorization_epoch(revoked_session.authorization_epoch())
    .with_idempotency_token(0x703);
    let ProductResponse::SecurityMutated(dispatched_revoke) = product.dispatch(
        &mut revoked_session,
        &revoked_context,
        ProductOperation::SecurityApiKeyRevokeSelf {
            key_id: self_key_id,
        },
    )?
    else {
        return Err("dispatch replay returned the wrong revoke response".into());
    };
    assert_eq!(dispatched_revoke, revoked);
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn assert_managed_case(
    case: ManagedCase,
    boundary: CommitBoundary,
) -> Result<(), Box<dyn Error>> {
    let mut prepared = prepare_managed(case)?;
    let expected_variant = managed_variant(case);
    if prepared.operation.security_mutation_registry_name() != Some(expected_variant) {
        return Err(format!("{case:?} did not execute {expected_variant}").into());
    }
    let mut session = authenticated_session(&prepared.product, &prepared.actor_secret, 1)?;
    prepared
        .product
        .interrupt_next_security_commit_for_test(boundary);
    let interrupted = dispatch(
        &mut prepared.product,
        &mut session,
        1,
        TOKEN,
        prepared.operation.clone(),
    );
    if interrupted.is_ok() {
        return Err(format!("{case:?} at {boundary:?} was not interrupted").into());
    }
    drop(session);
    drop(prepared.product);

    let mut product = NativeProduct::open(&prepared.directory.data)?;
    let owner = product.authenticate_api_key(&prepared.owner_secret, 0)?;
    let actor = product.authenticate_api_key(&prepared.actor_secret, 0)?;
    let committed = expects_complete(boundary);
    let recovered_status = product.access_control_status()?;
    let recovered_audits = product.security_audit_count_for_test()?;
    let recovered_marker = product.security_mutation_observation_for_test(&actor, TOKEN)?;
    let recovered_target = prepared
        .target_key
        .or_else(|| recovered_marker.and_then(|marker| marker.result_key_id));
    assert_eq!(
        recovered_marker.is_some(),
        committed,
        "{case:?} {boundary:?}"
    );
    assert_eq!(
        recovered_status.epoch,
        if committed {
            prepared
                .baseline_epoch
                .checked_next()
                .ok_or("epoch overflow")?
        } else {
            prepared.baseline_epoch
        },
        "{case:?} {boundary:?} epoch"
    );
    assert_eq!(
        recovered_audits,
        prepared.baseline_audits + usize::from(committed),
        "{case:?} {boundary:?} audit"
    );
    assert_recovered_shape(
        case,
        &product,
        &owner,
        recovered_target,
        committed,
        &prepared.baseline_status,
    )?;
    assert!(
        product
            .authenticate_api_key(&prepared.owner_secret, 0)
            .is_ok()
    );

    let mut session = authenticated_session(&product, &prepared.actor_secret, 2)?;
    let first = dispatch(
        &mut product,
        &mut session,
        2,
        TOKEN,
        prepared.operation.clone(),
    );
    let start = is_start(case);
    if committed && start {
        assert_error_code(first, ProductErrorCode::SecretDeliveryConsumed)?;
    } else {
        let first = first?;
        if let ProductResponse::SecurityApiKeyStarted(started) = &first
            && let Some(secret) = started.secret.take()
        {
            prepared
                .known_secrets
                .push(secret.expose_secret().to_owned());
        }
        let second = dispatch(
            &mut product,
            &mut session,
            3,
            TOKEN,
            prepared.operation.clone(),
        );
        if start {
            assert_error_code(second, ProductErrorCode::SecretDeliveryConsumed)?;
        } else {
            assert_eq!(second?, first, "{case:?} replay receipt changed");
        }
    }
    assert_error_code(
        dispatch(&mut product, &mut session, 4, TOKEN, prepared.conflicting),
        ProductErrorCode::IdempotencyConflict,
    )
    .map_err(|error| format!("{case:?} {boundary:?} conflict check failed: {error}"))?;
    let marker = product
        .security_mutation_observation_for_test(&actor, TOKEN)?
        .ok_or("eventual mutation marker is absent")?;
    if committed {
        assert_eq!(
            recovered_marker,
            Some(marker),
            "{case:?} marker changed on retry"
        );
    }
    let final_status = product.access_control_status()?;
    assert_eq!(
        final_status.epoch,
        prepared
            .baseline_epoch
            .checked_next()
            .ok_or("epoch overflow")?
    );
    assert_eq!(
        product.security_audit_count_for_test()?,
        prepared.baseline_audits + 1
    );
    assert_eq!(marker.authorization_epoch, final_status.epoch);
    let audit = product.read_security_audit(&owner, None, 1_000, 5)?;
    let mutation_event = audit
        .events
        .iter()
        .filter(|event| event.commit_csn() == marker.commit.commit_csn)
        .count();
    assert_eq!(mutation_event, 1, "{case:?} audit/CSN coherence");
    assert_recovered_shape(
        case,
        &product,
        &owner,
        match case {
            ManagedCase::KeyIssueSelfStart
            | ManagedCase::KeyIssueStart
            | ManagedCase::KeyRotateSelfStart
            | ManagedCase::KeyRotateStart => marker.result_key_id,
            _ => prepared.target_key.or(marker.result_key_id),
        },
        true,
        &prepared.baseline_status,
    )?;
    assert_no_serialized_secret(&prepared.directory.data, &prepared.known_secrets)?;
    drop(owner);
    drop(actor);
    drop(session);
    drop(product);
    let disconnected = NativeProduct::open(&prepared.directory.data)?;
    let final_status = disconnected.access_control_status()?;
    assert_eq!(final_status.epoch, marker.authorization_epoch);
    assert_eq!(
        disconnected.security_audit_count_for_test()?,
        prepared.baseline_audits + 1
    );
    Ok(())
}

fn prepare_managed(case: ManagedCase) -> Result<ManagedPrepared, Box<dyn Error>> {
    if matches!(case, ManagedCase::LegacyBearerRevoke) {
        return prepare_legacy_revoke();
    }
    let mut directory = TestDirectory::create(&format!("{case:?}"))?;
    let mut product = NativeProduct::create(&directory.data)?;
    product.bootstrap_access_control_to_file("Owner", "owner", &directory.owner_key, 1)?;
    let owner_secret = fs::read_to_string(&directory.owner_key)?;
    let owner = product.authenticate_api_key(&owner_secret, 0)?;
    let admin = product.create_security_principal(&owner, "Admin actor", 2)?;
    let owner = product.authenticate_api_key(&owner_secret, 0)?;
    product.set_security_principal_enabled(&owner, admin.principal_id, true, 3)?;
    let owner = product.authenticate_api_key(&owner_secret, 0)?;
    product.assign_built_in_role(
        &owner,
        admin.principal_id,
        BuiltInRole::Admin,
        ProductScope::Instance,
        4,
    )?;
    let owner = product.authenticate_api_key(&owner_secret, 0)?;
    product.issue_api_key_to_file(
        &owner,
        admin.principal_id,
        "admin-actor",
        [BuiltInRole::Admin],
        BuiltInRole::Admin.authorization(),
        None,
        &directory.actor_key,
        5,
    )?;
    let actor_secret = fs::read_to_string(&directory.actor_key)?;
    let mut known_secrets = vec![owner_secret.clone(), actor_secret.clone()];
    let (operation, conflicting, target_key) = prepare_operation(
        case,
        &mut product,
        &mut directory,
        &owner_secret,
        &actor_secret,
        &mut known_secrets,
        admin.principal_id,
    )?;
    Ok(ManagedPrepared {
        baseline_epoch: product.access_control_status()?.epoch,
        baseline_audits: product.security_audit_count_for_test()?,
        baseline_status: product.access_control_status()?,
        directory,
        product,
        owner_secret: owner_secret.clone(),
        actor_secret: if matches!(case, ManagedCase::KeyRevoke) {
            owner_secret.clone()
        } else {
            actor_secret
        },
        operation,
        conflicting,
        target_key,
        known_secrets,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn prepare_operation(
    case: ManagedCase,
    product: &mut NativeProduct,
    directory: &mut TestDirectory,
    owner_secret: &str,
    actor_secret: &str,
    known_secrets: &mut Vec<String>,
    admin_principal: SecurityId,
) -> Result<(ProductOperation, ProductOperation, Option<ApiKeyId>), Box<dyn Error>> {
    let grant = CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
        .ok_or("invalid test grant")?;
    let unknown_security = SecurityId::new(u128::MAX).ok_or("invalid unknown security ID")?;
    let unknown_key = ApiKeyId::from_bytes([0xee; 16]).ok_or("invalid unknown key ID")?;
    let mut setup_token = SETUP_TOKEN;
    let mut new_principal = |product: &mut NativeProduct,
                             owner_secret: &str,
                             label: &str|
     -> Result<SecurityId, Box<dyn Error>> {
        setup_token += 1;
        let owner = product.authenticate_api_key(owner_secret, 0)?;
        Ok(product
            .create_security_principal_idempotent(&owner, label, setup_token, 10)?
            .principal_id)
    };
    let prepared = match case {
        ManagedCase::PrincipalCreate => (
            ProductOperation::SecurityPrincipalCreate {
                display_name: "Crash principal".to_owned(),
            },
            ProductOperation::SecurityPrincipalCreate {
                display_name: "Different principal".to_owned(),
            },
            None,
        ),
        ManagedCase::PrincipalSetEnabled => {
            let principal = new_principal(product, owner_secret, "Enable target")?;
            (
                ProductOperation::SecurityPrincipalSetEnabled {
                    principal_id: principal,
                    enabled: true,
                },
                ProductOperation::SecurityPrincipalSetEnabled {
                    principal_id: principal,
                    enabled: false,
                },
                None,
            )
        }
        ManagedCase::CustomRoleCreate => (
            ProductOperation::SecurityCustomRoleCreate {
                display_name: "Crash role".to_owned(),
                grants: vec![grant],
            },
            ProductOperation::SecurityCustomRoleCreate {
                display_name: "Different role".to_owned(),
                grants: vec![grant],
            },
            None,
        ),
        ManagedCase::BuiltInAssignmentCreate => {
            let principal = new_principal(product, owner_secret, "Built-in target")?;
            (
                ProductOperation::SecurityBuiltInAssignmentCreate {
                    principal_id: principal,
                    role: BuiltInRole::Reader,
                    scope: ProductScope::Instance,
                },
                ProductOperation::SecurityBuiltInAssignmentCreate {
                    principal_id: unknown_security,
                    role: BuiltInRole::Reader,
                    scope: ProductScope::Instance,
                },
                None,
            )
        }
        ManagedCase::CustomAssignmentCreate => {
            let principal = new_principal(product, owner_secret, "Custom target")?;
            let owner = product.authenticate_api_key(owner_secret, 0)?;
            let role = product.create_custom_security_role_idempotent(
                &owner,
                "Crash custom role",
                [grant],
                SETUP_TOKEN + 20,
                11,
            )?;
            (
                ProductOperation::SecurityCustomAssignmentCreate {
                    principal_id: principal,
                    role_id: role.role_id,
                },
                ProductOperation::SecurityCustomAssignmentCreate {
                    principal_id: principal,
                    role_id: unknown_security,
                },
                None,
            )
        }
        ManagedCase::AssignmentRevoke => {
            let principal = new_principal(product, owner_secret, "Revoke assignment target")?;
            let owner = product.authenticate_api_key(owner_secret, 0)?;
            let assignment = product.assign_built_in_role_idempotent(
                &owner,
                principal,
                BuiltInRole::Reader,
                ProductScope::Instance,
                SETUP_TOKEN + 21,
                11,
            )?;
            (
                ProductOperation::SecurityAssignmentRevoke {
                    assignment_id: assignment.assignment_id,
                },
                ProductOperation::SecurityAssignmentRevoke {
                    assignment_id: unknown_security,
                },
                None,
            )
        }
        ManagedCase::KeyIssueSelfStart | ManagedCase::KeyIssueStart => {
            let self_manage = matches!(case, ManagedCase::KeyIssueSelfStart);
            let operation = key_issue_start(admin_principal, "Crash issue", self_manage);
            let conflicting = key_issue_start(admin_principal, "Different issue", self_manage);
            (operation, conflicting, None)
        }
        ManagedCase::KeyIssueSelfActivate
        | ManagedCase::KeyIssueActivate
        | ManagedCase::KeyIssueSelfAbort
        | ManagedCase::KeyIssueAbort => {
            let self_manage = matches!(
                case,
                ManagedCase::KeyIssueSelfActivate | ManagedCase::KeyIssueSelfAbort
            );
            let actor = product.authenticate_api_key(
                if self_manage {
                    actor_secret
                } else {
                    owner_secret
                },
                0,
            )?;
            let started = product.start_api_key_issue_idempotent(
                &actor,
                admin_principal,
                "Pending issue",
                [BuiltInRole::Admin],
                [],
                BuiltInRole::Admin.authorization(),
                [ProductScope::Instance],
                None,
                SETUP_TOKEN + 30,
                12,
                self_manage,
            )?;
            let secret = started.secret.take().ok_or("missing setup issue secret")?;
            let digest = secret.confirmation_digest();
            known_secrets.push(secret.expose_secret().to_owned());
            let key_id = started.key_id;
            match case {
                ManagedCase::KeyIssueSelfActivate | ManagedCase::KeyIssueActivate => (
                    key_issue_activate(key_id, digest, self_manage),
                    key_issue_activate(
                        key_id,
                        ApiKeyConfirmationDigest::from_bytes([0x44; 32]),
                        self_manage,
                    ),
                    Some(key_id),
                ),
                _ => (
                    key_issue_abort(key_id, self_manage),
                    key_issue_abort(unknown_key, self_manage),
                    Some(key_id),
                ),
            }
        }
        ManagedCase::KeyRotateSelfStart
        | ManagedCase::KeyRotateStart
        | ManagedCase::KeyRotateSelfActivate
        | ManagedCase::KeyRotateActivate
        | ManagedCase::KeyRotateSelfAbort
        | ManagedCase::KeyRotateAbort => {
            let self_manage = matches!(
                case,
                ManagedCase::KeyRotateSelfStart
                    | ManagedCase::KeyRotateSelfActivate
                    | ManagedCase::KeyRotateSelfAbort
            );
            let predecessor = product.authenticate_api_key(actor_secret, 0)?.key_id();
            match case {
                ManagedCase::KeyRotateSelfStart | ManagedCase::KeyRotateStart => (
                    key_rotate_start(predecessor, "Crash rotation", self_manage),
                    key_rotate_start(predecessor, "Different rotation", self_manage),
                    None,
                ),
                _ => {
                    let actor = product.authenticate_api_key(
                        if self_manage {
                            actor_secret
                        } else {
                            owner_secret
                        },
                        0,
                    )?;
                    let started = product.start_api_key_rotation_idempotent(
                        &actor,
                        predecessor,
                        "Pending rotation",
                        60,
                        None,
                        SETUP_TOKEN + 40,
                        13,
                        self_manage,
                    )?;
                    let secret = started
                        .secret
                        .take()
                        .ok_or("missing setup rotation secret")?;
                    let digest = secret.confirmation_digest();
                    known_secrets.push(secret.expose_secret().to_owned());
                    let successor = started.key_id;
                    if matches!(
                        case,
                        ManagedCase::KeyRotateSelfActivate | ManagedCase::KeyRotateActivate
                    ) {
                        (
                            key_rotate_activate(successor, digest, self_manage),
                            key_rotate_activate(
                                successor,
                                ApiKeyConfirmationDigest::from_bytes([0x55; 32]),
                                self_manage,
                            ),
                            Some(successor),
                        )
                    } else {
                        (
                            key_rotate_abort(successor, self_manage),
                            key_rotate_abort(unknown_key, self_manage),
                            Some(successor),
                        )
                    }
                }
            }
        }
        ManagedCase::KeyRevokeSelf | ManagedCase::KeyRevoke => {
            let self_manage = matches!(case, ManagedCase::KeyRevokeSelf);
            let target = if self_manage {
                let path = directory.extra_key("self-revoke-target");
                let actor = product.authenticate_api_key(actor_secret, 0)?;
                product.issue_api_key_to_file(
                    &actor,
                    admin_principal,
                    "self-revoke-target",
                    [BuiltInRole::Admin],
                    BuiltInRole::Admin.authorization(),
                    None,
                    &path,
                    14,
                )?;
                let secret = fs::read_to_string(path)?;
                known_secrets.push(secret.clone());
                product.authenticate_api_key(&secret, 0)?.key_id()
            } else {
                product.authenticate_api_key(actor_secret, 0)?.key_id()
            };
            (
                key_revoke(target, self_manage),
                key_revoke(target, !self_manage),
                Some(target),
            )
        }
        ManagedCase::LegacyBearerRevoke => unreachable!(),
    };
    Ok(prepared)
}

fn prepare_legacy_revoke() -> Result<ManagedPrepared, Box<dyn Error>> {
    let directory = TestDirectory::create("legacy-revoke")?;
    let product = NativeProduct::create(&directory.data)?;
    drop(product);
    let mut product = NativeProduct::open_offline_owner(&directory.data)?;
    let started = product.start_legacy_bearer_migration_offline(
        "Legacy owner",
        "canonical-owner",
        LEGACY_BEARER,
        1,
    )?;
    let owner_secret = started.secret.expose_secret().to_owned();
    product.activate_legacy_bearer_migration_offline(
        started.key_id,
        &owner_secret,
        started.authorization_epoch,
        "Legacy owner",
        "canonical-owner",
        LEGACY_BEARER,
        2,
    )?;
    fs::write(&directory.owner_key, &owner_secret)?;
    drop(product);
    let product = NativeProduct::open(&directory.data)?;
    Ok(ManagedPrepared {
        baseline_epoch: product.access_control_status()?.epoch,
        baseline_audits: product.security_audit_count_for_test()?,
        baseline_status: product.access_control_status()?,
        directory,
        product,
        owner_secret: owner_secret.clone(),
        actor_secret: owner_secret.clone(),
        operation: ProductOperation::SecurityLegacyBearerRevoke,
        conflicting: ProductOperation::SecurityPrincipalCreate {
            display_name: "Different legacy token operation".to_owned(),
        },
        target_key: Some(started.key_id),
        known_secrets: vec![owner_secret],
    })
}

#[allow(clippy::too_many_lines)]
fn assert_recovered_shape(
    case: ManagedCase,
    product: &NativeProduct,
    owner: &hyphae_native_product::AuthenticatedAuthority,
    target_key: Option<ApiKeyId>,
    complete: bool,
    baseline: &hyphae_native_product::AccessControlStatus,
) -> Result<(), Box<dyn Error>> {
    let status = product.access_control_status()?;
    let (principal_delta, role_delta, assignment_delta, key_delta, pending_delta) = match case {
        ManagedCase::PrincipalCreate => (usize::from(complete), 0, 0_isize, 0_isize, 0_isize),
        ManagedCase::CustomRoleCreate => (0, usize::from(complete), 0, 0, 0),
        ManagedCase::BuiltInAssignmentCreate | ManagedCase::CustomAssignmentCreate => {
            (0, 0, isize::from(complete), 0, 0)
        }
        ManagedCase::AssignmentRevoke => (0, 0, -isize::from(complete), 0, 0),
        ManagedCase::KeyIssueSelfStart
        | ManagedCase::KeyIssueStart
        | ManagedCase::KeyRotateSelfStart
        | ManagedCase::KeyRotateStart => (0, 0, 0, isize::from(complete), isize::from(complete)),
        ManagedCase::KeyIssueSelfActivate
        | ManagedCase::KeyIssueActivate
        | ManagedCase::KeyRotateSelfActivate
        | ManagedCase::KeyRotateActivate => (0, 0, 0, 0, -isize::from(complete)),
        ManagedCase::KeyIssueSelfAbort
        | ManagedCase::KeyIssueAbort
        | ManagedCase::KeyRotateSelfAbort
        | ManagedCase::KeyRotateAbort => (0, 0, 0, -isize::from(complete), -isize::from(complete)),
        ManagedCase::PrincipalSetEnabled
        | ManagedCase::KeyRevokeSelf
        | ManagedCase::KeyRevoke
        | ManagedCase::LegacyBearerRevoke => (0, 0, 0, 0, 0),
    };
    assert_eq!(status.principals, baseline.principals + principal_delta);
    assert_eq!(status.custom_roles, baseline.custom_roles + role_delta);
    assert_eq!(
        isize::try_from(status.assignments + status.custom_assignments)?,
        isize::try_from(baseline.assignments + baseline.custom_assignments)? + assignment_delta
    );
    assert_eq!(
        isize::try_from(status.keys)?,
        isize::try_from(baseline.keys)? + key_delta
    );
    assert_eq!(
        isize::try_from(status.pending_keys)?,
        isize::try_from(baseline.pending_keys)? + pending_delta
    );
    if let Some(key_id) = target_key {
        let keys = product.read_security_keys_for_test(
            owner,
            SecurityKeyListRequest::new(None, 1_000)?,
            0,
        )?;
        let key = keys.items().iter().find(|key| key.id() == key_id);
        match case {
            ManagedCase::KeyIssueSelfActivate
            | ManagedCase::KeyIssueActivate
            | ManagedCase::KeyRotateSelfActivate
            | ManagedCase::KeyRotateActivate
                if complete =>
            {
                assert!(key.is_some_and(|key| key.active() && !key.revoked()));
            }
            ManagedCase::KeyIssueSelfAbort
            | ManagedCase::KeyIssueAbort
            | ManagedCase::KeyRotateSelfAbort
            | ManagedCase::KeyRotateAbort
                if complete =>
            {
                assert!(key.is_none());
            }
            ManagedCase::KeyRevokeSelf | ManagedCase::KeyRevoke if complete => {
                assert!(key.is_some_and(hyphae_native_product::SecurityKeySummary::revoked));
            }
            ManagedCase::KeyIssueSelfStart
            | ManagedCase::KeyIssueStart
            | ManagedCase::KeyRotateSelfStart
            | ManagedCase::KeyRotateStart
                if complete =>
            {
                assert!(key.is_some_and(|key| !key.active() && !key.revoked()));
            }
            _ => {}
        }
        if matches!(
            case,
            ManagedCase::KeyRotateSelfStart | ManagedCase::KeyRotateStart
        ) && complete
        {
            let key = key.ok_or("recovered rotation successor is absent")?;
            let predecessor = key
                .predecessor_id()
                .ok_or("recovered rotation successor has no predecessor")?;
            let predecessor = keys
                .items()
                .iter()
                .find(|candidate| candidate.id() == predecessor)
                .ok_or("recovered rotation predecessor is absent")?;
            assert_eq!(predecessor.successor_id(), Some(key.id()));
            assert!(predecessor.active() && !predecessor.revoked());
        }
    }
    if matches!(case, ManagedCase::LegacyBearerRevoke) {
        assert_eq!(
            product.legacy_bearer_migration_inspection()?.state,
            if complete {
                LegacyBearerState::Revoked
            } else {
                LegacyBearerState::DualWindow
            }
        );
    }
    Ok(())
}

pub(crate) fn assert_offline_case(
    case: OfflineCase,
    boundary: CommitBoundary,
) -> Result<(), Box<dyn Error>> {
    match case {
        OfflineCase::OwnerRecoveryStart => assert_owner_start(boundary),
        OfflineCase::OwnerRecoveryActivate => assert_owner_activate(boundary),
        OfflineCase::OwnerRecoveryAbort => assert_owner_abort(boundary),
        OfflineCase::LegacyBearerMigrate => assert_legacy_start(boundary),
        OfflineCase::LegacyBearerActivate => assert_legacy_activate(boundary),
    }
}

fn owner_directory(label: &str) -> Result<(TestDirectory, String), Box<dyn Error>> {
    let directory = TestDirectory::create(label)?;
    let mut product = NativeProduct::create(&directory.data)?;
    product.bootstrap_access_control_to_file("Owner", "owner", &directory.owner_key, 1)?;
    let secret = fs::read_to_string(&directory.owner_key)?;
    drop(product);
    Ok((directory, secret))
}

fn assert_owner_start(boundary: CommitBoundary) -> Result<(), Box<dyn Error>> {
    let (directory, old_secret) = owner_directory("owner-start")?;
    let mut product = NativeProduct::open_offline_owner(&directory.data)?;
    let baseline = product.access_control_status()?;
    let audits = product.security_audit_count_for_test()?;
    product.interrupt_next_security_commit_for_test(boundary);
    assert!(
        product
            .start_owner_recovery_offline("replacement", 2)
            .is_err()
    );
    drop(product);
    let mut reopened = NativeProduct::open_offline_owner(&directory.data)?;
    let complete = expects_complete(boundary);
    assert_eq!(
        reopened.inspect_owner_recovery_offline()?.pending.is_some(),
        complete
    );
    assert_eq!(
        reopened.access_control_status()?.epoch,
        if complete {
            baseline.epoch.checked_next().ok_or("epoch overflow")?
        } else {
            baseline.epoch
        }
    );
    assert_eq!(
        reopened.security_audit_count_for_test()?,
        audits + usize::from(complete)
    );
    if complete {
        assert_eq!(
            reopened
                .last_security_audit_for_test()?
                .ok_or("owner recovery start audit is absent")?
                .action(),
            SecurityAuditAction::RecoverOwner
        );
        let error = reopened
            .start_owner_recovery_offline("replacement", 3)
            .err()
            .ok_or("second owner recovery start replaced pending state")?;
        assert_eq!(error.code(), ProductErrorCode::CatalogConflict);
        let owner = reopened.authenticate_api_key(&old_secret, 0)?;
        let pending = reopened
            .inspect_owner_recovery_offline()?
            .pending
            .ok_or("recovered owner key is not pending")?;
        let keys = reopened.read_security_keys_for_test(
            &owner,
            SecurityKeyListRequest::new(None, 1_000)?,
            0,
        )?;
        assert!(
            keys.items()
                .iter()
                .any(|key| key.id() == pending.key_id() && !key.active() && !key.revoked())
        );
    }
    assert!(reopened.authenticate_api_key(&old_secret, 0).is_ok());
    if !complete {
        let started = reopened.start_owner_recovery_offline("replacement", 3)?;
        assert_eq!(
            started.principal_id,
            reopened
                .authenticate_api_key(&old_secret, 0)?
                .principal_id()
        );
    }
    assert_no_serialized_secret(&directory.data, &[old_secret])
}

fn assert_owner_activate(boundary: CommitBoundary) -> Result<(), Box<dyn Error>> {
    let (directory, old_secret) = owner_directory("owner-activate")?;
    let mut product = NativeProduct::open_offline_owner(&directory.data)?;
    let started = product.start_owner_recovery_offline("replacement", 2)?;
    let new_secret = started.secret.expose_secret().to_owned();
    let audits = product.security_audit_count_for_test()?;
    product.interrupt_next_security_commit_for_test(boundary);
    assert!(
        product
            .resume_owner_recovery_offline(
                started.key_id,
                &new_secret,
                started.authorization_epoch,
                3,
            )
            .is_err()
    );
    drop(product);
    let mut reopened = NativeProduct::open_offline_owner(&directory.data)?;
    let complete = expects_complete(boundary);
    assert_eq!(
        reopened.inspect_owner_recovery_offline()?.pending.is_none(),
        complete
    );
    assert_eq!(
        reopened.authenticate_api_key(&new_secret, 0).is_ok(),
        complete
    );
    assert_eq!(
        reopened.authenticate_api_key(&old_secret, 0).is_err(),
        complete
    );
    let receipt = reopened.resume_owner_recovery_offline(
        started.key_id,
        &new_secret,
        started.authorization_epoch,
        4,
    )?;
    assert_eq!(
        receipt,
        reopened.resume_owner_recovery_offline(
            started.key_id,
            &new_secret,
            started.authorization_epoch,
            5,
        )?
    );
    assert_eq!(reopened.security_audit_count_for_test()?, audits + 1);
    assert_eq!(
        reopened
            .last_security_audit_for_test()?
            .ok_or("owner activation audit is absent")?
            .commit_csn(),
        receipt.commit.commit_csn
    );
    assert_no_serialized_secret(&directory.data, &[old_secret, new_secret])
}

fn assert_owner_abort(boundary: CommitBoundary) -> Result<(), Box<dyn Error>> {
    let (directory, old_secret) = owner_directory("owner-abort")?;
    let mut product = NativeProduct::open_offline_owner(&directory.data)?;
    let started = product.start_owner_recovery_offline("abort", 2)?;
    let pending_secret = started.secret.expose_secret().to_owned();
    let audits = product.security_audit_count_for_test()?;
    product.interrupt_next_security_commit_for_test(boundary);
    assert!(
        product
            .abort_owner_recovery_offline(started.key_id, started.authorization_epoch, 3)
            .is_err()
    );
    drop(product);
    let mut reopened = NativeProduct::open_offline_owner(&directory.data)?;
    let complete = expects_complete(boundary);
    assert_eq!(
        reopened.inspect_owner_recovery_offline()?.pending.is_none(),
        complete
    );
    assert!(reopened.authenticate_api_key(&old_secret, 0).is_ok());
    assert!(reopened.authenticate_api_key(&pending_secret, 0).is_err());
    let receipt =
        reopened.abort_owner_recovery_offline(started.key_id, started.authorization_epoch, 4)?;
    assert_eq!(
        receipt,
        reopened.abort_owner_recovery_offline(started.key_id, started.authorization_epoch, 5,)?
    );
    assert_eq!(reopened.security_audit_count_for_test()?, audits + 1);
    assert_eq!(
        reopened
            .last_security_audit_for_test()?
            .ok_or("owner abort audit is absent")?
            .commit_csn(),
        receipt.commit.commit_csn
    );
    assert_no_serialized_secret(&directory.data, &[old_secret, pending_secret])
}

fn legacy_directory(label: &str) -> Result<TestDirectory, Box<dyn Error>> {
    let directory = TestDirectory::create(label)?;
    drop(NativeProduct::create(&directory.data)?);
    Ok(directory)
}

fn assert_legacy_start(boundary: CommitBoundary) -> Result<(), Box<dyn Error>> {
    let directory = legacy_directory("legacy-start")?;
    let mut product = NativeProduct::open_offline_owner(&directory.data)?;
    let baseline = product.access_control_status()?;
    product.interrupt_next_security_commit_for_test(boundary);
    assert!(
        product
            .start_legacy_bearer_migration_offline("Legacy owner", "canonical", LEGACY_BEARER, 1,)
            .is_err()
    );
    drop(product);
    let mut reopened = NativeProduct::open_offline_owner(&directory.data)?;
    let complete = expects_complete(boundary);
    assert_eq!(
        reopened.legacy_bearer_migration_inspection()?.state,
        if complete {
            LegacyBearerState::MigrationPending
        } else {
            LegacyBearerState::NeverEnabled
        }
    );
    assert_eq!(
        reopened.access_control_status()?.epoch,
        if complete {
            baseline.epoch.checked_next().ok_or("epoch overflow")?
        } else {
            baseline.epoch
        }
    );
    if complete {
        assert_eq!(
            reopened
                .last_security_audit_for_test()?
                .ok_or("legacy migration audit is absent")?
                .action(),
            SecurityAuditAction::MigrateLegacyBearer
        );
    }
    if complete {
        let error = reopened
            .start_legacy_bearer_migration_offline("Different owner", "canonical", LEGACY_BEARER, 2)
            .err()
            .ok_or("legacy start redelivered a secret after recovered commit")?;
        assert_eq!(error.code(), ProductErrorCode::CatalogConflict);
    } else {
        let started = reopened.start_legacy_bearer_migration_offline(
            "Legacy owner",
            "canonical",
            LEGACY_BEARER,
            2,
        )?;
        assert_eq!(
            started.key_id,
            reopened
                .legacy_bearer_migration_inspection()?
                .key_id
                .ok_or("missing key")?
        );
    }
    assert_no_serialized_secret(&directory.data, &[])
}

fn assert_legacy_activate(boundary: CommitBoundary) -> Result<(), Box<dyn Error>> {
    let directory = legacy_directory("legacy-activate")?;
    let mut product = NativeProduct::open_offline_owner(&directory.data)?;
    let started = product.start_legacy_bearer_migration_offline(
        "Legacy owner",
        "canonical",
        LEGACY_BEARER,
        1,
    )?;
    let secret = started.secret.expose_secret().to_owned();
    let audits = product.security_audit_count_for_test()?;
    product.interrupt_next_security_commit_for_test(boundary);
    assert!(
        product
            .activate_legacy_bearer_migration_offline(
                started.key_id,
                &secret,
                started.authorization_epoch,
                "Legacy owner",
                "canonical",
                LEGACY_BEARER,
                2,
            )
            .is_err()
    );
    drop(product);
    let mut reopened = NativeProduct::open_offline_owner(&directory.data)?;
    let complete = expects_complete(boundary);
    assert_eq!(
        reopened.legacy_bearer_migration_inspection()?.state,
        if complete {
            LegacyBearerState::DualWindow
        } else {
            LegacyBearerState::MigrationPending
        }
    );
    let receipt = reopened.activate_legacy_bearer_migration_offline(
        started.key_id,
        &secret,
        started.authorization_epoch,
        "Legacy owner",
        "canonical",
        LEGACY_BEARER,
        3,
    )?;
    assert_eq!(
        receipt,
        reopened.activate_legacy_bearer_migration_offline(
            started.key_id,
            &secret,
            started.authorization_epoch,
            "Legacy owner",
            "canonical",
            LEGACY_BEARER,
            4,
        )?
    );
    let error = reopened
        .activate_legacy_bearer_migration_offline(
            started.key_id,
            &secret,
            started.authorization_epoch,
            "Different owner",
            "canonical",
            LEGACY_BEARER,
            5,
        )
        .err()
        .ok_or("legacy activation accepted a different payload")?;
    assert_eq!(error.code(), ProductErrorCode::IdempotencyConflict);
    assert_eq!(reopened.security_audit_count_for_test()?, audits + 1);
    assert_eq!(
        reopened
            .last_security_audit_for_test()?
            .ok_or("legacy activation audit is absent")?
            .commit_csn(),
        receipt.commit.commit_csn
    );
    assert_no_serialized_secret(&directory.data, &[secret])
}

pub(crate) fn managed_variant(case: ManagedCase) -> &'static str {
    match case {
        ManagedCase::PrincipalCreate => "SecurityPrincipalCreate",
        ManagedCase::PrincipalSetEnabled => "SecurityPrincipalSetEnabled",
        ManagedCase::CustomRoleCreate => "SecurityCustomRoleCreate",
        ManagedCase::BuiltInAssignmentCreate => "SecurityBuiltInAssignmentCreate",
        ManagedCase::CustomAssignmentCreate => "SecurityCustomAssignmentCreate",
        ManagedCase::AssignmentRevoke => "SecurityAssignmentRevoke",
        ManagedCase::KeyIssueSelfStart => "SecurityApiKeyIssueSelfStart",
        ManagedCase::KeyIssueStart => "SecurityApiKeyIssueStart",
        ManagedCase::KeyIssueSelfActivate => "SecurityApiKeyIssueSelfActivate",
        ManagedCase::KeyIssueActivate => "SecurityApiKeyIssueActivate",
        ManagedCase::KeyRotateSelfStart => "SecurityApiKeyRotateSelfStart",
        ManagedCase::KeyRotateStart => "SecurityApiKeyRotateStart",
        ManagedCase::KeyRotateSelfActivate => "SecurityApiKeyRotateSelfActivate",
        ManagedCase::KeyRotateActivate => "SecurityApiKeyRotateActivate",
        ManagedCase::KeyIssueSelfAbort => "SecurityApiKeyIssueSelfAbort",
        ManagedCase::KeyIssueAbort => "SecurityApiKeyIssueAbort",
        ManagedCase::KeyRotateSelfAbort => "SecurityApiKeyRotateSelfAbort",
        ManagedCase::KeyRotateAbort => "SecurityApiKeyRotateAbort",
        ManagedCase::KeyRevokeSelf => "SecurityApiKeyRevokeSelf",
        ManagedCase::KeyRevoke => "SecurityApiKeyRevoke",
        ManagedCase::LegacyBearerRevoke => "SecurityLegacyBearerRevoke",
    }
}

fn key_issue_start(principal_id: SecurityId, label: &str, self_manage: bool) -> ProductOperation {
    let fields = (
        principal_id,
        label.to_owned(),
        vec![BuiltInRole::Admin],
        Vec::new(),
        BuiltInRole::Admin.authorization(),
        vec![ProductScope::Instance],
        None,
    );
    if self_manage {
        ProductOperation::SecurityApiKeyIssueSelfStart {
            principal_id: fields.0,
            label: fields.1,
            roles: fields.2,
            custom_roles: fields.3,
            permission_ceiling: fields.4,
            scope_ceiling: fields.5,
            expires_at_micros: fields.6,
        }
    } else {
        ProductOperation::SecurityApiKeyIssueStart {
            principal_id: fields.0,
            label: fields.1,
            roles: fields.2,
            custom_roles: fields.3,
            permission_ceiling: fields.4,
            scope_ceiling: fields.5,
            expires_at_micros: fields.6,
        }
    }
}

fn key_issue_activate(
    key_id: ApiKeyId,
    confirmation_digest: ApiKeyConfirmationDigest,
    self_manage: bool,
) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyIssueSelfActivate {
            key_id,
            confirmation_digest,
        }
    } else {
        ProductOperation::SecurityApiKeyIssueActivate {
            key_id,
            confirmation_digest,
        }
    }
}

fn key_issue_abort(key_id: ApiKeyId, self_manage: bool) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyIssueSelfAbort { key_id }
    } else {
        ProductOperation::SecurityApiKeyIssueAbort { key_id }
    }
}

fn key_rotate_start(
    predecessor_key_id: ApiKeyId,
    label: &str,
    self_manage: bool,
) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyRotateSelfStart {
            predecessor_key_id,
            label: label.to_owned(),
            overlap_seconds: 60,
            expires_at_micros: None,
        }
    } else {
        ProductOperation::SecurityApiKeyRotateStart {
            predecessor_key_id,
            label: label.to_owned(),
            overlap_seconds: 60,
            expires_at_micros: None,
        }
    }
}

fn key_rotate_activate(
    successor_key_id: ApiKeyId,
    confirmation_digest: ApiKeyConfirmationDigest,
    self_manage: bool,
) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id,
            confirmation_digest,
        }
    } else {
        ProductOperation::SecurityApiKeyRotateActivate {
            successor_key_id,
            confirmation_digest,
        }
    }
}

fn key_rotate_abort(successor_key_id: ApiKeyId, self_manage: bool) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyRotateSelfAbort { successor_key_id }
    } else {
        ProductOperation::SecurityApiKeyRotateAbort { successor_key_id }
    }
}

fn key_revoke(key_id: ApiKeyId, self_manage: bool) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyRevokeSelf { key_id }
    } else {
        ProductOperation::SecurityApiKeyRevoke { key_id }
    }
}

fn authenticated_session(
    product: &NativeProduct,
    secret: &str,
    id: u128,
) -> Result<ProductSession, Box<dyn Error>> {
    Ok(ProductSession::new_authenticated(
        ProductSessionId::new(id).ok_or("zero session ID")?,
        product.authenticate_api_key(secret, 0)?,
    ))
}

#[allow(clippy::result_large_err)]
fn dispatch(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    request_id: u128,
    token: u128,
    operation: ProductOperation,
) -> Result<ProductResponse, ProductError> {
    let context = ProductRequestContext::new(
        request_id,
        session.id(),
        100,
        session.principal().clone(),
        session.authorization(),
    )
    .with_authorization_epoch(session.authorization_epoch())
    .with_idempotency_token(token);
    product.dispatch(session, &context, operation)
}

fn assert_error_code<T>(
    result: Result<T, ProductError>,
    expected: ProductErrorCode,
) -> Result<(), Box<dyn Error>> {
    let error = result.err().ok_or("operation unexpectedly succeeded")?;
    if error.code() != expected {
        return Err(format!("unexpected error: {error:?}, expected {expected:?}").into());
    }
    Ok(())
}

fn is_start(case: ManagedCase) -> bool {
    matches!(
        case,
        ManagedCase::KeyIssueSelfStart
            | ManagedCase::KeyIssueStart
            | ManagedCase::KeyRotateSelfStart
            | ManagedCase::KeyRotateStart
    )
}

fn expects_complete(boundary: CommitBoundary) -> bool {
    matches!(
        boundary,
        CommitBoundary::WalAppended
            | CommitBoundary::WalSynchronized
            | CommitBoundary::RootPublished
    )
}

fn assert_no_serialized_secret(directory: &Path, known: &[String]) -> Result<(), Box<dyn Error>> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            assert!(!bytes.windows(5).any(|window| window == b"hyp1_"));
            for secret in known {
                assert!(
                    !bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes())
                );
            }
            assert!(
                !bytes
                    .windows(LEGACY_BEARER.len())
                    .any(|window| window == LEGACY_BEARER)
            );
        }
    }
    Ok(())
}
