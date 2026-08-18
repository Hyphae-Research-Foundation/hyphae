// SPDX-License-Identifier: Apache-2.0

//! Process hard-kill matrix for every durable security mutation.

#[path = "security_crash_support.rs"]
mod security_crash_support;

use std::{
    error::Error,
    fs,
    io::{self, BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hyphae_native_product::{
    AccessControlStatus, ApiKeyConfirmationDigest, ApiKeyId, AuthorizationEpoch, BuiltInRole,
    CustomRoleGrant, LegacyBearerState, NativeProduct, ProductErrorCode, ProductOperation,
    ProductPermission, ProductRequestContext, ProductScope, ProductSession, ProductSessionId,
    SecurityKeyListRequest,
};
use hyphae_native_runtime::CommitBoundary;
use security_crash_support::{ManagedCase, OfflineCase, managed_variant};
use serde_json::json;

const CHILD_MODE: &str = "--child";
const READY_PREFIX: &str = "hyphae-security-crash-ready:";
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_EXIT_GRACE: Duration = Duration::from_millis(100);
const DATA_DIRECTORY: &str = "data";
const OWNER_KEY_FILE: &str = "owner.key";
const ACTOR_KEY_FILE: &str = "actor.key";
const META_FILE: &str = "case.meta";
const REQUEST_FILE: &str = "operation.bin";
const UNWIND_SENTINEL: &str = "child-unwound";
const TOKEN: u128 = 0x7356_0001;
const SETUP_TOKEN: u128 = 0x7356_1000;
const LEGACY_BEARER: &[u8] = b"security-crash-legacy-bearer-0123456789abcdef";

const BOUNDARIES: [(&str, CommitBoundary); 7] = [
    ("BlobStaged", CommitBoundary::BlobStaged),
    ("BlobPromoted", CommitBoundary::BlobPromoted),
    ("PageAppended", CommitBoundary::PageAppended),
    ("PageSynchronized", CommitBoundary::PageSynchronized),
    ("WalAppended", CommitBoundary::WalAppended),
    ("WalSynchronized", CommitBoundary::WalSynchronized),
    ("RootPublished", CommitBoundary::RootPublished),
];

const CASES: [CrashCase; 26] = [
    CrashCase::offline(
        "offline.legacy_bearer_activate",
        "legacy-bearer-activate",
        OfflineCase::LegacyBearerActivate,
    ),
    CrashCase::offline(
        "offline.legacy_bearer_migrate",
        "legacy-bearer-migrate",
        OfflineCase::LegacyBearerMigrate,
    ),
    CrashCase::offline(
        "offline.owner_recovery_abort",
        "owner-recovery-abort",
        OfflineCase::OwnerRecoveryAbort,
    ),
    CrashCase::offline(
        "offline.owner_recovery_activate",
        "owner-recovery-activate",
        OfflineCase::OwnerRecoveryActivate,
    ),
    CrashCase::offline(
        "offline.owner_recovery_begin",
        "owner-recovery-begin",
        OfflineCase::OwnerRecoveryStart,
    ),
    CrashCase::managed(
        "security.assignment_create_built_in",
        "assignment-create-built-in",
        ManagedCase::BuiltInAssignmentCreate,
    ),
    CrashCase::managed(
        "security.assignment_create_custom",
        "assignment-create-custom",
        ManagedCase::CustomAssignmentCreate,
    ),
    CrashCase::managed(
        "security.assignment_revoke",
        "assignment-revoke",
        ManagedCase::AssignmentRevoke,
    ),
    CrashCase::managed(
        "security.custom_role_create",
        "custom-role-create",
        ManagedCase::CustomRoleCreate,
    ),
    CrashCase::managed(
        "security.key_issue_abort",
        "key-issue-abort",
        ManagedCase::KeyIssueAbort,
    ),
    CrashCase::managed(
        "security.key_issue_activate",
        "key-issue-activate",
        ManagedCase::KeyIssueActivate,
    ),
    CrashCase::managed(
        "security.key_issue_self_abort",
        "key-issue-abort",
        ManagedCase::KeyIssueSelfAbort,
    ),
    CrashCase::managed(
        "security.key_issue_self_activate",
        "key-issue-activate",
        ManagedCase::KeyIssueSelfActivate,
    ),
    CrashCase::managed(
        "security.key_issue_self_start",
        "key-issue-start",
        ManagedCase::KeyIssueSelfStart,
    ),
    CrashCase::managed(
        "security.key_issue_start",
        "key-issue-start",
        ManagedCase::KeyIssueStart,
    ),
    CrashCase::managed("security.key_revoke", "key-revoke", ManagedCase::KeyRevoke),
    CrashCase::managed(
        "security.key_revoke_self",
        "key-revoke",
        ManagedCase::KeyRevokeSelf,
    ),
    CrashCase::managed(
        "security.key_rotate_abort",
        "key-rotate-abort",
        ManagedCase::KeyRotateAbort,
    ),
    CrashCase::managed(
        "security.key_rotate_activate",
        "key-rotate-activate",
        ManagedCase::KeyRotateActivate,
    ),
    CrashCase::managed(
        "security.key_rotate_self_abort",
        "key-rotate-abort",
        ManagedCase::KeyRotateSelfAbort,
    ),
    CrashCase::managed(
        "security.key_rotate_self_activate",
        "key-rotate-activate",
        ManagedCase::KeyRotateSelfActivate,
    ),
    CrashCase::managed(
        "security.key_rotate_self_start",
        "key-rotate-start",
        ManagedCase::KeyRotateSelfStart,
    ),
    CrashCase::managed(
        "security.key_rotate_start",
        "key-rotate-start",
        ManagedCase::KeyRotateStart,
    ),
    CrashCase::managed(
        "security.legacy_bearer_revoke",
        "legacy-bearer-revoke",
        ManagedCase::LegacyBearerRevoke,
    ),
    CrashCase::managed(
        "security.principal_create",
        "principal-create",
        ManagedCase::PrincipalCreate,
    ),
    CrashCase::managed(
        "security.principal_set_enabled",
        "principal-set-enabled",
        ManagedCase::PrincipalSetEnabled,
    ),
];

#[derive(Clone, Copy)]
struct CrashCase {
    id: &'static str,
    family: &'static str,
    kind: CrashKind,
}

impl CrashCase {
    const fn managed(id: &'static str, family: &'static str, case: ManagedCase) -> Self {
        Self {
            id,
            family,
            kind: CrashKind::Managed(case),
        }
    }

    const fn offline(id: &'static str, family: &'static str, case: OfflineCase) -> Self {
        Self {
            id,
            family,
            kind: CrashKind::Offline(case),
        }
    }

    fn operation(self) -> Option<&'static str> {
        match self.kind {
            CrashKind::Managed(case) => Some(managed_variant(case)),
            CrashKind::Offline(_) => None,
        }
    }
}

#[derive(Clone, Copy)]
enum CrashKind {
    Managed(ManagedCase),
    Offline(OfflineCase),
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-security-process-crash-{}-{timestamp}",
            std::process::id()
        ))))
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

struct UnwindSentinel(PathBuf);

impl Drop for UnwindSentinel {
    fn drop(&mut self) {
        let _ignored = fs::write(&self.0, b"unwound");
    }
}

struct HarnessMeta {
    secret: String,
    key_id: Option<ApiKeyId>,
    expected_epoch: Option<AuthorizationEpoch>,
    baseline: AccessControlStatus,
}

fn boundary_hook(boundary: CommitBoundary) {
    println!("{READY_PREFIX}{}", boundary_name(boundary));
    let _ignored = io::stdout().flush();
    loop {
        thread::park();
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args();
    let _program = arguments.next();
    let first = arguments
        .next()
        .ok_or_else(|| failure("missing mode or source commit"))?;
    if first == CHILD_MODE {
        let directory = PathBuf::from(
            arguments
                .next()
                .ok_or_else(|| failure("missing data directory"))?,
        );
        let case_id = arguments.next().ok_or_else(|| failure("missing case ID"))?;
        let boundary = arguments
            .next()
            .ok_or_else(|| failure("missing boundary"))?;
        require_no_remaining(arguments)?;
        return run_child(&directory, &case_id, &boundary);
    }

    let source_commit = first;
    let environment = arguments
        .next()
        .ok_or_else(|| failure("missing environment label"))?;
    let shard_index = parse_optional_usize(arguments.next(), 0, "shard index")?;
    let shard_count = parse_optional_usize(arguments.next(), 1, "shard count")?;
    require_no_remaining(arguments)?;
    validate_label("source commit", &source_commit)?;
    validate_label("environment", &environment)?;
    if source_commit.len() != 40
        || !source_commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(failure("source commit must be 40 hexadecimal digits"));
    }
    if shard_count == 0 || shard_index >= shard_count {
        return Err(failure("invalid shard selection"));
    }
    run_parent(&source_commit, &environment, shard_index, shard_count)
}

fn run_child(directory: &Path, case_id: &str, boundary_name: &str) -> Result<(), Box<dyn Error>> {
    let case = case_by_id(case_id).ok_or_else(|| failure("unknown case ID"))?;
    let boundary = parse_boundary(boundary_name).ok_or_else(|| failure("unknown boundary"))?;
    let _unwind = UnwindSentinel(directory.join(UNWIND_SENTINEL));
    execute_case(directory, case, boundary)?;
    Err(failure("commit boundary hook returned or was not reached"))
}

fn run_parent(
    source_commit: &str,
    environment: &str,
    shard_index: usize,
    shard_count: usize,
) -> Result<(), Box<dyn Error>> {
    let executable = std::env::current_exe()?;
    let temporary = TemporaryDirectory::create()?;
    fs::create_dir_all(&temporary.0)?;
    let selected: Vec<_> = CASES
        .into_iter()
        .enumerate()
        .filter_map(|(index, case)| (index % shard_count == shard_index).then_some(case))
        .collect();
    let mut observations = Vec::with_capacity(selected.len() * BOUNDARIES.len());
    for case in selected {
        for (boundary_name, boundary) in BOUNDARIES {
            let directory = temporary
                .0
                .join(format!("{}-{boundary_name}", case.id.replace('.', "-")));
            prepare_case(&directory, case)?;
            let termination =
                kill_child_at_boundary(&executable, &directory, case.id, boundary_name)?;
            verify_case(&directory, case, boundary)?;
            observations.push(json!({
                "case_id": case.id,
                "semantic_family": case.family,
                "kind": match case.kind { CrashKind::Managed(_) => "product-operation", CrashKind::Offline(_) => "offline" },
                "product_operation": case.operation(),
                "boundary": boundary_name,
                "expected_state": if expects_complete(boundary) { "complete" } else { "prior" },
                "recovered_state": if expects_complete(boundary) { "complete" } else { "prior" },
                "boundary_hook_reached": true,
                "child_unwound": false,
                "termination": termination,
                "recovery_verified": true,
            }));
        }
    }
    let receipt = json!({
        "schema": "hyphae-security-process-crash-matrix-v2",
        "status": "passed",
        "source_commit": source_commit,
        "environment": environment,
        "target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        "semantics": "process-crash-not-power-loss",
        "shard_index": shard_index,
        "shard_count": shard_count,
        "case_count": selected_case_count(shard_index, shard_count),
        "boundary_case_count": observations.len(),
        "observations": observations,
    });
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

fn execute_case(
    directory: &Path,
    case: CrashCase,
    boundary: CommitBoundary,
) -> Result<(), Box<dyn Error>> {
    match case.kind {
        CrashKind::Managed(managed) => execute_managed(directory, managed, boundary),
        CrashKind::Offline(offline) => execute_offline(directory, offline, boundary),
    }
}

fn prepare_case(directory: &Path, case: CrashCase) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = directory.parent() {
        fs::create_dir_all(parent)?;
    }
    match case.kind {
        CrashKind::Managed(managed) => prepare_managed(directory, managed),
        CrashKind::Offline(offline) => prepare_offline(directory, offline),
    }
}

#[allow(clippy::too_many_lines)]
fn prepare_managed(directory: &Path, case: ManagedCase) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(directory)?;
    let data = directory.join(DATA_DIRECTORY);
    if matches!(case, ManagedCase::LegacyBearerRevoke) {
        drop(NativeProduct::create(&data)?);
        let mut product = NativeProduct::open_offline_owner(&data)?;
        let started = product.start_legacy_bearer_migration_offline(
            "Legacy owner",
            "canonical-owner",
            LEGACY_BEARER,
            1,
        )?;
        let secret = started.secret.expose_secret().to_owned();
        product.activate_legacy_bearer_migration_offline(
            started.key_id,
            &secret,
            started.authorization_epoch,
            "Legacy owner",
            "canonical-owner",
            LEGACY_BEARER,
            2,
        )?;
        fs::write(directory.join(OWNER_KEY_FILE), &secret)?;
        let request = hyphae_native_protocol::WireRequest {
            operation: ProductOperation::SecurityLegacyBearerRevoke,
            logical_time_micros: 100,
            deadline_micros: None,
            idempotency_token: Some(TOKEN),
            limits: hyphae_native_product::ProductLimits::default(),
            durability: hyphae_native_product::ProductDurabilityPolicy::STRICT,
        };
        fs::write(
            directory.join(REQUEST_FILE),
            hyphae_native_protocol::encode_product_request(&request)?,
        )?;
        write_meta(
            directory,
            &secret,
            None,
            None,
            product.access_control_status()?,
        )?;
        return Ok(());
    }
    let mut product = NativeProduct::create(&data)?;
    product.bootstrap_access_control_to_file(
        "Owner",
        "owner",
        directory.join(OWNER_KEY_FILE),
        1,
    )?;
    let owner_secret = fs::read_to_string(directory.join(OWNER_KEY_FILE))?;
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
        directory.join(ACTOR_KEY_FILE),
        5,
    )?;
    let actor_secret = fs::read_to_string(directory.join(ACTOR_KEY_FILE))?;
    let (operation, meta) = prepare_product_operation(
        case,
        &mut product,
        &owner_secret,
        &actor_secret,
        admin.principal_id,
    )?;
    let key_id = operation_key_id(&operation);
    let encoded =
        hyphae_native_protocol::encode_product_request(&hyphae_native_protocol::WireRequest {
            operation,
            logical_time_micros: 100,
            deadline_micros: None,
            idempotency_token: Some(TOKEN),
            limits: hyphae_native_product::ProductLimits::default(),
            durability: hyphae_native_product::ProductDurabilityPolicy::STRICT,
        })?;
    fs::write(directory.join(REQUEST_FILE), encoded)?;
    write_meta(
        directory,
        &meta,
        key_id,
        None,
        product.access_control_status()?,
    )?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn prepare_product_operation(
    case: ManagedCase,
    product: &mut NativeProduct,
    owner_secret: &str,
    actor_secret: &str,
    admin_principal: hyphae_native_product::SecurityId,
) -> Result<(ProductOperation, String), Box<dyn Error>> {
    let grant = CustomRoleGrant::new(ProductPermission::DataRead, ProductScope::Instance)
        .ok_or("invalid grant")?;
    let mut setup_token = SETUP_TOKEN;
    let mut principal = |label: &str, product: &mut NativeProduct| -> Result<_, Box<dyn Error>> {
        setup_token += 1;
        let owner = product.authenticate_api_key(owner_secret, 0)?;
        Ok(product
            .create_security_principal_idempotent(&owner, label, setup_token, 10)?
            .principal_id)
    };
    let mut meta = String::new();
    let operation = match case {
        ManagedCase::PrincipalCreate => ProductOperation::SecurityPrincipalCreate {
            display_name: "Crash principal".to_owned(),
        },
        ManagedCase::PrincipalSetEnabled => ProductOperation::SecurityPrincipalSetEnabled {
            principal_id: principal("Enable target", product)?,
            enabled: true,
        },
        ManagedCase::CustomRoleCreate => ProductOperation::SecurityCustomRoleCreate {
            display_name: "Crash role".to_owned(),
            grants: vec![grant],
        },
        ManagedCase::BuiltInAssignmentCreate => ProductOperation::SecurityBuiltInAssignmentCreate {
            principal_id: principal("Built-in target", product)?,
            role: BuiltInRole::Reader,
            scope: ProductScope::Instance,
        },
        ManagedCase::CustomAssignmentCreate => {
            let target = principal("Custom target", product)?;
            let owner = product.authenticate_api_key(owner_secret, 0)?;
            let role = product.create_custom_security_role_idempotent(
                &owner,
                "Crash role",
                [grant],
                SETUP_TOKEN + 20,
                11,
            )?;
            ProductOperation::SecurityCustomAssignmentCreate {
                principal_id: target,
                role_id: role.role_id,
            }
        }
        ManagedCase::AssignmentRevoke => {
            let target = principal("Assignment target", product)?;
            let owner = product.authenticate_api_key(owner_secret, 0)?;
            let assignment = product.assign_built_in_role_idempotent(
                &owner,
                target,
                BuiltInRole::Reader,
                ProductScope::Instance,
                SETUP_TOKEN + 21,
                11,
            )?;
            ProductOperation::SecurityAssignmentRevoke {
                assignment_id: assignment.assignment_id,
            }
        }
        ManagedCase::KeyIssueSelfStart | ManagedCase::KeyIssueStart => key_issue_start(
            admin_principal,
            matches!(case, ManagedCase::KeyIssueSelfStart),
        ),
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
            let secret = started.secret.take().ok_or("missing setup secret")?;
            secret.expose_secret().clone_into(&mut meta);
            if matches!(
                case,
                ManagedCase::KeyIssueSelfActivate | ManagedCase::KeyIssueActivate
            ) {
                key_issue_activate(started.key_id, secret.confirmation_digest(), self_manage)
            } else {
                key_issue_abort(started.key_id, self_manage)
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
            if matches!(
                case,
                ManagedCase::KeyRotateSelfStart | ManagedCase::KeyRotateStart
            ) {
                key_rotate_start(predecessor, self_manage)
            } else {
                let actor = product.authenticate_api_key(
                    if self_manage {
                        actor_secret
                    } else {
                        owner_secret
                    },
                    if self_manage { 0 } else { 60 },
                )?;
                let started = product.start_api_key_rotation_idempotent(
                    &actor,
                    predecessor,
                    "Pending rotation",
                    0,
                    None,
                    SETUP_TOKEN + 40,
                    13,
                    self_manage,
                )?;
                let secret = started.secret.take().ok_or("missing rotation secret")?;
                secret.expose_secret().clone_into(&mut meta);
                if matches!(
                    case,
                    ManagedCase::KeyRotateSelfActivate | ManagedCase::KeyRotateActivate
                ) {
                    key_rotate_activate(started.key_id, secret.confirmation_digest(), self_manage)
                } else {
                    key_rotate_abort(started.key_id, self_manage)
                }
            }
        }
        ManagedCase::KeyRevokeSelf | ManagedCase::KeyRevoke => {
            let self_manage = matches!(case, ManagedCase::KeyRevokeSelf);
            let key_id = product.authenticate_api_key(actor_secret, 0)?.key_id();
            key_revoke(key_id, self_manage)
        }
        ManagedCase::LegacyBearerRevoke => ProductOperation::SecurityLegacyBearerRevoke,
    };
    Ok((operation, meta))
}

fn execute_managed(
    directory: &Path,
    case: ManagedCase,
    boundary: CommitBoundary,
) -> Result<(), Box<dyn Error>> {
    let mut product = NativeProduct::open(directory.join(DATA_DIRECTORY))?;
    let secret = fs::read_to_string(directory.join(
        if matches!(case, ManagedCase::LegacyBearerRevoke) {
            OWNER_KEY_FILE
        } else {
            ACTOR_KEY_FILE
        },
    ))?;
    let actor_secret = if matches!(
        case,
        ManagedCase::KeyIssueStart
            | ManagedCase::KeyIssueActivate
            | ManagedCase::KeyIssueAbort
            | ManagedCase::KeyRotateStart
            | ManagedCase::KeyRotateActivate
            | ManagedCase::KeyRotateAbort
            | ManagedCase::KeyRevoke
            | ManagedCase::LegacyBearerRevoke
    ) {
        fs::read_to_string(directory.join(OWNER_KEY_FILE))?
    } else {
        secret
    };
    let request =
        hyphae_native_protocol::decode_product_request(&fs::read(directory.join(REQUEST_FILE))?)?;
    let mut session = ProductSession::new_authenticated(
        ProductSessionId::new(1).ok_or("invalid session")?,
        product.authenticate_api_key(&actor_secret, 0)?,
    );
    let context = ProductRequestContext::new(
        1,
        session.id(),
        100,
        session.principal().clone(),
        session.authorization(),
    )
    .with_authorization_epoch(session.authorization_epoch())
    .with_idempotency_token(TOKEN);
    product.hook_next_security_commit_for_test(boundary, boundary_hook);
    let _response = product.dispatch(&mut session, &context, request.operation)?;
    Err(failure("managed mutation returned past boundary hook"))
}

fn prepare_offline(directory: &Path, case: OfflineCase) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(directory)?;
    let data = directory.join(DATA_DIRECTORY);
    match case {
        OfflineCase::LegacyBearerMigrate | OfflineCase::LegacyBearerActivate => {
            drop(NativeProduct::create(&data)?);
        }
        _ => {
            let mut product = NativeProduct::create(&data)?;
            product.bootstrap_access_control_to_file(
                "Owner",
                "owner",
                directory.join(OWNER_KEY_FILE),
                1,
            )?;
        }
    }
    if matches!(
        case,
        OfflineCase::OwnerRecoveryActivate | OfflineCase::OwnerRecoveryAbort
    ) {
        let mut product = NativeProduct::open_offline_owner(&data)?;
        let started = product.start_owner_recovery_offline("replacement", 2)?;
        write_meta(
            directory,
            started.secret.expose_secret(),
            Some(started.key_id),
            Some(started.authorization_epoch),
            product.access_control_status()?,
        )?;
    }
    if matches!(case, OfflineCase::LegacyBearerActivate) {
        let mut product = NativeProduct::open_offline_owner(&data)?;
        let started = product.start_legacy_bearer_migration_offline(
            "Legacy owner",
            "canonical",
            LEGACY_BEARER,
            1,
        )?;
        write_meta(
            directory,
            started.secret.expose_secret(),
            Some(started.key_id),
            Some(started.authorization_epoch),
            product.access_control_status()?,
        )?;
    }
    if !directory.join(META_FILE).exists() {
        let product = NativeProduct::open_offline_owner(&data)?;
        write_meta(directory, "", None, None, product.access_control_status()?)?;
    }
    Ok(())
}

fn execute_offline(
    directory: &Path,
    case: OfflineCase,
    boundary: CommitBoundary,
) -> Result<(), Box<dyn Error>> {
    let mut product = NativeProduct::open_offline_owner(directory.join(DATA_DIRECTORY))?;
    product.hook_next_security_commit_for_test(boundary, boundary_hook);
    match case {
        OfflineCase::OwnerRecoveryStart => {
            let _ = product.start_owner_recovery_offline("replacement", 2)?;
        }
        OfflineCase::OwnerRecoveryActivate => {
            let (key, epoch, secret) = read_offline_meta(directory)?;
            let _ = product.resume_owner_recovery_offline(key, &secret, epoch, 3)?;
        }
        OfflineCase::OwnerRecoveryAbort => {
            let (key, epoch, _) = read_offline_meta(directory)?;
            let _ = product.abort_owner_recovery_offline(key, epoch, 3)?;
        }
        OfflineCase::LegacyBearerMigrate => {
            let _ = product.start_legacy_bearer_migration_offline(
                "Legacy owner",
                "canonical",
                LEGACY_BEARER,
                1,
            )?;
        }
        OfflineCase::LegacyBearerActivate => {
            let (key, epoch, secret) = read_offline_meta(directory)?;
            let _ = product.activate_legacy_bearer_migration_offline(
                key,
                &secret,
                epoch,
                "Legacy owner",
                "canonical",
                LEGACY_BEARER,
                2,
            )?;
        }
    }
    Err(failure("offline mutation returned past boundary hook"))
}

fn read_offline_meta(
    directory: &Path,
) -> Result<
    (
        hyphae_native_product::ApiKeyId,
        hyphae_native_product::AuthorizationEpoch,
        String,
    ),
    Box<dyn Error>,
> {
    let meta = read_meta(directory)?;
    Ok((
        meta.key_id.ok_or("missing key")?,
        meta.expected_epoch.ok_or("missing epoch")?,
        meta.secret,
    ))
}

fn write_meta(
    directory: &Path,
    secret: &str,
    key_id: Option<ApiKeyId>,
    expected_epoch: Option<AuthorizationEpoch>,
    baseline: AccessControlStatus,
) -> Result<(), Box<dyn Error>> {
    fs::write(
        directory.join(META_FILE),
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            key_id.map_or_else(|| "-".to_owned(), |value| value.to_string()),
            expected_epoch.map_or_else(|| "-".to_owned(), |value| value.get().to_string()),
            baseline.epoch.get(),
            baseline.principals,
            baseline.assignments,
            baseline.custom_roles,
            baseline.custom_assignments,
            baseline.keys,
            baseline.pending_keys,
            secret,
        ),
    )?;
    Ok(())
}

fn read_meta(directory: &Path) -> Result<HarnessMeta, Box<dyn Error>> {
    let value = fs::read_to_string(directory.join(META_FILE))?;
    let mut lines = value.lines();
    let key = lines.next().ok_or("missing key metadata")?;
    let epoch = lines.next().ok_or("missing expected epoch metadata")?;
    let baseline_epoch = lines.next().ok_or("missing baseline epoch")?.parse()?;
    let baseline = AccessControlStatus {
        bootstrapped: true,
        epoch: AuthorizationEpoch::new(baseline_epoch),
        principals: lines.next().ok_or("missing principal count")?.parse()?,
        assignments: lines.next().ok_or("missing assignment count")?.parse()?,
        custom_roles: lines.next().ok_or("missing role count")?.parse()?,
        custom_assignments: lines
            .next()
            .ok_or("missing custom assignment count")?
            .parse()?,
        keys: lines.next().ok_or("missing key count")?.parse()?,
        pending_keys: lines.next().ok_or("missing pending count")?.parse()?,
        audit_events: 0,
    };
    Ok(HarnessMeta {
        secret: lines.next().unwrap_or_default().to_owned(),
        key_id: (key != "-").then(|| key.parse()).transpose()?,
        expected_epoch: (epoch != "-")
            .then(|| epoch.parse().map(AuthorizationEpoch::new))
            .transpose()?,
        baseline,
    })
}

fn operation_key_id(operation: &ProductOperation) -> Option<ApiKeyId> {
    match operation {
        ProductOperation::SecurityApiKeyIssueSelfActivate { key_id, .. }
        | ProductOperation::SecurityApiKeyIssueActivate { key_id, .. }
        | ProductOperation::SecurityApiKeyIssueSelfAbort { key_id }
        | ProductOperation::SecurityApiKeyIssueAbort { key_id }
        | ProductOperation::SecurityApiKeyRevokeSelf { key_id }
        | ProductOperation::SecurityApiKeyRevoke { key_id } => Some(*key_id),
        ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id, ..
        }
        | ProductOperation::SecurityApiKeyRotateActivate {
            successor_key_id, ..
        }
        | ProductOperation::SecurityApiKeyRotateSelfAbort { successor_key_id }
        | ProductOperation::SecurityApiKeyRotateAbort { successor_key_id } => {
            Some(*successor_key_id)
        }
        _ => None,
    }
}

fn verify_managed(
    directory: &Path,
    case: ManagedCase,
    complete: bool,
    meta: &HarnessMeta,
) -> Result<(), Box<dyn Error>> {
    let data = directory.join(DATA_DIRECTORY);
    let mut product = NativeProduct::open(&data)?;
    let owner_secret = fs::read_to_string(directory.join(OWNER_KEY_FILE))?;
    let owner = product.authenticate_api_key(&owner_secret, 0)?;
    let status = product.access_control_status()?;
    let expected_epoch = if complete {
        meta.baseline.epoch.checked_next().ok_or("epoch overflow")?
    } else {
        meta.baseline.epoch
    };
    if status.epoch != expected_epoch {
        return Err(failure("managed recovered epoch differs"));
    }
    if is_start(case) {
        if status.keys != meta.baseline.keys + usize::from(complete)
            || status.pending_keys != meta.baseline.pending_keys + usize::from(complete)
        {
            return Err(failure("start recovery is not exactly one inactive key"));
        }
        if complete {
            let keys = product.read_security_keys_for_test(
                &owner,
                SecurityKeyListRequest::new(None, 1_000)?,
                0,
            )?;
            let pending = keys
                .items()
                .iter()
                .filter(|key| !key.active() && !key.revoked())
                .count();
            if pending != status.pending_keys {
                return Err(failure("recovered start key became active"));
            }
        }
    }
    if let Some(key_id) = meta.key_id {
        let keys = product.read_security_keys_for_test(
            &owner,
            SecurityKeyListRequest::new(None, 1_000)?,
            0,
        )?;
        let key = keys.items().iter().find(|key| key.id() == key_id);
        match case {
            ManagedCase::KeyIssueSelfActivate
            | ManagedCase::KeyIssueActivate
            | ManagedCase::KeyRotateSelfActivate
            | ManagedCase::KeyRotateActivate
                if complete && !key.is_some_and(|key| key.active() && !key.revoked()) =>
            {
                return Err(failure("activation recovery is not terminal active state"));
            }
            ManagedCase::KeyIssueSelfAbort
            | ManagedCase::KeyIssueAbort
            | ManagedCase::KeyRotateSelfAbort
            | ManagedCase::KeyRotateAbort
                if complete && key.is_some() =>
            {
                return Err(failure("abort recovery retained the pending key"));
            }
            ManagedCase::KeyRevokeSelf | ManagedCase::KeyRevoke
                if complete
                    && !key.is_some_and(hyphae_native_product::SecurityKeySummary::revoked) =>
            {
                return Err(failure("revoke recovery is not terminal revoked state"));
            }
            _ => {}
        }
    }
    if matches!(case, ManagedCase::LegacyBearerRevoke) {
        let expected = if complete {
            LegacyBearerState::Revoked
        } else {
            LegacyBearerState::DualWindow
        };
        if product.legacy_bearer_migration_inspection()?.state != expected {
            return Err(failure("legacy revoke recovered state differs"));
        }
    }
    verify_managed_retry(&mut product, directory, case, complete)?;
    drop(owner);
    drop(product);
    assert_secret_free(&data, &[&meta.secret, &owner_secret])
}

fn verify_managed_retry(
    product: &mut NativeProduct,
    directory: &Path,
    case: ManagedCase,
    complete: bool,
) -> Result<(), Box<dyn Error>> {
    let actor_secret = if matches!(
        case,
        ManagedCase::KeyIssueStart
            | ManagedCase::KeyIssueActivate
            | ManagedCase::KeyIssueAbort
            | ManagedCase::KeyRotateStart
            | ManagedCase::KeyRotateActivate
            | ManagedCase::KeyRotateAbort
            | ManagedCase::KeyRevoke
            | ManagedCase::LegacyBearerRevoke
    ) {
        fs::read_to_string(directory.join(OWNER_KEY_FILE))?
    } else {
        fs::read_to_string(directory.join(ACTOR_KEY_FILE))?
    };
    let request =
        hyphae_native_protocol::decode_product_request(&fs::read(directory.join(REQUEST_FILE))?)?;
    let terminal = complete && matches!(case, ManagedCase::KeyRevokeSelf)
        || matches!(case, ManagedCase::KeyRotateSelfActivate);
    if terminal {
        let authority = match product.authenticate_api_key(&actor_secret, 0) {
            Ok(authority) => authority,
            Err(error) if error.code() == ProductErrorCode::AuthorizationDenied => {
                product.authenticate_api_key_for_terminal_replay(&actor_secret)?
            }
            Err(error) => return Err(error.into()),
        };
        let mut session = ProductSession::new_authenticated(
            ProductSessionId::new(2).ok_or("invalid terminal replay session")?,
            authority,
        );
        let context = ProductRequestContext::new(
            2,
            session.id(),
            101,
            session.principal().clone(),
            session.authorization(),
        )
        .with_authorization_epoch(session.authorization_epoch())
        .with_idempotency_token(TOKEN);
        let _receipt = product.dispatch(&mut session, &context, request.operation)?;
        return Ok(());
    }
    let authority = match product.authenticate_api_key(&actor_secret, 0) {
        Ok(authority) => authority,
        Err(error) if error.code() == ProductErrorCode::AuthorizationDenied => {
            product.authenticate_api_key_for_terminal_replay(&actor_secret)?
        }
        Err(error) => return Err(error.into()),
    };
    let mut session = ProductSession::new_authenticated(
        ProductSessionId::new(2).ok_or("invalid session")?,
        authority,
    );
    let context = ProductRequestContext::new(
        2,
        session.id(),
        101,
        session.principal().clone(),
        session.authorization(),
    )
    .with_authorization_epoch(session.authorization_epoch())
    .with_idempotency_token(TOKEN);
    let first = product.dispatch(&mut session, &context, request.operation.clone());
    if complete && is_start(case) {
        if first.err().map(|error| error.code()) != Some(ProductErrorCode::SecretDeliveryConsumed) {
            return Err(failure("committed start redelivered its secret"));
        }
        return Ok(());
    }
    let first = first?;
    let second = product.dispatch(&mut session, &context, request.operation);
    if is_start(case) {
        if second.err().map(|error| error.code()) != Some(ProductErrorCode::SecretDeliveryConsumed)
        {
            return Err(failure("start retry redelivered its secret"));
        }
    } else if second? != first {
        return Err(failure("terminal replay receipt changed"));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn verify_offline(
    directory: &Path,
    case: OfflineCase,
    complete: bool,
    meta: &HarnessMeta,
) -> Result<(), Box<dyn Error>> {
    let data = directory.join(DATA_DIRECTORY);
    let mut product = NativeProduct::open_offline_owner(&data)?;
    let status = product.access_control_status()?;
    let expected_epoch = if complete {
        meta.baseline.epoch.checked_next().ok_or("epoch overflow")?
    } else {
        meta.baseline.epoch
    };
    if status.epoch != expected_epoch {
        return Err(failure("offline recovered epoch differs"));
    }
    match case {
        OfflineCase::OwnerRecoveryStart => {
            let pending = product.inspect_owner_recovery_offline()?.pending;
            if pending.is_some() != complete {
                return Err(failure("owner start pending state differs"));
            }
            let old_secret = fs::read_to_string(directory.join(OWNER_KEY_FILE))?;
            let owner = product.authenticate_api_key(&old_secret, 0)?;
            if let Some(pending) = pending {
                let keys = product.read_security_keys_for_test(
                    &owner,
                    SecurityKeyListRequest::new(None, 1_000)?,
                    0,
                )?;
                if !keys
                    .items()
                    .iter()
                    .any(|key| key.id() == pending.key_id() && !key.active() && !key.revoked())
                {
                    return Err(failure("owner start recovered active replacement"));
                }
            }
        }
        OfflineCase::OwnerRecoveryActivate => {
            let old_secret = fs::read_to_string(directory.join(OWNER_KEY_FILE))?;
            if product.authenticate_api_key(&meta.secret, 0).is_ok() != complete
                || product.authenticate_api_key(&old_secret, 0).is_err() != complete
                || product.inspect_owner_recovery_offline()?.pending.is_none() != complete
            {
                return Err(failure("owner activation recovered state differs"));
            }
            let key = meta.key_id.ok_or("missing activation key")?;
            let epoch = meta.expected_epoch.ok_or("missing activation epoch")?;
            let first = product.resume_owner_recovery_offline(key, &meta.secret, epoch, 4)?;
            if product.resume_owner_recovery_offline(key, &meta.secret, epoch, 5)? != first {
                return Err(failure("owner activation terminal replay changed"));
            }
        }
        OfflineCase::OwnerRecoveryAbort => {
            let old_secret = fs::read_to_string(directory.join(OWNER_KEY_FILE))?;
            if product.authenticate_api_key(&old_secret, 0).is_err()
                || product.authenticate_api_key(&meta.secret, 0).is_ok()
                || product.inspect_owner_recovery_offline()?.pending.is_none() != complete
            {
                return Err(failure("owner abort recovered state differs"));
            }
            let key = meta.key_id.ok_or("missing abort key")?;
            let epoch = meta.expected_epoch.ok_or("missing abort epoch")?;
            let first = product.abort_owner_recovery_offline(key, epoch, 4)?;
            if product.abort_owner_recovery_offline(key, epoch, 5)? != first {
                return Err(failure("owner abort terminal replay changed"));
            }
        }
        OfflineCase::LegacyBearerMigrate => {
            let expected = if complete {
                LegacyBearerState::MigrationPending
            } else {
                LegacyBearerState::NeverEnabled
            };
            if product.legacy_bearer_migration_inspection()?.state != expected {
                return Err(failure("legacy start recovered state differs"));
            }
        }
        OfflineCase::LegacyBearerActivate => {
            let expected = if complete {
                LegacyBearerState::DualWindow
            } else {
                LegacyBearerState::MigrationPending
            };
            if product.legacy_bearer_migration_inspection()?.state != expected {
                return Err(failure("legacy activation recovered state differs"));
            }
            let key = meta.key_id.ok_or("missing legacy key")?;
            let epoch = meta.expected_epoch.ok_or("missing legacy epoch")?;
            let first = product.activate_legacy_bearer_migration_offline(
                key,
                &meta.secret,
                epoch,
                "Legacy owner",
                "canonical",
                LEGACY_BEARER,
                3,
            )?;
            if product.activate_legacy_bearer_migration_offline(
                key,
                &meta.secret,
                epoch,
                "Legacy owner",
                "canonical",
                LEGACY_BEARER,
                4,
            )? != first
            {
                return Err(failure("legacy activation terminal replay changed"));
            }
        }
    }
    drop(product);
    assert_secret_free(&data, &[&meta.secret])
}

fn assert_secret_free(directory: &Path, secrets: &[&str]) -> Result<(), Box<dyn Error>> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
                continue;
            }
            let bytes = fs::read(entry.path())?;
            if bytes.windows(5).any(|window| window == b"hyp1_")
                || bytes
                    .windows(LEGACY_BEARER.len())
                    .any(|window| window == LEGACY_BEARER)
                || secrets
                    .iter()
                    .filter(|secret| !secret.is_empty())
                    .any(|secret| {
                        bytes
                            .windows(secret.len())
                            .any(|window| window == secret.as_bytes())
                    })
            {
                return Err(failure("secret persisted in the data directory"));
            }
        }
    }
    Ok(())
}

fn verify_case(
    directory: &Path,
    case: CrashCase,
    boundary: CommitBoundary,
) -> Result<(), Box<dyn Error>> {
    if directory.join(UNWIND_SENTINEL).exists() {
        return Err(failure("child unwound before SIGKILL"));
    }
    let meta = read_meta(directory)?;
    let complete = expects_complete(boundary);
    match case.kind {
        CrashKind::Managed(managed) => verify_managed(directory, managed, complete, &meta),
        CrashKind::Offline(offline) => verify_offline(directory, offline, complete, &meta),
    }
}

fn kill_child_at_boundary(
    executable: &Path,
    directory: &Path,
    case_id: &str,
    boundary: &str,
) -> Result<String, Box<dyn Error>> {
    let mut child = Command::new(executable)
        .arg(CHILD_MODE)
        .arg(directory)
        .arg(case_id)
        .arg(boundary)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let ready = wait_for_child_ready(&mut child)?;
    if ready.trim_end() != format!("{READY_PREFIX}{boundary}") {
        stop_child(&mut child);
        return Err(failure("child emitted unexpected boundary notification"));
    }
    let deadline = Instant::now() + CHILD_EXIT_GRACE;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(failure(format!(
                "child exited after boundary hook before SIGKILL: {status:?}"
            )));
        }
        thread::sleep(Duration::from_millis(5));
    }
    if directory.join(UNWIND_SENTINEL).exists() {
        stop_child(&mut child);
        return Err(failure("child unwound before parent SIGKILL"));
    }
    child.kill()?;
    validate_hard_kill(boundary, child.wait()?)
}

fn wait_for_child_ready(child: &mut Child) -> Result<String, Box<dyn Error>> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("child stdout was not piped"))?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ignored = sender.send(result);
    });
    let result = receiver.recv_timeout(CHILD_READY_TIMEOUT);
    if result.is_err() {
        stop_child(child);
    }
    reader
        .join()
        .map_err(|_| failure("child readiness reader panicked"))?;
    match result {
        Ok(line) => Ok(line?),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(failure("child did not reach boundary")),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(failure("child readiness channel disconnected"))
        }
    }
}

fn stop_child(child: &mut Child) {
    let _ignored = child.kill();
    let _ignored = child.wait();
}

#[cfg(unix)]
fn validate_hard_kill(name: &str, status: ExitStatus) -> Result<String, Box<dyn Error>> {
    use std::os::unix::process::ExitStatusExt as _;
    if status.signal() != Some(9) {
        return Err(failure(format!(
            "boundary {name} child was not terminated by SIGKILL: {status:?}"
        )));
    }
    Ok("signal-9".to_owned())
}

#[cfg(not(unix))]
fn validate_hard_kill(name: &str, status: ExitStatus) -> Result<String, Box<dyn Error>> {
    if status.success() {
        return Err(failure(format!(
            "boundary {name} child exited instead of being killed"
        )));
    }
    Ok(status.code().map_or_else(
        || "terminated-without-exit-code".to_owned(),
        |code| format!("exit-code-{code}"),
    ))
}

fn case_by_id(id: &str) -> Option<CrashCase> {
    CASES.into_iter().find(|case| case.id == id)
}
fn parse_boundary(name: &str) -> Option<CommitBoundary> {
    BOUNDARIES
        .iter()
        .find_map(|(candidate, boundary)| (*candidate == name).then_some(*boundary))
}
fn boundary_name(boundary: CommitBoundary) -> &'static str {
    BOUNDARIES
        .iter()
        .find_map(|(name, candidate)| (*candidate == boundary).then_some(*name))
        .unwrap_or("Unknown")
}
fn selected_case_count(index: usize, count: usize) -> usize {
    (0..CASES.len())
        .filter(|case| case % count == index)
        .count()
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

fn key_issue_start(
    principal_id: hyphae_native_product::SecurityId,
    self_manage: bool,
) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyIssueSelfStart {
            principal_id,
            label: "Crash issue".to_owned(),
            roles: vec![BuiltInRole::Admin],
            custom_roles: Vec::new(),
            permission_ceiling: BuiltInRole::Admin.authorization(),
            scope_ceiling: vec![ProductScope::Instance],
            expires_at_micros: None,
        }
    } else {
        ProductOperation::SecurityApiKeyIssueStart {
            principal_id,
            label: "Crash issue".to_owned(),
            roles: vec![BuiltInRole::Admin],
            custom_roles: Vec::new(),
            permission_ceiling: BuiltInRole::Admin.authorization(),
            scope_ceiling: vec![ProductScope::Instance],
            expires_at_micros: None,
        }
    }
}
fn key_issue_activate(
    key_id: hyphae_native_product::ApiKeyId,
    digest: ApiKeyConfirmationDigest,
    self_manage: bool,
) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyIssueSelfActivate {
            key_id,
            confirmation_digest: digest,
        }
    } else {
        ProductOperation::SecurityApiKeyIssueActivate {
            key_id,
            confirmation_digest: digest,
        }
    }
}
fn key_issue_abort(key_id: hyphae_native_product::ApiKeyId, self_manage: bool) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyIssueSelfAbort { key_id }
    } else {
        ProductOperation::SecurityApiKeyIssueAbort { key_id }
    }
}
fn key_rotate_start(
    key_id: hyphae_native_product::ApiKeyId,
    self_manage: bool,
) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyRotateSelfStart {
            predecessor_key_id: key_id,
            label: "Crash rotation".to_owned(),
            overlap_seconds: 60,
            expires_at_micros: None,
        }
    } else {
        ProductOperation::SecurityApiKeyRotateStart {
            predecessor_key_id: key_id,
            label: "Crash rotation".to_owned(),
            overlap_seconds: 60,
            expires_at_micros: None,
        }
    }
}
fn key_rotate_activate(
    key_id: hyphae_native_product::ApiKeyId,
    digest: ApiKeyConfirmationDigest,
    self_manage: bool,
) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id: key_id,
            confirmation_digest: digest,
        }
    } else {
        ProductOperation::SecurityApiKeyRotateActivate {
            successor_key_id: key_id,
            confirmation_digest: digest,
        }
    }
}
fn key_rotate_abort(
    key_id: hyphae_native_product::ApiKeyId,
    self_manage: bool,
) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyRotateSelfAbort {
            successor_key_id: key_id,
        }
    } else {
        ProductOperation::SecurityApiKeyRotateAbort {
            successor_key_id: key_id,
        }
    }
}
fn key_revoke(key_id: hyphae_native_product::ApiKeyId, self_manage: bool) -> ProductOperation {
    if self_manage {
        ProductOperation::SecurityApiKeyRevokeSelf { key_id }
    } else {
        ProductOperation::SecurityApiKeyRevoke { key_id }
    }
}

fn parse_optional_usize(
    value: Option<String>,
    default: usize,
    name: &str,
) -> Result<usize, Box<dyn Error>> {
    value.map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_| failure(format!("invalid {name}")))
    })
}
fn require_no_remaining(mut values: impl Iterator) -> Result<(), Box<dyn Error>> {
    if values.next().is_some() {
        Err(failure("unexpected additional argument"))
    } else {
        Ok(())
    }
}
fn validate_label(name: &str, value: &str) -> Result<(), Box<dyn Error>> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        Err(failure(format!("invalid {name} receipt label")))
    } else {
        Ok(())
    }
}
fn failure(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::other(message.into()).into()
}
