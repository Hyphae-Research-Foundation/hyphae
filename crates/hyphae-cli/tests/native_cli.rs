// SPDX-License-Identifier: Apache-2.0

//! Black-box conformance for the native-authority single binary.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use hyphae_client::v2::{ClientError, HttpTransport, HyphaeClient, RequestOptions};
#[cfg(unix)]
use hyphae_native_product::ProductResponse;
use hyphae_native_product::proof::{
    AdmittedProofLimits, CanonicalBytes, CompletionStatus, NativeProof, NativeProofAnchor,
    NativeProofContent, NativeProofKind, ProofCodecLimits, WitnessCodecLimits,
    bundle_native_witness, encode_native_proof,
};
use hyphae_native_product::{
    BuiltInRole, NativeProduct, ProductDocValue, ProductDocument, ProductDurability,
    ProductErrorCode, ProductSearchIngestBatch,
};
use hyphae_storage::{SnapshotReadLimits, load_snapshot};
use uuid::Uuid;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("hyphae-native-cli-{}", Uuid::now_v7()));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn run(arguments: &[&str]) -> Result<serde_json::Value, Box<dyn Error>> {
    let output = output(arguments)?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "hyphae {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn assert_json_keys(value: &serde_json::Value, expected: &[&str]) {
    let mut actual = value
        .as_object()
        .map(|object| object.keys().map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

fn output(arguments: &[&str]) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(arguments)
        .output()?)
}

/// Runs one CLI command with ambient credential variables removed, so tests
/// stay deterministic when the harness environment exports a key file.
fn run_isolated(arguments: &[&str]) -> Result<serde_json::Value, Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(arguments)
        .env_remove("HYPHAE_NATIVE_API_KEY_FILE")
        .env_remove("HYPHAE_BASE_URL")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "hyphae {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn security_output(data: &Path, key: &Path, operation: &[&str]) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(["security", "--data-dir"])
        .arg(data)
        .arg("--native-api-key-file")
        .arg(key)
        .args(operation)
        .output()?)
}

fn run_security(
    data: &Path,
    key: &Path,
    operation: &[&str],
) -> Result<serde_json::Value, Box<dyn Error>> {
    let output = security_output(data, key, operation)?;
    if !output.status.success() {
        return Err(std::io::Error::other(String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

struct SecurityWriteFixture {
    temporary: TestDirectory,
    data: PathBuf,
    owner_key: PathBuf,
    owner_secret: String,
}

impl SecurityWriteFixture {
    fn create() -> Result<Self, Box<dyn Error>> {
        let temporary = TestDirectory::new()?;
        let data = temporary.0.join("data");
        let owner_key = temporary.0.join("owner.key");
        run(&["init", "--data-dir", &path(&data)])?;
        run(&[
            "security",
            "--data-dir",
            &path(&data),
            "bootstrap",
            "--name",
            "Owner",
            "--key-out",
            &path(&owner_key),
        ])?;
        let owner_secret = fs::read_to_string(&owner_key)?;
        Ok(Self {
            temporary,
            data,
            owner_key,
            owner_secret,
        })
    }

    fn owner(&self, operation: &[&str]) -> Result<serde_json::Value, Box<dyn Error>> {
        run_security(&self.data, &self.owner_key, operation)
    }

    fn issue_reader_key(
        &self,
        principal_id: &str,
        destination: &Path,
    ) -> Result<String, Box<dyn Error>> {
        self.issue_built_in_key(principal_id, destination, BuiltInRole::Reader, "reader-cli")
    }

    fn issue_auditor_key(
        &self,
        principal_id: &str,
        destination: &Path,
    ) -> Result<String, Box<dyn Error>> {
        self.issue_built_in_key(
            principal_id,
            destination,
            BuiltInRole::Auditor,
            "auditor-mcp",
        )
    }

    fn issue_built_in_key(
        &self,
        principal_id: &str,
        destination: &Path,
        role: BuiltInRole,
        label: &str,
    ) -> Result<String, Box<dyn Error>> {
        let mut product = NativeProduct::open(&self.data)?;
        let authority = product.authenticate_api_key(self.owner_secret.trim(), 0)?;
        product.issue_api_key_to_file(
            &authority,
            principal_id.parse()?,
            label,
            [role],
            role.authorization(),
            None,
            destination,
            1,
        )?;
        Ok(fs::read_to_string(destination)?)
    }
}

#[test]
fn cli_api_key_issue_writes_restricted_file_before_activation() -> Result<(), Box<dyn Error>> {
    let fixture = SecurityWriteFixture::create()?;
    let principal = fixture.owner(&[
        "principal",
        "create",
        "--name",
        "CLI key target",
        "--idempotency-token",
        "901",
    ])?;
    let principal_id = principal["result_id"].as_str().ok_or("missing principal")?;
    fixture.owner(&[
        "assignment",
        "create-built-in",
        "--principal-id",
        principal_id,
        "--role",
        "reader",
        "--scope",
        "instance",
        "--idempotency-token",
        "902",
    ])?;
    let destination = fixture.temporary.0.join("cli-issued.key");
    let receipt = fixture.owner(&[
        "key",
        "issue",
        "--principal-id",
        principal_id,
        "--label",
        "cli-issued",
        "--role",
        "reader",
        "--permission",
        "catalog.read",
        "--permission",
        "credential.self_manage",
        "--permission",
        "data.read",
        "--permission",
        "discover",
        "--permission",
        "proof.generate",
        "--permission",
        "proof.verify",
        "--permission",
        "search.execute",
        "--scope",
        "instance",
        "--key-out",
        &path(&destination),
        "--idempotency-token",
        "903",
    ])?;
    assert_eq!(receipt["operation"], "security.key_issue");
    let secret = fs::read_to_string(&destination)?;
    assert!(secret.starts_with("hyp1_"));
    assert!(!receipt.to_string().contains(&secret));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&destination)?.permissions().mode() & 0o777,
            0o600
        );
    }
    Ok(())
}

#[test]
fn cli_key_issue_reserves_output_before_start_and_cleans_failed_start() -> Result<(), Box<dyn Error>>
{
    let fixture = SecurityWriteFixture::create()?;
    let before = NativeProduct::open(&fixture.data)?.access_control_status()?;
    let existing = fixture.temporary.0.join("existing-key-out");
    fs::write(&existing, b"existing-canary")?;
    let denied = security_output(
        &fixture.data,
        &fixture.owner_key,
        &[
            "key",
            "issue",
            "--principal-id",
            "00000000000000000000000000000001",
            "--label",
            "existing-output",
            "--role",
            "reader",
            "--permission",
            "data.read",
            "--scope",
            "instance",
            "--key-out",
            &path(&existing),
            "--idempotency-token",
            "904",
        ],
    )?;
    assert!(!denied.status.success());
    assert_eq!(fs::read(&existing)?, b"existing-canary");
    assert_eq!(
        NativeProduct::open(&fixture.data)?.access_control_status()?,
        before
    );

    let reserved = fixture.temporary.0.join("failed-start.key");
    let failed = security_output(
        &fixture.data,
        &fixture.owner_key,
        &[
            "key",
            "issue",
            "--principal-id",
            "00000000000000000000000000000001",
            "--label",
            "failed-start",
            "--role",
            "reader",
            "--permission",
            "data.read",
            "--scope",
            "instance",
            "--key-out",
            &path(&reserved),
            "--idempotency-token",
            "905",
        ],
    )?;
    assert!(!failed.status.success());
    assert!(!reserved.exists());
    assert_eq!(
        NativeProduct::open(&fixture.data)?.access_control_status()?,
        before
    );
    Ok(())
}

#[test]
fn cli_self_revoke_exact_replay_accepts_the_revoked_request_identity() -> Result<(), Box<dyn Error>>
{
    let fixture = SecurityWriteFixture::create()?;
    let key_id = NativeProduct::open(&fixture.data)?
        .authenticate_api_key(&fixture.owner_secret, 0)?
        .key_id()
        .to_string();
    let first = run_security(
        &fixture.data,
        &fixture.owner_key,
        &[
            "key",
            "revoke",
            "--key-id",
            &key_id,
            "--self-manage",
            "--idempotency-token",
            "906",
        ],
    )?;
    let replay = run_security(
        &fixture.data,
        &fixture.owner_key,
        &[
            "key",
            "revoke",
            "--key-id",
            &key_id,
            "--self-manage",
            "--idempotency-token",
            "906",
        ],
    )?;
    assert_eq!(replay, first);
    Ok(())
}

#[test]
fn explicit_upgrade_migrates_a_pre_binding_native_directory() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("old-native");
    drop(hyphae_native_runtime::NativeDatabase::create(&data)?);
    let before = output(&["capabilities", "--data-dir", &path(&data)])?;
    assert!(!before.status.success());

    let upgraded = run(&["upgrade", "--data-dir", &path(&data)])?;
    assert_eq!(upgraded["status"], "upgraded");
    assert_eq!(upgraded["default_scalar_keyspace_created"], true);
    let capabilities = run(&["capabilities", "--data-dir", &path(&data)])?;
    assert_eq!(capabilities["product_api_version"], 1);

    Ok(())
}

fn owner_output(data: &Path, operation: &[&str]) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(["security", "--data-dir"])
        .arg(data)
        .arg("owner")
        .args(operation)
        .output()?)
}

fn run_owner(data: &Path, operation: &[&str]) -> Result<serde_json::Value, Box<dyn Error>> {
    let output = owner_output(data, operation)?;
    if !output.status.success() {
        return Err(std::io::Error::other(String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn offline_owner_recovery_is_pending_until_exact_resume() -> Result<(), Box<dyn Error>> {
    let fixture = SecurityWriteFixture::create()?;
    let replacement = fixture.temporary.0.join("replacement.key");
    let started = run_owner(
        &fixture.data,
        &[
            "recover",
            "--label",
            "recovered-owner",
            "--key-out",
            &path(&replacement),
        ],
    )?;
    let replacement_secret = fs::read_to_string(&replacement)?;
    assert!(!started.to_string().contains(&replacement_secret));
    assert_eq!(started["status"], "pending");

    let inspected = run_owner(&fixture.data, &["inspect"])?;
    assert_eq!(
        inspected["pending"]["pending_key_id"],
        started["pending_key_id"]
    );
    assert!(!inspected.to_string().contains(&replacement_secret));

    let product = NativeProduct::open(&fixture.data)?;
    assert!(
        product
            .authenticate_api_key(fixture.owner_secret.trim(), 0)
            .is_ok()
    );
    let error = product
        .authenticate_api_key(&replacement_secret, 0)
        .err()
        .ok_or("pending replacement authenticated")?;
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    drop(product);

    let pending_key = started["pending_key_id"]
        .as_str()
        .ok_or("missing pending key")?;
    let pending_epoch = started["authorization_epoch"]
        .as_u64()
        .ok_or("missing pending epoch")?
        .to_string();
    let resumed = run_owner(
        &fixture.data,
        &[
            "resume",
            "--pending-key-id",
            pending_key,
            "--key-file",
            &path(&replacement),
            "--expected-authorization-epoch",
            &pending_epoch,
        ],
    )?;
    assert_eq!(resumed["status"], "activated");
    let replay = run_owner(
        &fixture.data,
        &[
            "resume",
            "--pending-key-id",
            pending_key,
            "--key-file",
            &path(&replacement),
            "--expected-authorization-epoch",
            &pending_epoch,
        ],
    )?;
    assert_eq!(replay, resumed);

    let product = NativeProduct::open(&fixture.data)?;
    assert!(product.authenticate_api_key(&replacement_secret, 0).is_ok());
    let error = product
        .authenticate_api_key(fixture.owner_secret.trim(), 0)
        .err()
        .ok_or("old owner remained valid")?;
    assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn offline_owner_recovery_rejects_bad_outputs_and_exact_abort_is_replayable()
-> Result<(), Box<dyn Error>> {
    let fixture = SecurityWriteFixture::create()?;
    let inside = fixture.data.join("inside.key");
    assert!(
        !owner_output(
            &fixture.data,
            &["recover", "--label", "inside", "--key-out", &path(&inside),],
        )?
        .status
        .success()
    );
    assert!(!inside.exists());

    let existing = fixture.temporary.0.join("existing.key");
    fs::write(&existing, b"canary-existing")?;
    assert!(
        !owner_output(
            &fixture.data,
            &[
                "recover",
                "--label",
                "existing",
                "--key-out",
                &path(&existing),
            ],
        )?
        .status
        .success()
    );
    assert_eq!(fs::read(&existing)?, b"canary-existing");

    let replacement = fixture.temporary.0.join("pending.key");
    let started = run_owner(
        &fixture.data,
        &[
            "recover",
            "--label",
            "pending",
            "--key-out",
            &path(&replacement),
        ],
    )?;
    let second = fixture.temporary.0.join("second.key");
    let denied = owner_output(
        &fixture.data,
        &["recover", "--label", "second", "--key-out", &path(&second)],
    )?;
    assert!(!denied.status.success());
    assert!(!second.exists());

    let pending_key = started["pending_key_id"]
        .as_str()
        .ok_or("missing pending key")?;
    let pending_epoch = started["authorization_epoch"]
        .as_u64()
        .ok_or("missing pending epoch")?
        .to_string();
    for bytes in [Vec::new(), b"hyp1_partial".to_vec(), vec![b'x'; 102]] {
        let wrong = fixture
            .temporary
            .0
            .join(format!("wrong-{}.key", bytes.len()));
        fs::write(&wrong, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&wrong, fs::Permissions::from_mode(0o600))?;
        }
        assert!(
            !owner_output(
                &fixture.data,
                &[
                    "resume",
                    "--pending-key-id",
                    pending_key,
                    "--key-file",
                    &path(&wrong),
                    "--expected-authorization-epoch",
                    &pending_epoch,
                ],
            )?
            .status
            .success()
        );
    }
    let aborted = run_owner(
        &fixture.data,
        &[
            "abort-pending",
            "--pending-key-id",
            pending_key,
            "--expected-authorization-epoch",
            &pending_epoch,
        ],
    )?;
    let replay = run_owner(
        &fixture.data,
        &[
            "abort-pending",
            "--pending-key-id",
            pending_key,
            "--expected-authorization-epoch",
            &pending_epoch,
        ],
    )?;
    assert_eq!(aborted, replay);
    let product = NativeProduct::open(&fixture.data)?;
    assert!(
        product
            .authenticate_api_key(fixture.owner_secret.trim(), 0)
            .is_ok()
    );
    Ok(())
}

#[test]
fn offline_owner_recovery_reserves_key_output_before_phase_one() -> Result<(), Box<dyn Error>> {
    let fixture = SecurityWriteFixture::create()?;
    let missing_parent = fixture.temporary.0.join("missing").join("owner.key");
    let before = NativeProduct::open(&fixture.data)?.access_control_status()?;
    let output = owner_output(
        &fixture.data,
        &[
            "recover",
            "--label",
            "must-not-start",
            "--key-out",
            &path(&missing_parent),
        ],
    )?;
    assert!(!output.status.success());
    let product = NativeProduct::open(&fixture.data)?;
    assert_eq!(product.access_control_status()?, before);
    drop(product);

    let conflict = fixture.temporary.0.join("reserved-conflict.key");
    let mut reservation = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&conflict)?;
    std::io::Write::write_all(&mut reservation, b"canary")?;
    drop(reservation);
    let output = owner_output(
        &fixture.data,
        &[
            "recover",
            "--label",
            "must-not-race",
            "--key-out",
            &path(&conflict),
        ],
    )?;
    assert!(!output.status.success());
    assert_eq!(fs::read(conflict)?, b"canary");
    assert_eq!(
        NativeProduct::open(&fixture.data)?.access_control_status()?,
        before
    );
    Ok(())
}

#[test]
fn offline_owner_recovery_rejects_lock_contention() -> Result<(), Box<dyn Error>> {
    let fixture = SecurityWriteFixture::create()?;
    let product = NativeProduct::open(&fixture.data)?;
    let output = owner_output(&fixture.data, &["inspect"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("data_directory_locked"));
    drop(product);
    Ok(())
}

#[cfg(unix)]
#[test]
fn offline_owner_recovery_rejects_symlink_directory_authority() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let fixture = SecurityWriteFixture::create()?;
    let link = fixture.temporary.0.join("data-link");
    symlink(&fixture.data, &link)?;
    let output = owner_output(&link, &["inspect"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("authorization_denied"));
    Ok(())
}

#[test]
fn offline_owner_recovery_recovers_after_phase_one_process_boundary() -> Result<(), Box<dyn Error>>
{
    let fixture = SecurityWriteFixture::create()?;
    let replacement = fixture.temporary.0.join("phase-one.key");
    let mut product = NativeProduct::open_offline_owner(&fixture.data)?;
    let started = product.start_owner_recovery_offline("phase-one", 1)?;
    write_restricted_test_key(&replacement, started.secret.expose_secret_bytes())?;
    let pending_key = started.key_id.to_string();
    let pending_epoch = started.authorization_epoch.get().to_string();
    drop(product);

    let inspected = run_owner(&fixture.data, &["inspect"])?;
    assert_eq!(inspected["pending"]["pending_key_id"], pending_key);
    let resumed = run_owner(
        &fixture.data,
        &[
            "resume",
            "--pending-key-id",
            &pending_key,
            "--key-file",
            &path(&replacement),
            "--expected-authorization-epoch",
            &pending_epoch,
        ],
    )?;
    assert_eq!(resumed["status"], "activated");
    Ok(())
}

fn write_restricted_test_key(path: &Path, secret: &[u8]) -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::GENERIC_WRITE,
            Storage::FileSystem::{READ_CONTROL, WRITE_DAC, WRITE_OWNER},
        };

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER)
            .share_mode(0)
            .open(path)?;
        // Match the product's Windows restricted-output contract rather than
        // inheriting the test runner's temporary-directory ACL.
        hyphae_native_product::restrict_windows_credential_file(path, &file)?;
        std::io::Write::write_all(&mut file, secret)?;
        file.sync_all()?;
        hyphae_native_product::validate_windows_restricted_file(&file)?;
    }
    #[cfg(not(windows))]
    fs::write(path, secret)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[test]
fn legacy_bearer_migration_and_terminal_revoke_are_file_only_and_redacted()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let legacy_path = temporary.0.join("legacy.bearer");
    let owner_path = temporary.0.join("migrated-owner.key");
    let canary = "legacy-cli-canary-0123456789abcdef";
    run(&["init", "--data-dir", &path(&data)])?;
    write_restricted_test_key(&legacy_path, canary.as_bytes())?;

    let migrated = output(&[
        "security",
        "--data-dir",
        &path(&data),
        "legacy-bearer",
        "migrate",
        "--name",
        "Migrated owner",
        "--label",
        "canonical-owner",
        "--legacy-bearer-file",
        &path(&legacy_path),
        "--key-out",
        &path(&owner_path),
    ])?;
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let migration: serde_json::Value = serde_json::from_slice(&migrated.stdout)?;
    assert_eq!(migration["status"], "dual_window");
    assert!(!String::from_utf8_lossy(&migrated.stdout).contains(canary));
    assert!(!String::from_utf8_lossy(&migrated.stderr).contains(canary));
    let canonical = fs::read_to_string(&owner_path)?;
    assert!(canonical.starts_with("hyp1_"));
    assert!(!migration.to_string().contains(&canonical));
    let product = NativeProduct::open(&data)?;
    assert_eq!(
        product.legacy_bearer_migration_inspection()?.state,
        hyphae_native_product::LegacyBearerState::DualWindow
    );
    drop(product);

    let revoked = security_output(
        &data,
        &owner_path,
        &["legacy-bearer", "revoke", "--idempotency-token", "9901"],
    )?;
    assert!(
        revoked.status.success(),
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    let revocation: serde_json::Value = serde_json::from_slice(&revoked.stdout)?;
    assert_eq!(revocation["status"], "revoked");
    assert!(!String::from_utf8_lossy(&revoked.stdout).contains(canary));
    let product = NativeProduct::open(&data)?;
    assert_eq!(
        product.legacy_bearer_migration_inspection()?.state,
        hyphae_native_product::LegacyBearerState::Revoked
    );
    drop(product);
    assert_directory_files_exclude(&data, canary.as_bytes())?;
    Ok(())
}

#[test]
fn legacy_migration_reserves_key_output_before_phase_one() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let legacy_path = temporary.0.join("legacy.bearer");
    let missing_output = temporary.0.join("missing").join("owner.key");
    let canary = "legacy-reservation-canary-0123456789abcdef";
    run(&["init", "--data-dir", &path(&data)])?;
    write_restricted_test_key(&legacy_path, canary.as_bytes())?;

    let failed = output(&[
        "security",
        "--data-dir",
        &path(&data),
        "legacy-bearer",
        "migrate",
        "--name",
        "Migrated owner",
        "--label",
        "canonical-owner",
        "--legacy-bearer-file",
        &path(&legacy_path),
        "--key-out",
        &path(&missing_output),
    ])?;
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stdout).contains(canary));
    assert!(!String::from_utf8_lossy(&failed.stderr).contains(canary));
    let product = NativeProduct::open(&data)?;
    assert_eq!(
        product.legacy_bearer_migration_inspection()?.state,
        hyphae_native_product::LegacyBearerState::NeverEnabled
    );
    drop(product);
    assert_directory_files_exclude(&data, canary.as_bytes())?;
    Ok(())
}

fn assert_directory_files_exclude(directory: &Path, canary: &[u8]) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            assert_directory_files_exclude(&entry.path(), canary)?;
        } else if file_type.is_file() {
            let bytes = fs::read(entry.path())?;
            assert!(
                !bytes.windows(canary.len()).any(|window| window == canary),
                "legacy bearer canary was persisted"
            );
        }
    }
    Ok(())
}

fn assert_security_mutation_receipt(receipt: &serde_json::Value, operation: &str) {
    assert_json_keys(
        receipt,
        &[
            "schema",
            "operation",
            "result_id",
            "authorization_epoch",
            "commit",
        ],
    );
    assert_eq!(receipt["schema"], "hyphae-native-security-mutation-v1");
    assert_eq!(receipt["operation"], operation);
    assert!(receipt["result_id"].as_str().is_some());
    assert_eq!(receipt["commit"]["durability"], "strict");
    assert!(receipt["authorization_epoch"].as_u64().is_some());
}

struct SecurityReadPlaneOutput {
    status: serde_json::Value,
    principals: serde_json::Value,
    roles_first: serde_json::Value,
    roles_second: serde_json::Value,
    assignments: serde_json::Value,
    keys: serde_json::Value,
    audit_first: serde_json::Value,
    audit_second: serde_json::Value,
}

fn load_security_read_plane(
    data: &Path,
    owner_key: &Path,
) -> Result<SecurityReadPlaneOutput, Box<dyn Error>> {
    let status = run_security(data, owner_key, &["status"])?;
    let principals = run_security(data, owner_key, &["principal", "list", "--limit", "1"])?;
    let roles_first = run_security(data, owner_key, &["role", "list", "--limit", "2"])?;
    let role_cursor = roles_first["next_cursor"]
        .as_str()
        .ok_or("missing role continuation")?;
    let roles_second = run_security(
        data,
        owner_key,
        &["role", "list", "--cursor", role_cursor, "--limit", "2"],
    )?;
    assert_ne!(roles_first["items"], roles_second["items"]);
    assert_eq!(
        security_output(data, owner_key, &["key", "list", "--cursor", role_cursor])?
            .status
            .code(),
        Some(2)
    );
    let assignments = run_security(data, owner_key, &["assignment", "list", "--limit", "1"])?;
    let keys = run_security(data, owner_key, &["key", "list", "--limit", "1"])?;
    let audit_first = run_security(data, owner_key, &["audit", "list", "--limit", "1"])?;
    let audit_cursor = audit_first["next_cursor"]
        .as_str()
        .ok_or("missing audit continuation")?;
    let audit_second = run_security(
        data,
        owner_key,
        &["audit", "list", "--cursor", audit_cursor, "--limit", "1"],
    )?;
    assert_ne!(audit_first["items"], audit_second["items"]);
    Ok(SecurityReadPlaneOutput {
        status,
        principals,
        roles_first,
        roles_second,
        assignments,
        keys,
        audit_first,
        audit_second,
    })
}

fn assert_security_page_envelopes(output: &SecurityReadPlaneOutput) {
    assert_json_keys(
        &output.status,
        &[
            "schema",
            "bootstrapped",
            "authorization_epoch",
            "principals",
            "assignments",
            "custom_roles",
            "custom_assignments",
            "keys",
            "pending_keys",
            "audit_events",
        ],
    );
    assert_eq!(
        output.status["schema"],
        "hyphae-native-access-control-status-v1"
    );
    for (page, schema) in [
        (&output.principals, "hyphae-native-security-principals-v1"),
        (&output.roles_first, "hyphae-native-security-roles-v1"),
        (&output.assignments, "hyphae-native-security-assignments-v1"),
        (&output.keys, "hyphae-native-security-keys-v1"),
    ] {
        assert_json_keys(
            page,
            &["schema", "authorization_epoch", "items", "next_cursor"],
        );
        assert_eq!(page["schema"], schema);
    }
    for page in [&output.audit_first, &output.audit_second] {
        assert_json_keys(page, &["schema", "items", "next_cursor"]);
        assert_eq!(page["schema"], "hyphae-native-security-audit-v1");
    }
}

fn assert_security_item_shapes(output: &SecurityReadPlaneOutput) {
    assert_json_keys(
        &output.principals["items"][0],
        &["id", "display_name", "enabled"],
    );
    assert_json_keys(
        &output.roles_first["items"][0],
        &["kind", "id", "display_name", "permissions", "grants"],
    );
    assert_json_keys(
        &output.assignments["items"][0],
        &[
            "id",
            "principal_id",
            "built_in_role",
            "custom_role_id",
            "scope",
        ],
    );
    assert_json_keys(
        &output.keys["items"][0],
        &[
            "id",
            "principal_id",
            "label",
            "active",
            "roles",
            "custom_roles",
            "permission_ceiling",
            "scope_ceiling",
            "created_at_micros",
            "expires_at_micros",
            "revoked",
            "published_epoch",
            "predecessor_id",
            "successor_id",
            "overlap_until_micros",
            "rotation_overlap_micros",
        ],
    );
    assert_json_keys(
        &output.audit_first["items"][0],
        &[
            "id",
            "commit_csn",
            "actor_principal_id",
            "actor_key_id",
            "action",
            "result",
            "targets",
            "metadata",
        ],
    );
    assert!(output.principals["next_cursor"].is_null());
    assert!(
        output.roles_first["next_cursor"]
            .as_str()
            .is_some_and(|cursor| cursor.starts_with("hysec1:"))
    );
    assert!(output.assignments["items"][0]["custom_role_id"].is_null());
    assert!(output.keys["items"][0]["expires_at_micros"].is_null());
}

fn assert_security_output_is_redacted(output: &SecurityReadPlaneOutput, owner_secret: &str) {
    for response in [
        &output.status,
        &output.principals,
        &output.roles_first,
        &output.roles_second,
        &output.assignments,
        &output.keys,
        &output.audit_first,
        &output.audit_second,
    ] {
        let rendered = response.to_string();
        assert!(!rendered.contains(owner_secret.trim()));
        assert!(!rendered.contains("verifier"));
        assert!(!rendered.contains("secret"));
    }
}

fn assert_authorization_denied(arguments: &[&str]) -> Result<Output, Box<dyn Error>> {
    let denied = output(arguments)?;
    assert_eq!(denied.status.code(), Some(8));
    let error: serde_json::Value = serde_json::from_slice(&denied.stderr)?;
    assert_eq!(error["error"]["code"], "authorization_denied");
    Ok(denied)
}

fn run_with_api_key_stdin(
    data: &Path,
    credential: &[u8],
) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args([
            "capabilities",
            "--data-dir",
            &path(data),
            "--native-api-key-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        use std::io::Write as _;

        let mut stdin = child.stdin.take().ok_or("missing child stdin")?;
        stdin.write_all(credential)?;
        stdin.write_all(b"\n")?;
    }
    let result = child.wait_with_output()?;
    if !result.status.success() {
        return Err(std::io::Error::other(String::from_utf8_lossy(&result.stderr)).into());
    }
    Ok(serde_json::from_slice(&result.stdout)?)
}

fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn materialize_format2_fixture(destination: &Path) -> Result<(), Box<dyn Error>> {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/format2-data-directory.json"))?;
    for (relative, encoded) in fixture["files_hex"]
        .as_object()
        .ok_or("fixture files_hex is not an object")?
    {
        let output = destination.join(relative);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = encoded.as_str().ok_or("fixture file is not hex")?;
        let bytes = bytes
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        fs::write(output, bytes)?;
    }
    Ok(())
}

#[test]
fn init_is_explicit_and_read_only_commands_never_create() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    let help = output(&["--help"])?;
    let help = String::from_utf8(help.stdout)?;
    for family in [
        "capabilities",
        "init",
        "catalog",
        "sql",
        "structure",
        "search",
        "transaction",
        "explain",
        "status",
        "telemetry",
        "doctor",
        "checkpoint",
        "compact",
        "vacuum",
        "backup",
        "restore",
        "proof",
        "serve",
    ] {
        assert!(help.contains(family), "missing command family {family}");
    }
    assert!(!help.contains("--native"));
    for compatibility in [
        "put",
        "get",
        "delete",
        "query",
        "snapshot",
        "backup-verify",
        "verify",
        "verify-retrieval",
        "remote",
        "mcp",
    ] {
        assert!(
            help.contains(compatibility),
            "missing compatibility command {compatibility}"
        );
    }

    for arguments in [
        vec!["status", "--data-dir", &data_text],
        vec!["catalog", "--data-dir", &data_text, "list"],
    ] {
        let failed = output(&arguments)?;
        assert!(!failed.status.success());
        assert!(!data.exists());
    }
    let doctor = run(&["doctor", "--data-dir", &data_text])?;
    assert_eq!(doctor["status"], "corrupt");
    assert!(!data.exists());

    let initialized = run(&["init", "--data-dir", &data_text])?;
    assert_eq!(initialized["status"], "initialized");
    assert_eq!(initialized["native_directory_format"], 1);
    let repeated = output(&["init", "--data-dir", &data_text])?;
    assert_eq!(repeated.status.code(), Some(4));
    let capabilities = run(&["capabilities", "--data-dir", &data_text])?;
    assert_eq!(capabilities["native_directory_format"], 1);
    let compatibility = output(&["get", "--data-dir", &data_text, "--key", "native"])?;
    assert!(!compatibility.status.success());
    assert!(fs::read_to_string(data.join("FORMAT"))?.starts_with("hyphae-native-format=1 "));
    assert!(!data.join("hyphae.sock").exists());
    assert!(!data.join("indexes").exists());
    assert!(!data.join("log").exists());
    Ok(())
}

#[test]
fn search_collection_helper_is_one_atomic_catalog_commit() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run(&["init", "--data-dir", &data_text])?;
    run(&[
        "sql",
        "--data-dir",
        &data_text,
        "execute",
        "--statement",
        "CREATE TABLE occupied (id BIGINT PRIMARY KEY)",
    ])?;
    let failed = output(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.occupied",
    ])?;
    assert!(!failed.status.success());
    let catalog = run(&["catalog", "--data-dir", &data_text, "list"])?;
    let items = catalog["items"].as_array().ok_or("catalog items absent")?;
    let occupied = items
        .iter()
        .filter(|item| item["name"] == "main.public.occupied")
        .count();
    assert_eq!(occupied, 1);
    Ok(())
}

#[test]
fn catalog_list_round_trips_the_complete_opaque_cursor_between_processes()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run(&["init", "--data-dir", &data_text])?;

    let first = run(&["catalog", "--data-dir", &data_text, "list", "--limit", "1"])?;
    let cursor = first["cursor"]
        .as_str()
        .ok_or("catalog first page omitted its opaque cursor")?;
    assert!(cursor.starts_with("hycatv1:"));
    let first_id = first["items"][0]["id"]
        .as_str()
        .ok_or("catalog first page omitted its ID")?;

    let second = run(&[
        "catalog",
        "--data-dir",
        &data_text,
        "list",
        "--limit",
        "1",
        "--cursor",
        cursor,
    ])?;
    assert_ne!(second["items"][0]["id"].as_str(), Some(first_id));
    Ok(())
}

#[test]
fn separate_cli_transactions_use_distinct_default_idempotency() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run(&["init", "--data-dir", &data_text])?;
    run(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.search",
    ])?;
    run(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-keyspace",
        "--id",
        "20",
        "--parent",
        "11",
        "--name",
        "main.public.values",
        "--family",
        "string",
    ])?;
    for key in ["first", "second"] {
        let steps = serde_json::json!([
            {"operation":"stage_structure","mutation":{
                "operation":"string_set","keyspace":20,"key":key,
                "value":"committed","expires_at_micros":null
            }},
            {"operation":"commit"}
        ])
        .to_string();
        let result = run(&[
            "transaction",
            "--data-dir",
            &data_text,
            "execute",
            "--steps-json",
            &steps,
        ])?;
        assert_eq!(result["steps"][2]["status"], "committed");
    }
    Ok(())
}

#[test]
fn format2_migration_runs_verifies_promotes_and_keeps_source_unchanged()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let source = temporary.0.join("source");
    let target = temporary.0.join("target");
    let manifest = temporary.0.join("migration.json");
    materialize_format2_fixture(&source)?;
    let source_snapshot = load_snapshot(
        source.join("snapshots/snapshot-00000000000000000014.hysnap"),
        &SnapshotReadLimits::default(),
    )?;
    let expected_values = source_snapshot
        .entries
        .iter()
        .map(|entry| (entry.key.clone(), entry.value.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let before = fs::read_dir(&source)?
        .map(|entry| {
            let entry = entry?;
            Ok((entry.path(), fs::metadata(entry.path())?.len()))
        })
        .collect::<Result<Vec<_>, std::io::Error>>()?;

    let imported = run(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
    ])?;
    assert_eq!(imported["status"], "imported");
    assert_eq!(imported["documents"], 2);
    assert_eq!(imported["receipts"], 4);
    assert!(target.join("FORMAT.pending").exists());

    let verified = run(&[
        "migrate",
        "verify",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
    ])?;
    assert_eq!(verified["status"], "verified");
    assert_eq!(verified["pending"], true);

    let promoted = run(&[
        "migrate",
        "promote",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
    ])?;
    assert_eq!(promoted["status"], "promoted");
    assert!(target.join("FORMAT").exists());
    assert!(!target.join("FORMAT.pending").exists());
    let reopened = run(&["status", "--data-dir", &path(&target)])?;
    assert_eq!(reopened["status"], "ready");
    for key in ["alpha", "beta"] {
        let request = serde_json::json!({
            "operation": "string_get",
            "keyspace": 3,
            "key": key,
        })
        .to_string();
        let read = run(&[
            "structure",
            "--data-dir",
            &path(&target),
            "read",
            "--request-json",
            &request,
        ])?;
        let expected = expected_values
            .get(key.as_bytes())
            .ok_or("missing expected migrated record")?;
        let expected_hex = expected.iter().fold(String::new(), |mut encoded, byte| {
            use std::fmt::Write;
            let _ = write!(encoded, "{byte:02x}");
            encoded
        });
        assert_eq!(read["result"]["value_hex"], expected_hex);
    }

    for (path, length) in before {
        assert_eq!(
            fs::metadata(&path)?.len(),
            length,
            "source changed: {}",
            path.display()
        );
    }
    assert!(source.join("FORMAT").exists());
    Ok(())
}

#[test]
fn migration_rejects_source_output_overlap_and_rolls_back_pending_target()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let source = temporary.0.join("source");
    materialize_format2_fixture(&source)?;
    let output_inside_source = source.join("manifest.json");
    let rejected = output(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&temporary.0.join("target")),
        "--manifest",
        &path(&output_inside_source),
    ])?;
    assert!(!rejected.status.success());
    assert!(!output_inside_source.exists());

    let target = temporary.0.join("target");
    let manifest = temporary.0.join("manifest.json");
    run(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
    ])?;
    assert!(target.join("FORMAT.pending").exists());
    let rolled_back = run(&[
        "migrate",
        "rollback",
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
    ])?;
    assert_eq!(rolled_back["status"], "rolled_back");
    assert!(!target.exists());
    assert!(source.exists());
    Ok(())
}

#[test]
fn version_json_keeps_the_release_contract() -> Result<(), Box<dyn Error>> {
    let version = run(&["version", "--json"])?;
    assert_eq!(version["product"], "hyphae");
    assert_eq!(version["engine_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(version["api_version"], "v1");
    assert_eq!(version["disk_format_version"], 2);
    assert_eq!(version["product_api_version"], 1);
    assert_eq!(version["native_directory_format"], 1);
    Ok(())
}

#[test]
fn doctor_reports_busy_corrupt_and_io_without_preopening() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run(&["init", "--data-dir", &data_text])?;

    let product = hyphae_native_product::NativeProduct::open(&data)?;
    let busy = run(&["doctor", "--data-dir", &data_text])?;
    assert_eq!(busy["status"], "busy");
    assert_eq!(busy["verified_open"], false);
    drop(product);

    let corrupt = temporary.0.join("corrupt");
    fs::create_dir(&corrupt)?;
    let corrupt = run(&["doctor", "--data-dir", &path(&corrupt)])?;
    assert_eq!(corrupt["status"], "corrupt");

    let io_path = temporary.0.join("io-error");
    fs::create_dir(&io_path)?;
    fs::create_dir(io_path.join("LOCK"))?;
    let io = run(&["doctor", "--data-dir", &path(&io_path)])?;
    assert_eq!(io["status"], "io");
    assert_eq!(io["verified_open"], false);

    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn native_sql_structure_status_and_administration_are_exposed() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run(&["init", "--data-dir", &data_text])?;

    let created = run(&[
        "sql",
        "--data-dir",
        &data_text,
        "execute",
        "--statement",
        "CREATE TABLE items (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
    ])?;
    assert_eq!(created["commit"]["status"], "committed");
    run(&[
        "sql",
        "--data-dir",
        &data_text,
        "execute",
        "--statement",
        "INSERT INTO items (id, name) VALUES (?, ?)",
        "--parameter",
        "1",
        "--parameter",
        r#""alpha""#,
    ])?;
    let selected = run(&[
        "sql",
        "--data-dir",
        &data_text,
        "execute",
        "--statement",
        "SELECT id, name FROM items WHERE id = ?",
        "--parameter",
        "1",
    ])?;
    assert_eq!(
        selected["result"]["rows"][0],
        serde_json::json!([1, "alpha"])
    );

    assert_eq!(
        run(&[
            "structure",
            "--data-dir",
            &data_text,
            "set",
            "--key",
            "session",
            "--value",
            "ready",
        ])?["status"],
        "committed"
    );
    let read = run(&[
        "structure",
        "--data-dir",
        &data_text,
        "get",
        "--key",
        "session",
    ])?;
    assert_eq!(read["value"], "ready");
    assert_eq!(
        run(&["status", "--data-dir", &data_text])?["status"],
        "ready"
    );
    assert_eq!(
        run(&["doctor", "--data-dir", &data_text])?["status"],
        "healthy"
    );
    assert_eq!(
        run(&["checkpoint", "--data-dir", &data_text])?["status"],
        "checkpointed"
    );
    assert!(run(&["catalog", "--data-dir", &data_text, "list"])?["items"].is_array());
    assert!(run(&["telemetry", "--data-dir", &data_text])?["metrics"].is_array());
    let explained = run(&[
        "explain",
        "--data-dir",
        &data_text,
        "sql",
        "--statement",
        "SELECT id, name FROM items WHERE id = 1",
    ])?;
    assert_eq!(explained["type"], "sql_plan_text");
    assert_eq!(
        run(&[
            "transaction",
            "--data-dir",
            &data_text,
            "status",
            "--id",
            "999",
        ])?["status"],
        "unknown"
    );
    let compacted = run(&["compact", "--data-dir", &data_text])?;
    assert!(matches!(
        compacted["status"].as_str(),
        Some("compacted" | "no_changes")
    ));
    let vacuumed = run(&["vacuum", "--data-dir", &data_text])?;
    assert!(matches!(
        vacuumed["status"].as_str(),
        Some("vacuumed" | "no_changes")
    ));
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn native_backup_restore_and_proof_verification_are_offline() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let backup = temporary.0.join("backup");
    let restored = temporary.0.join("restored");
    let data_text = path(&data);
    let backup_text = path(&backup);
    let restored_text = path(&restored);
    run(&["init", "--data-dir", &data_text])?;
    run(&[
        "structure",
        "--data-dir",
        &data_text,
        "set",
        "--key",
        "durable",
        "--value",
        "yes",
    ])?;
    assert_eq!(
        run(&[
            "backup",
            "create",
            "--data-dir",
            &data_text,
            "--out",
            &backup_text,
        ])?["status"],
        "created"
    );
    assert_eq!(
        run(&["backup", "verify", "--backup", &backup_text])?["status"],
        "verified"
    );
    assert_eq!(
        run(&[
            "restore",
            "--backup",
            &backup_text,
            "--data-dir",
            &restored_text,
        ])?["status"],
        "restored"
    );
    assert_eq!(
        run(&[
            "structure",
            "--data-dir",
            &restored_text,
            "get",
            "--key",
            "durable",
        ])?["value"],
        "yes"
    );

    let origin = temporary.0.join("witness-origin");
    fs::create_dir(&origin)?;
    fs::write(origin.join("ROOT"), b"native proof fixture")?;
    let anchor = NativeProofAnchor {
        directory_lineage: [3; 24],
        history_epoch: 1,
        visible_csn: 1,
        catalog_version: 1,
        root_digest: [4; 32],
        checkpoint_sequence: 1,
        checkpoint_digest: [5; 32],
    };
    let witness = bundle_native_witness(&origin, anchor, &WitnessCodecLimits::default())?;
    let proof = NativeProof::new(NativeProofContent {
        kind: NativeProofKind::Point,
        anchor,
        semantics_version: 1,
        ordering_version: 1,
        objects: Vec::new(),
        request: CanonicalBytes::new(b"request".to_vec()),
        result: CanonicalBytes::new(b"result".to_vec()),
        evidence: CanonicalBytes::new(b"evidence".to_vec()),
        limits: AdmittedProofLimits {
            result_items: 1,
            candidate_items: 0,
            evidence_bytes: 8,
        },
        completion: CompletionStatus::Complete,
        witness: witness.reference()?,
        ann: None,
        hybrid: None,
    })?;
    let proof_path = temporary.0.join("proof.hynproof");
    let witness_path = temporary.0.join("witness.hynwit");
    fs::write(
        &proof_path,
        encode_native_proof(&proof, &ProofCodecLimits::default())?,
    )?;
    fs::write(&witness_path, witness.bytes)?;
    let verified = run(&[
        "proof",
        "verify",
        "--proof",
        &path(&proof_path),
        "--witness",
        &path(&witness_path),
        "--anchor",
        &encode_hex(&anchor.digest()),
    ])?;
    assert_eq!(verified["status"], "verified");
    assert_eq!(verified["kind"], "point");
    Ok(())
}

#[cfg(unix)]
fn exercise_service(data: &Path) -> Result<(), Box<dyn Error>> {
    let endpoint = std::env::temp_dir().join(format!("hyphae-cli-{}.sock", Uuid::now_v7()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .arg("serve")
        .arg("--data-dir")
        .arg(data)
        .arg("--endpoint")
        .arg(&endpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !endpoint.exists() && Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            let error = read_child_stderr(&mut child)?;
            return Err(std::io::Error::other(error).into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(endpoint.exists());
    let _guard = ChildGuard(&mut child);
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}

#[cfg(windows)]
fn exercise_service(data: &Path) -> Result<(), Box<dyn Error>> {
    let endpoint = format!("hyphae-cli-{}", Uuid::now_v7());
    let mut child = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .arg("serve")
        .arg("--data-dir")
        .arg(data)
        .arg("--endpoint")
        .arg(endpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    thread::sleep(Duration::from_millis(500));
    if child.try_wait()?.is_some() {
        let error = read_child_stderr(&mut child)?;
        return Err(std::io::Error::other(error).into());
    }
    let _guard = ChildGuard(&mut child);
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn full_admitted_operation_corpus_runs_through_the_single_binary() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let backup = temporary.0.join("backup");
    let restored = temporary.0.join("restored");
    let proof = temporary.0.join("read.hynproof");
    let witness = temporary.0.join("read.hynwit");
    let data_text = path(&data);
    run(&["init", "--data-dir", &data_text])?;

    assert_eq!(
        run(&["capabilities", "--data-dir", &data_text])?["product_api_version"],
        1
    );
    run(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.products",
    ])?;
    for (id, family, name) in [
        (20, "string", "strings"),
        (21, "counter", "counters"),
        (22, "hash", "hashes"),
        (23, "list", "lists"),
        (24, "set", "sets"),
        (25, "sorted-set", "sorted"),
        (26, "stream", "streams"),
    ] {
        run(&[
            "catalog",
            "--data-dir",
            &data_text,
            "create-keyspace",
            "--id",
            &id.to_string(),
            "--parent",
            "11",
            "--name",
            &format!("main.public.{name}"),
            "--family",
            family,
        ])?;
    }
    assert!(run(&["catalog", "--data-dir", &data_text, "list"])?["items"].is_array());
    assert_eq!(
        run(&[
            "catalog",
            "--data-dir",
            &data_text,
            "describe",
            "--id",
            "13",
        ])?["object"]["kind"],
        "search_collection"
    );
    assert_eq!(
        run(&[
            "catalog",
            "--data-dir",
            &data_text,
            "resolve",
            "--name",
            "main.public.products",
        ])?["object"]["id"],
        "13"
    );
    assert!(
        run(&[
            "catalog",
            "--data-dir",
            &data_text,
            "dependencies",
            "--id",
            "13",
        ])?["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );

    assert_eq!(
        run(&[
            "sql",
            "--data-dir",
            &data_text,
            "execute",
            "--statement",
            "CREATE TABLE events (id BIGINT PRIMARY KEY, body TEXT NOT NULL)",
        ])?["commit"]["status"],
        "committed"
    );
    run(&[
        "sql",
        "--data-dir",
        &data_text,
        "execute",
        "--statement",
        "INSERT INTO events (id, body) VALUES (?, ?)",
        "--parameter",
        "1",
        "--parameter",
        r#""first""#,
    ])?;
    assert_eq!(
        run(&[
            "sql",
            "--data-dir",
            &data_text,
            "execute",
            "--statement",
            "SELECT id, body FROM events WHERE id = ?",
            "--parameter",
            "1",
        ])?["result"]["rows"][0],
        serde_json::json!([1, "first"])
    );
    assert_eq!(
        run(&[
            "sql",
            "--data-dir",
            &data_text,
            "prepared",
            "--statement",
            "SELECT id, body FROM events WHERE id = ?",
            "--parameter",
            "1",
        ])?["deallocated"],
        true
    );
    assert_eq!(
        run(&[
            "explain",
            "--data-dir",
            &data_text,
            "sql",
            "--statement",
            "SELECT id, body FROM events WHERE id = 1",
        ])?["type"],
        "sql_plan_text"
    );

    run(&[
        "structure",
        "--data-dir",
        &data_text,
        "set",
        "--key",
        "scalar",
        "--value",
        "native",
    ])?;
    assert_eq!(
        run(&[
            "structure",
            "--data-dir",
            &data_text,
            "get",
            "--key",
            "scalar",
        ])?["value"],
        "native"
    );
    assert_eq!(
        run(&[
            "structure",
            "--data-dir",
            &data_text,
            "ttl",
            "--key",
            "scalar",
        ])?["status"],
        "persistent"
    );
    assert!(matches!(
        run(&["compact", "--data-dir", &data_text])?["status"].as_str(),
        Some("compacted" | "no_changes")
    ));
    let mutations = serde_json::json!([
        {"operation":"string_set","keyspace":20,"key":"message","value":"hello","expires_at_micros":4_000_000_000_000_000_000_i64},
        {"operation":"counter_add","keyspace":21,"key":"count","delta":3},
        {"operation":"create","keyspace":22,"key":"hash","family":"hash"},
        {"operation":"hash_set","keyspace":22,"key":"hash","field":"name","value":"hyphae"},
        {"operation":"hash_counter_add","keyspace":22,"key":"hash","field":"visits","delta":2},
        {"operation":"hash_expire_field","keyspace":22,"key":"hash","field":"name","expires_at_micros":4_000_000_000_000_000_000_i64},
        {"operation":"create","keyspace":23,"key":"list","family":"list"},
        {"operation":"list_push","keyspace":23,"key":"list","side":"right","value":"item"},
        {"operation":"create","keyspace":24,"key":"set-a","family":"set"},
        {"operation":"set_add","keyspace":24,"key":"set-a","member":"shared"},
        {"operation":"create","keyspace":24,"key":"set-b","family":"set"},
        {"operation":"set_add","keyspace":24,"key":"set-b","member":"shared"},
        {"operation":"set_add","keyspace":24,"key":"set-b","member":"second"},
        {"operation":"create","keyspace":25,"key":"ranked","family":"sorted_set"},
        {"operation":"sorted_set_add","keyspace":25,"key":"ranked","member":"first","score":1.5},
        {"operation":"create","keyspace":26,"key":"events","family":"stream"},
        {"operation":"stream_add","keyspace":26,"key":"events","fields":{"kind":"created"}}
    ]).to_string();
    assert_eq!(
        run(&[
            "structure",
            "--data-dir",
            &data_text,
            "batch",
            "--mutations-json",
            &mutations,
        ])?["status"],
        "committed"
    );
    for (request, expected) in [
        (
            serde_json::json!({"operation":"string_get","keyspace":20,"key":"message"}),
            "value",
        ),
        (
            serde_json::json!({"operation":"counter_get","keyspace":21,"key":"count"}),
            "counter",
        ),
        (
            serde_json::json!({"operation":"ttl","keyspace":20,"key":"message","family":"string"}),
            "ttl",
        ),
        (
            serde_json::json!({"operation":"hash_scan","keyspace":22,"key":"hash","start_after":null,"limit":8}),
            "hash_entries",
        ),
        (
            serde_json::json!({"operation":"list_range","keyspace":23,"key":"list","start":0,"stop":-1}),
            "values",
        ),
        (
            serde_json::json!({"operation":"set_members","keyspace":24,"key":"set-a","start_after":null,"limit":8}),
            "values",
        ),
        (
            serde_json::json!({"operation":"set_algebra","keyspace":24,"operation_kind":"union","keys":["set-a","set-b"],"output_member_limit":8,"visit_limit":32}),
            "set_algebra",
        ),
        (
            serde_json::json!({"operation":"sorted_set_range","keyspace":25,"key":"ranked","start":0,"stop":-1,"order":"ascending"}),
            "sorted_set_entries",
        ),
        (
            serde_json::json!({"operation":"stream_range","keyspace":26,"key":"events","start":0,"end":u64::MAX,"limit":8}),
            "stream_entries",
        ),
    ] {
        assert_eq!(
            run(&[
                "structure",
                "--data-dir",
                &data_text,
                "read",
                "--request-json",
                &request.to_string(),
            ])?["result"]["type"],
            expected
        );
    }

    let provisioned = run(&[
        "search",
        "--data-dir",
        &data_text,
        "provision",
        "--collection",
        "13",
    ])?;
    let lexical_index = provisioned["binding"]["lexical_index"]
        .as_str()
        .ok_or("missing lexical index")?
        .to_owned();
    let ann_index = provisioned["binding"]["vectors"]
        .as_array()
        .and_then(|vectors| vectors.iter().find(|vector| vector["name"] == "ann"))
        .and_then(|vector| vector["index"].as_str())
        .ok_or("missing ANN index")?
        .to_owned();
    let documents = serde_json::json!([
        {"id":201,"text":"rust database engine","doc_values":{"category":"book","price":30},"vectors":{"exact":[0.0,0.0],"ann":[0.0,0.0]}},
        {"id":202,"text":"rust field guide","doc_values":{"category":"book","price":10},"vectors":{"exact":[1.0,0.0],"ann":[1.0,0.0]}},
        {"id":203,"text":"database hardware","doc_values":{"category":"gear","price":20},"vectors":{"exact":[2.0,0.0],"ann":[2.0,0.0]}}
    ]).to_string();
    run(&[
        "search",
        "--data-dir",
        &data_text,
        "ingest",
        "--collection",
        "13",
        "--idempotency-id",
        "1",
        "--documents-json",
        &documents,
    ])?;
    assert!(
        !run(&[
            "search",
            "--data-dir",
            &data_text,
            "query",
            "--index",
            &lexical_index,
            "--query",
            "rust",
        ])?["hits"]
            .as_array()
            .ok_or("missing lexical hits")?
            .is_empty()
    );
    let exact = run(&[
        "search",
        "--data-dir",
        &data_text,
        "integrated",
        "--collection",
        "13",
        "--vector-target",
        "exact",
        "--vector",
        "0",
        "--vector",
        "0",
        "--vector-strategy",
        "exact",
    ])?;
    assert_eq!(exact["vector_branches"][0]["strategy"], "exact_filtered");
    let ann = run(&[
        "search",
        "--data-dir",
        &data_text,
        "integrated",
        "--collection",
        "13",
        "--vector-target",
        "ann",
        "--vector",
        "0",
        "--vector",
        "0",
        "--vector-strategy",
        "ann",
    ])?;
    assert_eq!(ann["vector_branches"][0]["strategy"], "filter_aware_ann");
    let hybrid = run(&[
        "search",
        "--data-dir",
        &data_text,
        "integrated",
        "--collection",
        "13",
        "--lexical",
        "rust",
        "--vector-target",
        "exact",
        "--vector",
        "0",
        "--vector",
        "0",
        "--filter-json",
        r#"{"operation":"compare","field":"category","operator":"equal","value":"book"}"#,
        "--sort-json",
        r#"[{"source":"field","field":"price","direction":"ascending","missing":"last"}]"#,
        "--facets-json",
        r#"[{"field":"category","limit":4}]"#,
        "--metrics-json",
        r#"[{"name":"count","operation":"count"},{"name":"sum_price","operation":"sum","field":"price"}]"#,
    ])?;
    assert_eq!(hybrid["facets"][0]["buckets"][0]["count"], 2);
    assert_eq!(hybrid["aggregations"][1]["value"], "40");
    let updated = serde_json::json!({
        "id":201,
        "text":"updated rust database",
        "doc_values":{"category":"book","price":31},
        "vectors":{"exact":[0.0,0.0],"ann":[0.0,0.0]}
    })
    .to_string();
    run(&[
        "search",
        "--data-dir",
        &data_text,
        "update",
        "--collection",
        "13",
        "--idempotency-id",
        "2",
        "--document-json",
        &updated,
    ])?;
    run(&[
        "search",
        "--data-dir",
        &data_text,
        "delete",
        "--collection",
        "13",
        "--idempotency-id",
        "3",
        "--document",
        "203",
    ])?;

    let transaction = serde_json::json!([
        {"operation":"status"},
        {"operation":"stage_sql","statement":"INSERT INTO events (id, body) VALUES (?, ?)","parameters":[2,"transaction"]},
        {"operation":"stage_structure","mutation":{"operation":"string_set","keyspace":20,"key":"transaction","value":"committed","expires_at_micros":null}},
        {"operation":"stage_search","action":"index","index":lexical_index.parse::<u128>()?,"document_id":"transaction","text":"transaction search"},
        {"operation":"stage_vector","action":"upsert","index":ann_index.parse::<u128>()?,"object_id":301,"vector":[0.5,0.5]},
        {"operation":"commit"}
    ]).to_string();
    let committed = run(&[
        "transaction",
        "--data-dir",
        &data_text,
        "execute",
        "--steps-json",
        &transaction,
    ])?;
    assert_eq!(committed["steps"][0]["status"], "active");
    assert_eq!(committed["steps"][1]["status"], "active");
    assert_eq!(committed["steps"][6]["status"], "committed");
    let transaction_id = committed["steps"][6]["commit"]["transaction_id"]
        .as_str()
        .ok_or("missing transaction ID")?;
    assert_eq!(
        run(&[
            "transaction",
            "--data-dir",
            &data_text,
            "status",
            "--id",
            transaction_id,
        ])?["status"],
        "committed"
    );
    let rollback = serde_json::json!([
        {"operation":"stage_structure","mutation":{"operation":"string_set","keyspace":20,"key":"rollback","value":"discarded","expires_at_micros":null}},
        {"operation":"rollback"}
    ]).to_string();
    let rolled_back = run(&[
        "transaction",
        "--data-dir",
        &data_text,
        "execute",
        "--steps-json",
        &rollback,
    ])?;
    assert_eq!(rolled_back["steps"][2]["status"], "rolled_back");

    assert_eq!(
        run(&["status", "--data-dir", &data_text])?["status"],
        "ready"
    );
    assert!(run(&["telemetry", "--data-dir", &data_text])?["metrics"].is_array());
    assert_eq!(
        run(&["doctor", "--data-dir", &data_text])?["status"],
        "healthy"
    );
    assert_eq!(
        run(&["checkpoint", "--data-dir", &data_text])?["status"],
        "checkpointed"
    );
    assert!(matches!(
        run(&["vacuum", "--data-dir", &data_text])?["status"].as_str(),
        Some("vacuumed" | "no_changes")
    ));

    let generated = run(&[
        "proof",
        "generate",
        "--data-dir",
        &data_text,
        "--operation-json",
        r#"{"operation":"sql","statement":"SELECT id, body FROM events WHERE id = ?","parameters":[1]}"#,
        "--proof-out",
        &path(&proof),
        "--witness-out",
        &path(&witness),
    ])?;
    assert_eq!(generated["status"], "generated");
    assert_eq!(
        run(&[
            "proof",
            "verify",
            "--proof",
            &path(&proof),
            "--witness",
            &path(&witness),
            "--anchor",
            generated["anchor"].as_str().ok_or("missing anchor")?,
        ])?["semantic_reexecution_performed"],
        true
    );
    assert_eq!(
        run(&[
            "backup",
            "create",
            "--data-dir",
            &data_text,
            "--out",
            &path(&backup),
        ])?["status"],
        "created"
    );
    assert_eq!(
        run(&["backup", "verify", "--backup", &path(&backup)])?["status"],
        "verified"
    );
    assert_eq!(
        run(&[
            "restore",
            "--backup",
            &path(&backup),
            "--data-dir",
            &path(&restored),
        ])?["status"],
        "restored"
    );
    exercise_service(&restored)?;
    Ok(())
}

#[test]
fn product_error_categories_drive_stable_machine_readable_exit_classes()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let missing = path(&temporary.0.join("missing"));
    let invalid = output(&["catalog", "--data-dir", &missing, "describe", "--id", "0"])?;
    assert_eq!(invalid.status.code(), Some(2));
    let invalid_error: serde_json::Value = serde_json::from_slice(&invalid.stderr)?;
    assert_eq!(invalid_error["error"]["category"], "invalid-request");
    assert_eq!(invalid_error["exit_class"], 2);

    let data = temporary.0.join("data");
    let data_text = path(&data);
    run(&["init", "--data-dir", &data_text])?;
    let missing_object = output(&[
        "catalog",
        "--data-dir",
        &data_text,
        "describe",
        "--id",
        "999",
    ])?;
    assert_eq!(missing_object.status.code(), Some(0));
    let described: serde_json::Value = serde_json::from_slice(&missing_object.stdout)?;
    assert_eq!(described["found"], false);

    fs::create_dir(temporary.0.join("format2"))?;
    fs::write(
        temporary.0.join("format2/FORMAT"),
        b"hyphae-disk-format=2\n",
    )?;
    fs::write(temporary.0.join("format2/LOCK"), b"")?;
    let format2 = output(&["status", "--data-dir", &path(&temporary.0.join("format2"))])?;
    assert_eq!(format2.status.code(), Some(2));
    let format2_error: serde_json::Value = serde_json::from_slice(&format2.stderr)?;
    assert_eq!(format2_error["error"]["code"], "format2_directory");
    Ok(())
}

#[cfg(unix)]
#[test]
fn serve_is_the_only_command_that_binds_the_native_listener() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let endpoint = std::env::temp_dir().join(format!("hyphae-cli-{}.sock", Uuid::now_v7()));
    run(&["init", "--data-dir", &path(&data)])?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .arg("serve")
        .arg("--data-dir")
        .arg(&data)
        .arg("--endpoint")
        .arg(&endpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !endpoint.exists() && Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            let error = read_child_stderr(&mut child)?;
            return Err(std::io::Error::other(error).into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(endpoint.exists());
    let _guard = ChildGuard(&mut child);
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}

#[cfg(unix)]
#[test]
fn serve_can_share_one_product_service_with_native_http_v2() -> Result<(), Box<dyn Error>> {
    use std::io::{Read as _, Write as _};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};

    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let endpoint = std::env::temp_dir().join(format!("hyphae-cli-{}.sock", Uuid::now_v7()));
    run(&["init", "--data-dir", &path(&data)])?;
    let probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let address = probe.local_addr()?;
    drop(probe);
    let mut child = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .arg("serve")
        .arg("--data-dir")
        .arg(&data)
        .arg("--endpoint")
        .arg(&endpoint)
        .arg("--http-bind")
        .arg(address.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(_error) if Instant::now() < deadline => {
                if child.try_wait()?.is_some() {
                    let error = read_child_stderr(&mut child)?;
                    return Err(std::io::Error::other(error).into());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    };
    let _guard = ChildGuard(&mut child);
    stream.write_all(
        b"GET /v1/capabilities HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    assert!(response.starts_with("HTTP/1.1 409"));
    assert!(endpoint.exists());
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn serve_bootstrapped_directory_is_managed_with_or_without_force_flag()
-> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let owner_key = temporary.0.join("owner.key");
    let managed_endpoint = std::env::temp_dir().join(format!("hcm-{}.sock", Uuid::now_v7()));
    let legacy_endpoint = std::env::temp_dir().join(format!("hcl-{}.sock", Uuid::now_v7()));
    run(&["init", "--data-dir", &path(&data)])?;
    run(&[
        "security",
        "--data-dir",
        &path(&data),
        "bootstrap",
        "--name",
        "Owner",
        "--key-out",
        &path(&owner_key),
    ])?;
    let owner_secret = fs::read_to_string(owner_key)?;

    {
        let mut child = spawn_native_serve(&data, &managed_endpoint, &["--native-api-key-auth"])?;
        wait_for_native_endpoint(&mut child, &managed_endpoint)
            .map_err(|error| std::io::Error::other(format!("managed serve failed: {error}")))?;
        let _guard = ChildGuard(&mut child);

        wait_for_authenticated_native_ready(&managed_endpoint, &owner_secret).await?;

        let unauthenticated = HyphaeClient::local(path(&managed_endpoint))?;
        let denied = match unauthenticated
            .capabilities(RequestOptions::default())
            .await
        {
            Err(error) => error,
            Ok(_response) => return Err("managed serve accepted a legacy handshake".into()),
        };
        let ClientError::Product(denied) = denied else {
            return Err("managed serve did not return a typed authorization denial".into());
        };
        assert_eq!(denied.code(), ProductErrorCode::AuthorizationDenied);
    }

    {
        let mut child = spawn_native_serve(&data, &legacy_endpoint, &[])?;
        wait_for_native_endpoint(&mut child, &legacy_endpoint)
            .map_err(|error| std::io::Error::other(format!("legacy serve failed: {error}")))?;
        let _guard = ChildGuard(&mut child);

        wait_for_authenticated_native_ready(&legacy_endpoint, &owner_secret).await?;

        let legacy = HyphaeClient::local(path(&legacy_endpoint))?;
        let denied = match legacy.capabilities(RequestOptions::default()).await {
            Err(error) => error,
            Ok(_response) => return Err("default serve accepted a legacy handshake".into()),
        };
        let ClientError::Product(denied) = denied else {
            return Err("default serve did not return a typed authorization denial".into());
        };
        assert_eq!(denied.code(), ProductErrorCode::AuthorizationDenied);
    }
    let _ignored = fs::remove_file(managed_endpoint);
    let _ignored = fs::remove_file(legacy_endpoint);
    Ok(())
}

#[test]
fn managed_security_read_plane_is_paginated_and_secret_safe() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let owner_key = temporary.0.join("owner.key");
    run(&["init", "--data-dir", &path(&data)])?;
    assert_authorization_denied(&["security", "--data-dir", &path(&data), "status"])?;
    run(&[
        "security",
        "--data-dir",
        &path(&data),
        "bootstrap",
        "--name",
        "Owner",
        "--key-out",
        &path(&owner_key),
    ])?;
    let output = load_security_read_plane(&data, &owner_key)?;
    assert_eq!(output.status["bootstrapped"], true);
    assert_eq!(output.status["principals"], 1);
    assert_eq!(output.principals["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        output.assignments["items"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(output.keys["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        output.audit_first["items"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        output.audit_second["items"].as_array().map(Vec::len),
        Some(1)
    );
    assert_security_page_envelopes(&output);
    assert_security_item_shapes(&output);
    let owner_secret = fs::read_to_string(&owner_key)?;
    assert_security_output_is_redacted(&output, &owner_secret);
    assert_eq!(
        security_output(&data, &owner_key, &["key", "list", "--limit", "0"])?
            .status
            .code(),
        Some(2)
    );
    Ok(())
}

fn create_principal_with_replay(
    fixture: &SecurityWriteFixture,
) -> Result<(serde_json::Value, String, Vec<u8>), Box<dyn Error>> {
    let created = fixture.owner(&[
        "principal",
        "create",
        "--name",
        "Application Reader",
        "--idempotency-token",
        "101",
    ])?;
    assert_security_mutation_receipt(&created, "security.principal_create");
    let principal_id = created["result_id"]
        .as_str()
        .ok_or("principal receipt omitted its result ID")?
        .to_owned();
    let first_page = fixture.owner(&["principal", "list"])?;
    assert!(first_page["items"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["id"] == principal_id.as_str()
                && item["display_name"] == "Application Reader"
                && item["enabled"] == false
        })
    }));

    let replay = fixture.owner(&[
        "principal",
        "create",
        "--name",
        "Application Reader",
        "--idempotency-token",
        "101",
    ])?;
    assert_eq!(replay, created);
    let conflict = security_output(
        &fixture.data,
        &fixture.owner_key,
        &[
            "principal",
            "create",
            "--name",
            "Different payload",
            "--idempotency-token",
            "101",
        ],
    )?;
    assert!(!conflict.status.success());
    let conflict_error: serde_json::Value = serde_json::from_slice(&conflict.stderr)?;
    assert_eq!(conflict_error["error"]["code"], "idempotency_conflict");
    Ok((created, principal_id, conflict.stderr))
}

fn create_role_and_assign(
    fixture: &SecurityWriteFixture,
    principal_id: &str,
) -> Result<(Vec<serde_json::Value>, String), Box<dyn Error>> {
    let role = fixture.owner(&[
        "role",
        "create",
        "--name",
        "Scoped Reader",
        "--grant",
        "data.read@instance",
        "--grant",
        "search.execute@catalog_subtree:13",
        "--idempotency-token",
        "102",
    ])?;
    assert_security_mutation_receipt(&role, "security.custom_role_create");
    let role_id = role["result_id"]
        .as_str()
        .ok_or("role receipt omitted its result ID")?;
    let built_in_assignment = fixture.owner(&[
        "assignment",
        "create-built-in",
        "--principal-id",
        principal_id,
        "--role",
        "reader",
        "--scope",
        "instance",
        "--idempotency-token",
        "103",
    ])?;
    assert_security_mutation_receipt(&built_in_assignment, "security.assignment_create_built_in");
    let custom_assignment = fixture.owner(&[
        "assignment",
        "create-custom",
        "--principal-id",
        principal_id,
        "--role-id",
        role_id,
        "--idempotency-token",
        "104",
    ])?;
    assert_security_mutation_receipt(&custom_assignment, "security.assignment_create_custom");
    let custom_assignment_id = custom_assignment["result_id"]
        .as_str()
        .ok_or("assignment receipt omitted its result ID")?
        .to_owned();
    Ok((
        vec![role, built_in_assignment, custom_assignment],
        custom_assignment_id,
    ))
}

#[test]
fn managed_security_write_plane_lifecycle_is_idempotent_and_redacted() -> Result<(), Box<dyn Error>>
{
    let fixture = SecurityWriteFixture::create()?;
    let (created, principal_id, conflict_stderr) = create_principal_with_replay(&fixture)?;
    let (mut receipts, custom_assignment_id) = create_role_and_assign(&fixture, &principal_id)?;
    let enabled = fixture.owner(&[
        "principal",
        "set-enabled",
        "--principal-id",
        &principal_id,
        "--enabled",
        "true",
        "--idempotency-token",
        "105",
    ])?;
    assert_security_mutation_receipt(&enabled, "security.principal_set_enabled");
    let revoked = fixture.owner(&[
        "assignment",
        "revoke",
        "--assignment-id",
        &custom_assignment_id,
        "--idempotency-token",
        "106",
    ])?;
    assert_security_mutation_receipt(&revoked, "security.assignment_revoke");

    receipts.extend([created, enabled, revoked]);
    let rendered = receipts
        .into_iter()
        .map(|value| value.to_string())
        .collect::<String>();
    assert!(!rendered.contains(fixture.owner_secret.trim()));
    assert!(!rendered.contains("hyp1_"));
    assert!(!rendered.to_ascii_lowercase().contains("verifier"));
    assert!(!String::from_utf8_lossy(&conflict_stderr).contains(fixture.owner_secret.trim()));
    Ok(())
}

#[test]
fn security_write_plane_rejects_missing_or_weak_authority() -> Result<(), Box<dyn Error>> {
    let fixture = SecurityWriteFixture::create()?;
    let missing_token = security_output(
        &fixture.data,
        &fixture.owner_key,
        &["principal", "create", "--name", "Missing token"],
    )?;
    assert_eq!(missing_token.status.code(), Some(2));

    let zero_token = security_output(
        &fixture.data,
        &fixture.owner_key,
        &[
            "principal",
            "create",
            "--name",
            "Zero token",
            "--idempotency-token",
            "0",
        ],
    )?;
    assert_eq!(zero_token.status.code(), Some(2));

    let owner_assignment = security_output(
        &fixture.data,
        &fixture.owner_key,
        &[
            "assignment",
            "create-built-in",
            "--principal-id",
            "00000000000000000000000000000001",
            "--role",
            "owner",
            "--scope",
            "instance",
            "--idempotency-token",
            "201",
        ],
    )?;
    assert_eq!(owner_assignment.status.code(), Some(2));

    let created = fixture.owner(&[
        "principal",
        "create",
        "--name",
        "Denied Reader",
        "--idempotency-token",
        "202",
    ])?;
    let principal_id = created["result_id"]
        .as_str()
        .ok_or("principal receipt omitted its result ID")?;
    fixture.owner(&[
        "assignment",
        "create-built-in",
        "--principal-id",
        principal_id,
        "--role",
        "reader",
        "--scope",
        "instance",
        "--idempotency-token",
        "203",
    ])?;
    fixture.owner(&[
        "principal",
        "set-enabled",
        "--principal-id",
        principal_id,
        "--enabled",
        "true",
        "--idempotency-token",
        "204",
    ])?;
    let reader_key = fixture.temporary.0.join("reader.key");
    let reader_secret = fixture.issue_reader_key(principal_id, &reader_key)?;
    let denied = security_output(
        &fixture.data,
        &reader_key,
        &[
            "principal",
            "create",
            "--name",
            "Forbidden",
            "--idempotency-token",
            "205",
        ],
    )?;
    assert_eq!(denied.status.code(), Some(8));
    let denied_error: serde_json::Value = serde_json::from_slice(&denied.stderr)?;
    assert_eq!(denied_error["error"]["code"], "authorization_denied");
    for secret in [fixture.owner_secret.trim(), reader_secret.trim()] {
        assert!(!String::from_utf8_lossy(&denied.stderr).contains(secret));
        assert!(!String::from_utf8_lossy(&denied.stdout).contains(secret));
    }
    Ok(())
}

#[test]
fn security_write_plane_parsers_are_canonical_and_bounded() -> Result<(), Box<dyn Error>> {
    let fixture = SecurityWriteFixture::create()?;
    let oversized_name = "x".repeat(129);
    for operation in [
        vec![
            "principal",
            "create",
            "--name",
            oversized_name.as_str(),
            "--idempotency-token",
            "301",
        ],
        vec![
            "role",
            "create",
            "--name",
            "Invalid grant",
            "--grant",
            "ownership.manage@instance",
            "--idempotency-token",
            "302",
        ],
        vec![
            "role",
            "create",
            "--name",
            "Duplicate grant",
            "--grant",
            "data.read@instance",
            "--grant",
            "data.read@instance",
            "--idempotency-token",
            "303",
        ],
        vec![
            "assignment",
            "create-built-in",
            "--principal-id",
            "not-a-security-id",
            "--role",
            "reader",
            "--scope",
            "catalog_object:01",
            "--idempotency-token",
            "304",
        ],
        vec![
            "assignment",
            "create-built-in",
            "--principal-id",
            "00000000000000000000000000000001",
            "--role",
            "reader",
            "--scope",
            "catalog_object:01",
            "--idempotency-token",
            "305",
        ],
    ] {
        assert_eq!(
            security_output(&fixture.data, &fixture.owner_key, &operation)?
                .status
                .code(),
            Some(2)
        );
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_hyphae"));
    command
        .args(["security", "--data-dir"])
        .arg(&fixture.data)
        .arg("--native-api-key-file")
        .arg(&fixture.owner_key)
        .args(["role", "create", "--name", "Too many grants"]);
    for object_id in 1..=257 {
        command
            .arg("--grant")
            .arg(format!("data.read@catalog_object:{object_id}"));
    }
    let output = command.args(["--idempotency-token", "306"]).output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(fixture.owner_secret.trim()));
    Ok(())
}

#[test]
fn native_mcp_requires_a_restricted_api_key_source() -> Result<(), Box<dyn Error>> {
    let fixture = SecurityWriteFixture::create()?;
    let missing = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .env_remove("HYPHAE_NATIVE_API_KEY_FILE")
        .args(["mcp", "--base-url", "http://127.0.0.1:1"])
        .output()?;
    assert_eq!(missing.status.code(), Some(8));

    let accepted_file = output(&[
        "mcp",
        "--base-url",
        "http://127.0.0.1:1",
        "--native-api-key-file",
        &path(&fixture.owner_key),
    ])?;
    assert!(
        accepted_file.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted_file.stderr)
    );
    assert!(!String::from_utf8_lossy(&accepted_file.stderr).contains(fixture.owner_secret.trim()));

    let plaintext_remote = output(&[
        "mcp",
        "--base-url",
        "http://example.test",
        "--native-api-key-file",
        &path(&fixture.owner_key),
    ])?;
    assert_eq!(plaintext_remote.status.code(), Some(10));
    assert!(
        !String::from_utf8_lossy(&plaintext_remote.stderr).contains(fixture.owner_secret.trim())
    );

    let legacy = output(&[
        "mcp",
        "--base-url",
        "http://127.0.0.1:1",
        "--bearer-token-file",
        &path(&fixture.owner_key),
    ])?;
    assert_eq!(legacy.status.code(), Some(2));
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_mcp_exits_when_stdout_breaks_while_stdin_remains_open() -> Result<(), Box<dyn Error>> {
    use std::io::Write as _;

    let fixture = SecurityWriteFixture::create()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(["mcp", "--base-url", "http://127.0.0.1:1"])
        .arg("--native-api-key-file")
        .arg(&fixture.owner_key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().ok_or("missing MCP stdin")?;
    drop(child.stdout.take().ok_or("missing MCP stdout")?);
    serde_json::to_writer(
        &mut input,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}
            }
        }),
    )?;
    input.write_all(b"\n")?;
    input.flush()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            assert!(!status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill()?;
            return Err("MCP process hung with broken stdout and open stdin".into());
        }
        thread::yield_now();
    }
    drop(input);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn native_mcp_is_paginated_redacted_and_cannot_escalate_prompt_authority()
-> Result<(), Box<dyn Error>> {
    use std::io::{BufRead as _, BufReader as IoBufReader, Read as _, Write as _};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    let fixture = SecurityWriteFixture::create()?;
    let auditor_key = fixture.temporary.0.join("auditor.key");
    let principal = fixture
        .owner(&[
            "principal",
            "create",
            "--name",
            "MCP auditor",
            "--idempotency-token",
            "8101",
        ])
        .map_err(|error| std::io::Error::other(format!("principal create: {error}")))?;
    let principal_id = principal["result_id"]
        .as_str()
        .ok_or("missing principal identity")?;
    fixture
        .owner(&[
            "assignment",
            "create-built-in",
            "--principal-id",
            principal_id,
            "--role",
            "auditor",
            "--scope",
            "instance",
            "--idempotency-token",
            "8102",
        ])
        .map_err(|error| std::io::Error::other(format!("auditor assignment: {error}")))?;
    fixture
        .owner(&[
            "principal",
            "set-enabled",
            "--principal-id",
            principal_id,
            "--enabled",
            "true",
            "--idempotency-token",
            "8103",
        ])
        .map_err(|error| std::io::Error::other(format!("principal enable: {error}")))?;
    let auditor_secret = fixture
        .issue_auditor_key(principal_id, &auditor_key)
        .map_err(|error| std::io::Error::other(format!("auditor key: {error}")))?;

    let probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let address = probe.local_addr()?;
    drop(probe);
    let endpoint = std::env::temp_dir().join(format!("hmc-{}.sock", Uuid::now_v7()));
    let address_text = address.to_string();
    let mut server = spawn_native_serve(
        &fixture.data,
        &endpoint,
        &["--native-api-key-auth", "--http-bind", &address_text],
    )?;
    wait_for_authenticated_http_ready(&mut server, &address_text, &fixture.owner_secret)
        .await
        .map_err(|error| std::io::Error::other(format!("HTTP readiness: {error}")))?;
    let _server_guard = ChildGuard(&mut server);

    let messages = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"},"_meta":{"client":"host-shaped"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":{"progressToken":"host-list-1"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"cursor":"hymcpt2:100","_meta":{"progressToken":2}}}),
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"hyphae_native_capabilities","arguments":{},"_meta":{"progressToken":"host-call-1"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"hyphae_native_security_status","arguments":{}}}),
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"hyphae_native_security_principals","arguments":{"limit":1}}}),
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"hyphae_native_security_status","arguments":{"role":"owner","api_key":auditor_secret.trim()}}}),
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/list","params":{"cursor":"hymcpt2:1"}}),
    ];
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(["mcp", "--base-url", &format!("http://{address_text}")])
        .arg("--native-api-key-file")
        .arg(&auditor_key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = mcp.stdin.take().ok_or("missing MCP stdin")?;
    let output = mcp.stdout.take().ok_or("missing MCP stdout")?;
    let mut output = IoBufReader::new(output);
    let mut observed = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut input, &message)?;
        input.write_all(b"\n")?;
        input.flush()?;
        if message.get("id").is_some() {
            let mut line = String::new();
            if output.read_line(&mut line)? == 0 {
                return Err("MCP stdout closed before its response barrier".into());
            }
            observed.push(line);
        }
    }
    drop(input);
    assert!(mcp.wait()?.success());
    let stdout = observed.concat();
    let mut stderr = String::new();
    mcp.stderr
        .take()
        .ok_or("missing MCP stderr")?
        .read_to_string(&mut stderr)?;
    assert!(!stdout.contains(auditor_secret.trim()));
    assert!(!stderr.contains(auditor_secret.trim()));
    assert_mcp_read_only_session(&stdout)?;
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn native_mcp_cancels_in_flight_http_rejects_saturation_and_recovers() -> Result<(), Box<dyn Error>>
{
    // Shared CI runners can starve the MCP child for several seconds; the
    // saturation contract is about cancellation, not wall-clock budgets.
    const CONTENDED: Duration = Duration::from_secs(10);

    use std::io::{BufRead as _, BufReader as IoBufReader, Write as _};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
    use std::sync::mpsc;

    let fixture = SecurityWriteFixture::create()?;
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    let (accepted_sender, accepted_receiver) = mpsc::channel();
    let server = thread::spawn(move || -> Result<(), std::io::Error> {
        let (mut connection, _) = listener.accept()?;
        let reader_connection = connection.try_clone()?;
        let mut reader = IoBufReader::new(reader_connection);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                break;
            }
        }
        accepted_sender
            .send(())
            .map_err(|_| std::io::Error::other("test receiver dropped"))?;
        let mut release = String::new();
        reader.read_line(&mut release)?;
        connection.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")?;
        Ok(())
    });

    let mut mcp = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(["mcp", "--base-url", &format!("http://{address}")])
        .arg("--native-api-key-file")
        .arg(&fixture.owner_key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = mcp.stdin.take().ok_or("missing MCP stdin")?;
    let output = mcp.stdout.take().ok_or("missing MCP stdout")?;
    let (line_sender, line_receiver) = mpsc::channel();
    let output_reader = thread::spawn(move || {
        for line in IoBufReader::new(output).lines() {
            if line_sender.send(line).is_err() {
                break;
            }
        }
    });
    let messages = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hyphae_native_capabilities","arguments":{}}}),
    ];
    for message in messages {
        serde_json::to_writer(&mut input, &message)?;
        input.write_all(b"\n")?;
        input.flush()?;
    }
    assert!(
        serde_json::from_str::<serde_json::Value>(
            &line_receiver.recv_timeout(CONTENDED)??
        )?["result"]
            .is_object()
    );
    accepted_receiver.recv_timeout(CONTENDED)?;
    for message in [
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"hyphae_native_capabilities","arguments":{}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"test"}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}),
    ] {
        serde_json::to_writer(&mut input, &message)?;
        input.write_all(b"\n")?;
        input.flush()?;
    }
    let mut responses = Vec::new();
    while responses.len() < 2 {
        responses.push(serde_json::from_str::<serde_json::Value>(
            &line_receiver.recv_timeout(CONTENDED)??,
        )?);
    }
    assert!(
        responses
            .iter()
            .any(|response| { response["id"] == 3 && response["error"]["code"] == -32001 })
    );
    assert!(responses.iter().any(|response| {
        response["id"] == 2
            && response["result"]["structuredContent"]["error"]["code"] == "cancelled"
    }));
    input.write_all(b"\r\n")?;
    input.flush()?;
    serde_json::to_writer(
        &mut input,
        &serde_json::json!({"jsonrpc":"2.0","id":4,"method":"ping","params":{}}),
    )?;
    input.write_all(b"\n")?;
    input.flush()?;
    let recovered =
        serde_json::from_str::<serde_json::Value>(&line_receiver.recv_timeout(CONTENDED)??)?;
    assert_eq!(recovered["id"], 4);
    assert_eq!(recovered["result"], serde_json::json!({}));
    drop(input);
    assert!(mcp.wait()?.success());
    output_reader
        .join()
        .map_err(|_| "MCP stdout reader panicked")?;
    server.join().map_err(|_| "HTTP test server panicked")??;
    Ok(())
}

#[test]
fn bootstrapped_embedded_cli_requires_a_restricted_api_key_source() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let owner_key = temporary.0.join("owner.key");
    let wrong_key = temporary.0.join("wrong.key");
    run(&["init", "--data-dir", &path(&data)])?;
    run(&[
        "security",
        "--data-dir",
        &path(&data),
        "bootstrap",
        "--name",
        "Owner",
        "--key-out",
        &path(&owner_key),
    ])?;

    assert_authorization_denied(&["capabilities", "--data-dir", &path(&data)])?;
    assert_authorization_denied(&["doctor", "--data-dir", &path(&data)])?;
    assert_authorization_denied(&["security", "--data-dir", &path(&data), "status"])?;

    let capabilities = run(&[
        "capabilities",
        "--data-dir",
        &path(&data),
        "--native-api-key-file",
        &path(&owner_key),
    ])?;
    assert_eq!(capabilities["product_api_version"], 1);
    let doctor = run(&[
        "doctor",
        "--data-dir",
        &path(&data),
        "--native-api-key-file",
        &path(&owner_key),
    ])?;
    assert_eq!(doctor["status"], "healthy");

    let stdin_capabilities = run_with_api_key_stdin(&data, &fs::read(&owner_key)?)?;
    assert_eq!(stdin_capabilities["product_api_version"], 1);

    #[cfg(windows)]
    hyphae_native_product::validate_windows_restricted_file(&fs::File::open(&owner_key)?)?;

    let wrong_secret = format!("hyp1_{}_{}", "0".repeat(32), "0".repeat(64));
    fs::write(&wrong_key, &wrong_secret)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&wrong_key, fs::Permissions::from_mode(0o600))?;
    }
    let wrong = assert_authorization_denied(&[
        "capabilities",
        "--data-dir",
        &path(&data),
        "--native-api-key-file",
        &path(&wrong_key),
    ])?;
    assert!(!String::from_utf8_lossy(&wrong.stderr).contains(&wrong_secret));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let insecure_key = temporary.0.join("insecure.key");
        fs::copy(&owner_key, &insecure_key)?;
        fs::set_permissions(&insecure_key, fs::Permissions::from_mode(0o644))?;
        let insecure = output(&[
            "capabilities",
            "--data-dir",
            &path(&data),
            "--native-api-key-file",
            &path(&insecure_key),
        ])?;
        assert_eq!(insecure.status.code(), Some(8));

        let linked_key = temporary.0.join("linked.key");
        symlink(&owner_key, &linked_key)?;
        let linked = output(&[
            "capabilities",
            "--data-dir",
            &path(&data),
            "--native-api-key-file",
            &path(&linked_key),
        ])?;
        assert_eq!(linked.status.code(), Some(8));
    }
    Ok(())
}

#[cfg(unix)]
fn spawn_native_serve(
    data: &Path,
    endpoint: &Path,
    extra_arguments: &[&str],
) -> Result<Child, Box<dyn Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .arg("serve")
        .arg("--data-dir")
        .arg(data)
        .arg("--endpoint")
        .arg(endpoint)
        .args(extra_arguments)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?)
}

#[cfg(unix)]
fn wait_for_native_endpoint(child: &mut Child, endpoint: &Path) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !endpoint.exists() && Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            let error = read_child_stderr(child)?;
            return Err(std::io::Error::other(error).into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !endpoint.exists() {
        return Err("serve did not bind the requested native endpoint".into());
    }
    Ok(())
}

#[cfg(unix)]
async fn wait_for_authenticated_native_ready(
    endpoint: &Path,
    owner_secret: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let client = HyphaeClient::local_authenticated(path(endpoint), owner_secret)?;
        match client.capabilities(RequestOptions::default()).await {
            Ok(ProductResponse::Capabilities(_)) => return Ok(()),
            Ok(_) => return Err("managed serve returned an unexpected readiness response".into()),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(_) => return Err("managed serve did not become ready".into()),
        }
    }
}

#[cfg(unix)]
async fn wait_for_authenticated_http_ready(
    child: &mut Child,
    address: &str,
    owner_secret: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let base_url = format!("http://{address}");
        let transport = HttpTransport::new(&base_url)?.bearer_token(owner_secret.trim())?;
        let client = HyphaeClient::new(transport);
        match client.capabilities(RequestOptions::default()).await {
            Ok(ProductResponse::Capabilities(_)) => return Ok(()),
            Ok(_) => return Err("managed HTTP returned an unexpected readiness response".into()),
            Err(_) if Instant::now() < deadline => {
                if child.try_wait()?.is_some() {
                    let error = read_child_stderr(child)?;
                    return Err(std::io::Error::other(error).into());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(_) => return Err("managed HTTP did not become ready".into()),
        }
    }
}

#[cfg(unix)]
fn assert_mcp_read_only_session(stdout: &str) -> Result<(), Box<dyn Error>> {
    let responses = stdout
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(responses.len(), 8);
    let schema_version = &responses[0]["result"]["_meta"]["hyphaeToolSchemaVersion"];
    let schema_digest = &responses[0]["result"]["_meta"]["hyphaeToolSchemaDigest"];
    assert_eq!(schema_version, "hyphae-native-mcp-tools-v4");
    assert_eq!(schema_digest.as_str().map(str::len), Some(64));
    assert_eq!(
        responses[1]["result"]["tools"].as_array().map(Vec::len),
        Some(8)
    );
    assert!(responses[1]["result"].get("nextCursor").is_none());
    assert_eq!(responses[2]["error"]["code"], -32602);
    for tool in responses[1]["result"]["tools"]
        .as_array()
        .ok_or("missing host-shaped MCP tools")?
    {
        assert_eq!(tool["outputSchema"]["type"], "object");
        assert_eq!(
            tool["outputSchema"]["oneOf"].as_array().map(Vec::len),
            Some(2)
        );
    }
    assert_eq!(responses[3]["result"]["isError"], false);
    assert_eq!(
        responses[3]["result"]["structuredContent"]["product_api_version"],
        1
    );
    assert_eq!(responses[4]["result"]["isError"], false);
    assert_eq!(
        responses[4]["result"]["structuredContent"]["schema"],
        "hyphae-native-access-control-status-v1"
    );
    assert_eq!(responses[5]["result"]["isError"], false);
    assert_eq!(
        responses[5]["result"]["structuredContent"]["schema"],
        "hyphae-native-security-principals-v1"
    );
    assert_eq!(responses[6]["result"]["isError"], false);
    assert_eq!(
        responses[6]["result"]["structuredContent"]["error"]["code"],
        "invalid_request"
    );
    assert_json_keys(
        &responses[6]["result"]["structuredContent"],
        &["schema", "error"],
    );
    assert_json_keys(
        &responses[6]["result"]["structuredContent"]["error"],
        &[
            "code",
            "category",
            "message",
            "retry",
            "transaction_state",
            "request_id",
            "trace_id",
            "object_id",
            "transaction_id",
        ],
    );
    assert_eq!(responses[7]["error"]["code"], -32602);
    for response in std::iter::once(&responses[1]).chain(responses[3..7].iter()) {
        assert_eq!(
            response["result"]["_meta"]["hyphaeToolSchemaVersion"],
            *schema_version
        );
        assert_eq!(
            response["result"]["_meta"]["hyphaeToolSchemaDigest"],
            *schema_digest
        );
    }
    for response in &responses[3..7] {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .ok_or("missing MCP text content")?;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(text)?,
            response["result"]["structuredContent"]
        );
    }
    for tool in responses[1]["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
    {
        assert_eq!(
            tool["outputSchema"]["oneOf"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert_eq!(tool["annotations"]["idempotentHint"], true);
        assert_eq!(tool["annotations"]["openWorldHint"], false);
    }
    Ok(())
}

struct ChildGuard<'a>(&'a mut Child);

impl Drop for ChildGuard<'_> {
    fn drop(&mut self) {
        let _ignored = self.0.kill();
        let _ignored = self.0.wait();
    }
}

fn read_child_stderr(child: &mut Child) -> Result<String, std::io::Error> {
    use std::io::Read as _;

    let mut error = String::new();
    if let Some(stderr) = child.stderr.as_mut() {
        stderr.read_to_string(&mut error)?;
    }
    Ok(error)
}

fn encode_hex(bytes: &[u8]) -> String {
    let hex = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(hex[usize::from(byte >> 4)]));
        encoded.push(char::from(hex[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Builds one minimal legacy-encoded Valkey/Redis RDB v11 payload without a
/// trailer checksum: two databases, strings with and without expiry, one
/// hash, one intset set, one list, and one sorted set.
fn valkey_rdb_sample() -> Vec<u8> {
    fn string(payload: &mut Vec<u8>, bytes: &[u8]) {
        assert!(bytes.len() < 64);
        payload.push(u8::try_from(bytes.len()).unwrap_or(0));
        payload.extend_from_slice(bytes);
    }
    let mut payload = b"REDIS0011".to_vec();
    payload.push(0xFA);
    string(&mut payload, b"redis-ver");
    string(&mut payload, b"7.2.5");
    payload.push(0xFE);
    payload.push(0);
    payload.push(0);
    string(&mut payload, b"greeting");
    string(&mut payload, b"hola");
    payload.push(0xFC);
    payload.extend_from_slice(&4_102_444_800_000_u64.to_le_bytes());
    payload.push(0);
    string(&mut payload, b"session");
    string(&mut payload, b"active");
    payload.push(4);
    string(&mut payload, b"note:1");
    payload.push(2);
    string(&mut payload, b"author");
    string(&mut payload, b"mario");
    string(&mut payload, b"state");
    string(&mut payload, b"published");
    payload.push(11);
    string(&mut payload, b"codes");
    let mut intset = Vec::new();
    intset.extend_from_slice(&4_u32.to_le_bytes());
    intset.extend_from_slice(&2_u32.to_le_bytes());
    intset.extend_from_slice(&7_u32.to_le_bytes());
    intset.extend_from_slice(&11_u32.to_le_bytes());
    string(&mut payload, &intset);
    payload.push(1);
    string(&mut payload, b"queue");
    payload.push(3);
    string(&mut payload, b"first");
    string(&mut payload, b"second");
    string(&mut payload, b"third");
    payload.push(3);
    string(&mut payload, b"ranking");
    payload.push(1);
    string(&mut payload, b"note:1");
    string(&mut payload, b"9.5");
    payload.push(0xFE);
    payload.push(1);
    payload.push(0);
    string(&mut payload, b"other");
    string(&mut payload, b"db");
    payload.push(0xFF);
    payload.extend_from_slice(&[0_u8; 8]);
    payload
}

#[test]
fn valkey_migration_runs_verifies_promotes_and_reads_back() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let source = temporary.0.join("dump.rdb");
    let target = temporary.0.join("target");
    let manifest = temporary.0.join("receipt.json");
    fs::write(&source, valkey_rdb_sample())?;

    let inspected = run(&[
        "migrate",
        "inspect",
        "--source",
        &path(&source),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert_eq!(inspected["status"], "inspected");
    assert_eq!(inspected["key_count"], 7);
    assert_eq!(inspected["database_count"], 2);
    assert_eq!(
        inspected["unwaived_constructs"],
        serde_json::json!(["checksum-absent"])
    );

    let imported = run(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
        "--waive",
        "checksum-absent",
    ])?;
    assert_eq!(imported["status"], "imported");
    assert_eq!(imported["imported_keys"], 7);
    assert_eq!(imported["skipped_expired"], 0);
    assert!(target.join("FORMAT.pending").exists());
    let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    assert_eq!(receipt["kind"], "hyphae-external-migration-receipt");
    assert_eq!(receipt["source"]["kind"], "valkey-rdb");
    assert_eq!(receipt["waivers"][0]["construct"], "checksum-absent");

    let verified = run(&[
        "migrate",
        "verify",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert_eq!(verified["status"], "verified");
    assert_eq!(verified["pending"], true);

    let promoted = run(&[
        "migrate",
        "promote",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert_eq!(promoted["status"], "promoted");
    assert!(target.join("FORMAT").exists());
    assert!(!target.join("FORMAT.pending").exists());

    let reopened = run(&["status", "--data-dir", &path(&target)])?;
    assert_eq!(reopened["status"], "ready");
    let catalog = run(&["catalog", "--data-dir", &path(&target), "list"])?;
    let rendered = catalog.to_string();
    assert!(rendered.contains("valkey_db0_strings"));
    assert!(rendered.contains("valkey_db1_strings"));
    assert!(rendered.contains("valkey_db0_hashes"));

    let verified_promoted = run(&[
        "migrate",
        "verify",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert_eq!(verified_promoted["pending"], false);
    Ok(())
}

#[test]
fn valkey_migration_fails_closed_without_the_required_waiver() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let source = temporary.0.join("dump.rdb");
    let target = temporary.0.join("target");
    let manifest = temporary.0.join("receipt.json");
    fs::write(&source, valkey_rdb_sample())?;

    let rejected = output(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert!(!rejected.status.success());
    assert_eq!(rejected.status.code(), Some(2));
    assert!(!target.exists());
    assert!(!manifest.exists());

    let unknown_waiver = output(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
        "--waive",
        "checksum-absent",
        "--waive",
        "nonexistent-construct",
    ])?;
    assert!(!unknown_waiver.status.success());
    assert!(!target.exists());

    let format2_waiver = output(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--waive",
        "checksum-absent",
    ])?;
    assert!(!format2_waiver.status.success());
    assert!(!target.exists());
    Ok(())
}

#[test]
fn valkey_migration_detects_receipt_and_target_tampering() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let source = temporary.0.join("dump.rdb");
    fs::write(&source, valkey_rdb_sample())?;
    let target = temporary.0.join("target");
    let manifest = temporary.0.join("receipt.json");
    run(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
        "--waive",
        "checksum-absent",
    ])?;

    // A tampered receipt fails its sealed digest validation.
    let mut receipt: serde_json::Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    receipt["source"]["key_count"] = serde_json::json!(99);
    let tampered_manifest = temporary.0.join("tampered.json");
    fs::write(&tampered_manifest, serde_json::to_vec(&receipt)?)?;
    let tampered = output(&[
        "migrate",
        "verify",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&tampered_manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert!(!tampered.status.success());

    // A receipt bound to a different target directory fails identity checks.
    let second_target = temporary.0.join("second-target");
    let second_manifest = temporary.0.join("second-receipt.json");
    run(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&second_target),
        "--manifest",
        &path(&second_manifest),
        "--source-kind",
        "valkey-rdb",
        "--waive",
        "checksum-absent",
    ])?;
    let swapped = output(&[
        "migrate",
        "verify",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&second_manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert!(!swapped.status.success());

    // A source that differs from the receipt fails identity checks.
    let mut altered = valkey_rdb_sample();
    let position = altered
        .windows(4)
        .position(|window| window == b"hola")
        .ok_or("sample value missing")?;
    altered[position] = b'H';
    let altered_source = temporary.0.join("altered.rdb");
    fs::write(&altered_source, altered)?;
    let altered_verify = output(&[
        "migrate",
        "verify",
        "--source",
        &path(&altered_source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert!(!altered_verify.status.success());
    Ok(())
}

#[test]
fn valkey_migration_rollback_removes_only_the_pending_target() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let source = temporary.0.join("dump.rdb");
    fs::write(&source, valkey_rdb_sample())?;
    let target = temporary.0.join("target");
    let manifest = temporary.0.join("receipt.json");
    run(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
        "--waive",
        "checksum-absent",
    ])?;
    assert!(target.join("FORMAT.pending").exists());

    let rolled_back = run(&["migrate", "rollback", "--target", &path(&target)])?;
    assert_eq!(rolled_back["status"], "rolled_back");
    assert!(!target.exists());
    assert!(source.exists());
    assert!(manifest.exists());
    Ok(())
}

#[test]
fn valkey_migration_rejects_path_overlap() -> Result<(), Box<dyn Error>> {
    let temporary = TestDirectory::new()?;
    let source_directory = temporary.0.join("sources");
    fs::create_dir_all(&source_directory)?;
    let source = source_directory.join("dump.rdb");
    fs::write(&source, valkey_rdb_sample())?;

    let inside = output(&[
        "migrate",
        "run",
        "--source",
        &path(&source_directory),
        "--target",
        &path(&source_directory.join("target")),
        "--manifest",
        &path(&source_directory.join("receipt.json")),
        "--source-kind",
        "valkey-rdb",
        "--waive",
        "checksum-absent",
    ])?;
    assert!(!inside.status.success());
    assert!(!source_directory.join("target").exists());
    assert!(!source_directory.join("receipt.json").exists());
    Ok(())
}

#[test]
fn valkey_fixture_runs_the_complete_cycle_with_stream_waivers() -> Result<(), Box<dyn Error>> {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/valkey-rdb-v11.json"))?;
    let rdb_hex = fixture["rdb_hex"]
        .as_str()
        .ok_or("fixture rdb_hex missing")?;
    let mut bytes = Vec::with_capacity(rdb_hex.len() / 2);
    for pair in rdb_hex.as_bytes().chunks(2) {
        bytes.push(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?);
    }

    let temporary = TestDirectory::new()?;
    let source = temporary.0.join("dump.rdb");
    let target = temporary.0.join("target");
    let manifest = temporary.0.join("receipt.json");
    fs::write(&source, &bytes)?;

    let inspected = run(&[
        "migrate",
        "inspect",
        "--source",
        &path(&source),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert_eq!(inspected["key_count"], fixture["expected"]["key_count"]);
    assert_eq!(
        inspected["database_count"],
        fixture["expected"]["database_count"]
    );
    assert_eq!(
        inspected["unwaived_constructs"],
        fixture["expected"]["required_waivers"]
    );
    assert_eq!(
        inspected["source_digest"].as_str(),
        fixture["blake3_hex"].as_str()
    );

    let imported = run(&[
        "migrate",
        "run",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
        "--waive",
        "streams",
        "--waive",
        "stream-consumer-groups",
    ])?;
    assert_eq!(imported["status"], "imported");
    assert_eq!(imported["imported_keys"], fixture["expected"]["key_count"]);

    let verified = run(&[
        "migrate",
        "verify",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert_eq!(verified["status"], "verified");

    let promoted = run(&[
        "migrate",
        "promote",
        "--source",
        &path(&source),
        "--target",
        &path(&target),
        "--manifest",
        &path(&manifest),
        "--source-kind",
        "valkey-rdb",
    ])?;
    assert_eq!(promoted["status"], "promoted");
    let reopened = run(&["status", "--data-dir", &path(&target)])?;
    assert_eq!(reopened["status"], "ready");
    Ok(())
}

/// Drives one authenticated MCP stdio session and returns one response line
/// per request identifier.
#[cfg(unix)]
fn run_mcp_session(
    address: &str,
    key_file: &Path,
    messages: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>, Box<dyn Error>> {
    use std::io::{BufRead as _, BufReader as IoBufReader, Write as _};
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(["mcp", "--base-url", &format!("http://{address}")])
        .arg("--native-api-key-file")
        .arg(key_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = mcp.stdin.take().ok_or("missing MCP stdin")?;
    let output = mcp.stdout.take().ok_or("missing MCP stdout")?;
    let mut output = IoBufReader::new(output);
    let mut responses = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut input, message)?;
        input.write_all(b"\n")?;
        input.flush()?;
        if message.get("id").is_some() {
            let mut line = String::new();
            if output.read_line(&mut line)? == 0 {
                return Err("MCP stdout closed before its response barrier".into());
            }
            responses.push(serde_json::from_str(&line)?);
        }
    }
    drop(input);
    assert!(mcp.wait()?.success());
    Ok(responses)
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn native_mcp_search_tools_execute_with_authority_and_fail_closed_without()
-> Result<(), Box<dyn Error>> {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    // Provision the collection while the directory is still unmanaged.
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run_isolated(&["init", "--data-dir", &data_text])?;
    run_isolated(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.notes",
    ])?;
    let provisioned = run_isolated(&[
        "search",
        "--data-dir",
        &data_text,
        "provision",
        "--collection",
        "13",
    ])?;
    let lexical_index = provisioned["binding"]["lexical_index"]
        .as_str()
        .ok_or("missing lexical index")?
        .parse::<u64>()?;
    let documents = serde_json::json!([
        {"id":301,"text":"rust database engine","doc_values":{"category":"book"},"vectors":{"exact":[0.0,0.0],"ann":[0.0,0.0]}},
        {"id":302,"text":"rust field guide","doc_values":{"category":"book"},"vectors":{"exact":[1.0,0.0],"ann":[1.0,0.0]}},
        {"id":303,"text":"database hardware","doc_values":{"category":"gear"},"vectors":{"exact":[2.0,0.0],"ann":[2.0,0.0]}}
    ])
    .to_string();
    run_isolated(&[
        "search",
        "--data-dir",
        &data_text,
        "ingest",
        "--collection",
        "13",
        "--idempotency-id",
        "1",
        "--documents-json",
        &documents,
    ])?;

    // Bootstrap security and issue one Reader key and one Auditor key.
    let owner_key = temporary.0.join("owner.key");
    run_isolated(&[
        "security",
        "--data-dir",
        &data_text,
        "bootstrap",
        "--name",
        "Owner",
        "--key-out",
        &path(&owner_key),
    ])?;
    let fixture = SecurityWriteFixture {
        temporary,
        data: data.clone(),
        owner_key: owner_key.clone(),
        owner_secret: fs::read_to_string(&owner_key)?,
    };
    let mut keys = Vec::new();
    for (name, role, token_base) in [
        ("Search reader", "reader", 8600),
        ("Search auditor", "auditor", 8700),
    ] {
        let principal = fixture.owner(&[
            "principal",
            "create",
            "--name",
            name,
            "--idempotency-token",
            &token_base.to_string(),
        ])?;
        let principal_id = principal["result_id"]
            .as_str()
            .ok_or("missing principal identity")?
            .to_owned();
        fixture.owner(&[
            "assignment",
            "create-built-in",
            "--principal-id",
            &principal_id,
            "--role",
            role,
            "--scope",
            "instance",
            "--idempotency-token",
            &(token_base + 1).to_string(),
        ])?;
        fixture.owner(&[
            "principal",
            "set-enabled",
            "--principal-id",
            &principal_id,
            "--enabled",
            "true",
            "--idempotency-token",
            &(token_base + 2).to_string(),
        ])?;
        let destination = fixture.temporary.0.join(format!("{role}.key"));
        if role == "reader" {
            fixture.issue_reader_key(&principal_id, &destination)?;
        } else {
            fixture.issue_auditor_key(&principal_id, &destination)?;
        }
        keys.push(destination);
    }
    let (reader_key, auditor_key) = (keys.remove(0), keys.remove(0));

    let probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let address = probe.local_addr()?;
    drop(probe);
    let endpoint = std::env::temp_dir().join(format!("hms-{}.sock", Uuid::now_v7()));
    let address_text = address.to_string();
    let mut server = spawn_native_serve(
        &data,
        &endpoint,
        &["--native-api-key-auth", "--http-bind", &address_text],
    )?;
    wait_for_authenticated_http_ready(&mut server, &address_text, &fixture.owner_secret)
        .await
        .map_err(|error| std::io::Error::other(format!("HTTP readiness: {error}")))?;
    let _server_guard = ChildGuard(&mut server);

    let handshake = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    ];
    let lexical_call = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
        "name":"hyphae_native_search_lexical",
        "arguments":{"index":lexical_index,"kind":"term","query":"rust","limit":10}}});
    let collection_call = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "name":"hyphae_native_search_collection",
        "arguments":{
            "collection":13,
            "lexical":{"query":"rust","candidate_limit":10},
            "vectors":[{"target":"exact","values":[0.0,0.0],"candidate_limit":10}],
            "filter":{"operation":"compare","field":"category","operator":"equal","value":"book"},
            "facets":[{"field":"category","limit":4}],
            "limit":10}}});
    let phrase_call = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
        "name":"hyphae_native_search_collection",
        "arguments":{
            "collection":13,
            "lexical":{"query":"rust database","candidate_limit":10,"phrase":true},
            "limit":10}}});

    // The Reader authority executes both search tools.
    let mut messages = handshake.to_vec();
    messages.push(lexical_call.clone());
    messages.push(collection_call.clone());
    messages.push(phrase_call);
    let reader = run_mcp_session(&address_text, &reader_key, &messages)?;
    assert_eq!(reader[1]["result"]["isError"], false);
    assert!(
        !reader[1]["result"]["structuredContent"]["hits"]
            .as_array()
            .ok_or("missing lexical hits")?
            .is_empty()
    );
    assert_eq!(reader[2]["result"]["isError"], false);
    let integrated = &reader[2]["result"]["structuredContent"];
    assert_eq!(integrated["hits"].as_array().map(Vec::len), Some(2));
    assert_eq!(integrated["facets"][0]["buckets"][0]["count"], 2);
    assert_eq!(
        integrated["vector_branches"][0]["strategy"],
        "exact_filtered"
    );
    assert_eq!(integrated["approximate"], false);
    // The phrase mode travels through the MCP surface end to end.
    assert_eq!(reader[3]["result"]["isError"], false);
    assert!(
        reader[3]["result"]["structuredContent"]["hits"]
            .as_array()
            .is_some()
    );

    // The Auditor authority lacks search.execute and fails closed.
    let mut messages = handshake.to_vec();
    messages.push(lexical_call);
    messages.push(collection_call);
    let auditor = run_mcp_session(&address_text, &auditor_key, &messages)?;
    for response in &auditor[1..=2] {
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"],
            "authorization_denied"
        );
    }
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}

#[cfg(unix)]
fn run_mcp_session_with_flags(
    address: &str,
    key_file: &Path,
    extra: &[&str],
    messages: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>, Box<dyn Error>> {
    use std::io::{BufRead as _, BufReader as IoBufReader, Write as _};
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .args(["mcp", "--base-url", &format!("http://{address}")])
        .arg("--native-api-key-file")
        .arg(key_file)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = mcp.stdin.take().ok_or("missing MCP stdin")?;
    let output = mcp.stdout.take().ok_or("missing MCP stdout")?;
    let mut output = IoBufReader::new(output);
    let mut responses = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut input, message)?;
        input.write_all(b"\n")?;
        input.flush()?;
        if message.get("id").is_some() {
            let mut line = String::new();
            if output.read_line(&mut line)? == 0 {
                return Err("MCP stdout closed before its response barrier".into());
            }
            responses.push(serde_json::from_str(&line)?);
        }
    }
    drop(input);
    assert!(mcp.wait()?.success());
    Ok(responses)
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn native_mcp_ingest_is_opt_in_write_scoped_and_fail_closed() -> Result<(), Box<dyn Error>> {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run_isolated(&["init", "--data-dir", &data_text])?;
    run_isolated(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.notes",
    ])?;
    run_isolated(&[
        "search",
        "--data-dir",
        &data_text,
        "provision",
        "--collection",
        "13",
    ])?;

    let owner_key = temporary.0.join("owner.key");
    run_isolated(&[
        "security",
        "--data-dir",
        &data_text,
        "bootstrap",
        "--name",
        "Owner",
        "--key-out",
        &path(&owner_key),
    ])?;
    let fixture = SecurityWriteFixture {
        temporary,
        data: data.clone(),
        owner_key: owner_key.clone(),
        owner_secret: fs::read_to_string(&owner_key)?,
    };
    let mut keys = Vec::new();
    for (name, role, token_base) in [
        ("Ingest writer", "writer", 8800),
        ("Ingest reader", "reader", 8900),
    ] {
        let principal = fixture.owner(&[
            "principal",
            "create",
            "--name",
            name,
            "--idempotency-token",
            &token_base.to_string(),
        ])?;
        let principal_id = principal["result_id"]
            .as_str()
            .ok_or("missing principal identity")?
            .to_owned();
        fixture.owner(&[
            "assignment",
            "create-built-in",
            "--principal-id",
            &principal_id,
            "--role",
            role,
            "--scope",
            "instance",
            "--idempotency-token",
            &(token_base + 1).to_string(),
        ])?;
        fixture.owner(&[
            "principal",
            "set-enabled",
            "--principal-id",
            &principal_id,
            "--enabled",
            "true",
            "--idempotency-token",
            &(token_base + 2).to_string(),
        ])?;
        let destination = fixture.temporary.0.join(format!("{role}.key"));
        if role == "writer" {
            fixture.issue_built_in_key(
                &principal_id,
                &destination,
                BuiltInRole::Writer,
                "writer-mcp",
            )?;
        } else {
            fixture.issue_reader_key(&principal_id, &destination)?;
        }
        keys.push(destination);
    }
    let (writer_key, reader_key) = (keys.remove(0), keys.remove(0));

    let probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let address = probe.local_addr()?;
    drop(probe);
    let endpoint = std::env::temp_dir().join(format!("hmi-{}.sock", Uuid::now_v7()));
    let address_text = address.to_string();
    let mut server = spawn_native_serve(
        &data,
        &endpoint,
        &["--native-api-key-auth", "--http-bind", &address_text],
    )?;
    wait_for_authenticated_http_ready(&mut server, &address_text, &fixture.owner_secret)
        .await
        .map_err(|error| std::io::Error::other(format!("HTTP readiness: {error}")))?;
    let _server_guard = ChildGuard(&mut server);

    let handshake = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    ];
    let list = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}});
    let ingest_call = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "name":"hyphae_native_search_ingest",
        "arguments":{
            "collection":13,
            "idempotency_id":41,
            "documents":[{"id":501,"text":"rust ingest via mcp","vectors":{"exact":[0.5,0.5],"ann":[0.5,0.5]}}]}}});
    let search_call = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
        "name":"hyphae_native_search_collection",
        "arguments":{"collection":13,"lexical":{"query":"ingest"},"limit":5}}});

    // Default session: the ingest tool is not listed and not callable.
    let mut messages = handshake.to_vec();
    messages.push(list.clone());
    messages.push(ingest_call.clone());
    let default_session = run_mcp_session_with_flags(&address_text, &writer_key, &[], &messages)?;
    assert_eq!(
        default_session[1]["result"]["tools"]
            .as_array()
            .map(Vec::len),
        Some(8)
    );
    assert_eq!(default_session[2]["error"]["code"], -32602);

    // Opted-in session with a Writer key: the tool is listed and the batch
    // commits; the ingested document is immediately searchable.
    let mut messages = handshake.to_vec();
    messages.push(list.clone());
    messages.push(ingest_call.clone());
    messages.push(ingest_call.clone());
    messages.push(search_call);
    let writer =
        run_mcp_session_with_flags(&address_text, &writer_key, &["--allow-ingest"], &messages)?;
    assert_eq!(
        writer[1]["result"]["tools"].as_array().map(Vec::len),
        Some(11)
    );
    assert_eq!(writer[2]["result"]["isError"], false);
    assert_eq!(
        writer[2]["result"]["structuredContent"]["status"],
        "committed"
    );
    assert_eq!(
        writer[3]["result"]["structuredContent"]["status"],
        "existing"
    );
    assert_eq!(
        writer[3]["result"]["structuredContent"]["idempotent_replay"],
        true
    );
    assert_eq!(
        writer[4]["result"]["structuredContent"]["hits"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // Opted-in session with a Reader key: exposure is not authority.
    let mut messages = handshake.to_vec();
    messages.push(ingest_call);
    let reader =
        run_mcp_session_with_flags(&address_text, &reader_key, &["--allow-ingest"], &messages)?;
    assert_eq!(
        reader[1]["result"]["structuredContent"]["error"]["code"],
        "authorization_denied"
    );
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}

#[cfg(unix)]
#[test]
fn agent_lifecycle_is_idempotent_and_preserves_data() -> Result<(), Box<dyn Error>> {
    let home = TestDirectory::new()?;
    let home_text = path(&home.0);
    let run_agent = |arguments: &[&str]| -> Result<std::process::Output, Box<dyn Error>> {
        Ok(Command::new(env!("CARGO_BIN_EXE_hyphae"))
            .env("HOME", &home_text)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("HYPHAE_NATIVE_API_KEY_FILE")
            .args(arguments)
            .output()?)
    };
    let setup = run_agent(&["agent", "setup", "--no-service"])?;
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let text = String::from_utf8_lossy(&setup.stdout);
    assert!(text.contains("created the memory-writer credential"));
    // Setup is idempotent.
    let again = run_agent(&["agent", "setup", "--no-service"])?;
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("already initialized"));
    // Status is redacted JSON with every credential present.
    let status = run_agent(&["agent", "status"])?;
    let status: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(status["initialized"], true);
    assert_eq!(status["credentials"]["memory_writer"], true);
    assert!(status.to_string().find("hyp1_").is_none());
    // Backup, then restore over the live directory.
    let backup = run_agent(&["agent", "backup"])?;
    assert!(
        backup.status.success(),
        "{}",
        String::from_utf8_lossy(&backup.stderr)
    );
    let backup_line = String::from_utf8_lossy(&backup.stdout);
    let backup_path = backup_line
        .lines()
        .find_map(|line| line.strip_prefix("backup written: "))
        .ok_or("backup path")?
        .to_owned();
    let restore = run_agent(&["agent", "restore", "--backup", &backup_path])?;
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let doctor = run_agent(&["agent", "doctor"])?;
    let doctor: serde_json::Value = serde_json::from_slice(&doctor.stdout)?;
    assert_eq!(doctor["status"], "healthy");
    // Configure writes host configurations that never contain a secret.
    let opencode = run_agent(&["agent", "configure", "opencode"])?;
    assert!(opencode.status.success());
    let opencode_config = String::from_utf8(opencode.stdout)?;
    assert!(opencode_config.contains("opencode mcp add hyphae-memory"));
    assert!(opencode_config.contains("memory-reader.key"));
    assert!(!opencode_config.contains("--allow-write"));
    assert!(!opencode_config.contains("hyp1_"));
    // Proactive hooks fail open before setup or when the daemon is absent,
    // returning no context rather than blocking the host.
    let hook = Command::new(env!("CARGO_BIN_EXE_hyphae"))
        .env("HOME", &home_text)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .args(["agent", "hook", "--host", "opencode"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let hook = hook.wait_with_output()?;
    assert!(!hook.status.success(), "empty hook input must fail closed");
    // Remove preserves data; purge deletes only with the explicit flag.
    let remove = run_agent(&["agent", "remove"])?;
    assert!(remove.status.success());
    let data = home.0.join(".local/share/hyphae/agent-memory");
    assert!(data.join("FORMAT").exists());
    assert!(
        !home
            .0
            .join(".config/hyphae/credentials/memory-writer.key")
            .exists()
    );
    // Reinstall recovers Owner authority, reissues the agent keys, and keeps
    // the preserved directory usable.
    let reinstalled = run_agent(&["agent", "setup", "--no-service"])?;
    assert!(
        reinstalled.status.success(),
        "{}",
        String::from_utf8_lossy(&reinstalled.stderr)
    );
    let reinstalled_status = run_agent(&["agent", "status"])?;
    let reinstalled_status: serde_json::Value = serde_json::from_slice(&reinstalled_status.stdout)?;
    assert_eq!(reinstalled_status["initialized"], true);
    assert_eq!(reinstalled_status["credentials"]["operator"], true);
    assert_eq!(reinstalled_status["credentials"]["memory_reader"], true);
    assert_eq!(reinstalled_status["credentials"]["memory_writer"], true);
    let purge = run_agent(&["agent", "purge-data", "--yes"])?;
    assert!(purge.status.success());
    assert!(!data.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn agent_domain_migration_copies_verifies_and_removes_legacy_data() -> Result<(), Box<dyn Error>> {
    let home = TestDirectory::new()?;
    let home_text = path(&home.0);
    let run_agent = |arguments: &[&str]| -> Result<std::process::Output, Box<dyn Error>> {
        Ok(Command::new(env!("CARGO_BIN_EXE_hyphae"))
            .env("HOME", &home_text)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("HYPHAE_NATIVE_API_KEY_FILE")
            .args(arguments)
            .output()?)
    };
    let setup = run_agent(&["agent", "setup", "--no-service"])?;
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let data = home.0.join(".local/share/hyphae/agent-memory");
    let project = "migration/project";
    let text = "Keep the domain migration retry-safe";
    let layer = "work";
    let digest = blake3::Hasher::new()
        .update(b"hyphae-agent-memory")
        .update(&[0])
        .update(project.as_bytes())
        .update(&[0])
        .update(layer.as_bytes())
        .update(&[0])
        .update(text.as_bytes())
        .finalize();
    let mut identity_bytes = [0_u8; 16];
    identity_bytes.copy_from_slice(&digest.as_bytes()[..16]);
    let identity = u128::from_le_bytes(identity_bytes).max(1);
    let object_id = hyphae_native_product::ObjectId::new(identity)?;
    let envelope = serde_json::to_vec(&serde_json::json!({
        "project": project,
        "scope": "project",
        "kind": "decision",
        "layer": layer,
        "agent": "legacy-agent",
        "harness": "legacy-harness",
        "model": "legacy-model",
        "text": text,
        "expires_at_micros": null,
    }))?;
    let mut product = NativeProduct::open(&data)?;
    product.ingest_search_batch(
        hyphae_native_product::ObjectId::new(13)?,
        &ProductSearchIngestBatch {
            idempotency_id: identity,
            documents: vec![ProductDocument {
                object_id,
                text: text.to_owned(),
                doc_values: BTreeMap::from([
                    (
                        "project".to_owned(),
                        ProductDocValue::String(project.to_owned()),
                    ),
                    (
                        "kind".to_owned(),
                        ProductDocValue::String("decision".to_owned()),
                    ),
                    (
                        "layer".to_owned(),
                        ProductDocValue::String(layer.to_owned()),
                    ),
                    (
                        "harness".to_owned(),
                        ProductDocValue::String("legacy-harness".to_owned()),
                    ),
                    (
                        "model".to_owned(),
                        ProductDocValue::String("legacy-model".to_owned()),
                    ),
                ]),
                vectors: BTreeMap::new(),
            }],
        },
        0,
        ProductDurability::Strict,
    )?;
    let mut lifecycle_key = b"hyphae-memory/".to_vec();
    lifecycle_key.extend_from_slice(&13_u128.to_le_bytes());
    lifecycle_key.extend_from_slice(&identity.to_le_bytes());
    product.migration_store_public_entry(lifecycle_key.clone(), envelope.clone(), None)?;
    drop(product);

    let migrated = run_agent(&["agent", "migrate-domains"])?;
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let product = NativeProduct::open(&data)?;
    let snapshot = product.snapshot_bounded(0)?;
    let legacy = NativeProduct::search_documents_at_snapshot(
        &snapshot,
        hyphae_native_product::ObjectId::new(13)?,
        None,
        1,
    )?;
    let work = NativeProduct::search_documents_at_snapshot(
        &snapshot,
        hyphae_native_product::ObjectId::new(22)?,
        None,
        1,
    )?;
    assert!(legacy.documents.is_empty());
    assert_eq!(work.documents.len(), 1);
    assert_eq!(work.documents[0].object_id, object_id);
    assert!(snapshot.structure_get(&lifecycle_key).is_none());
    let mut work_key = b"hyphae-memory/".to_vec();
    work_key.extend_from_slice(&22_u128.to_le_bytes());
    work_key.extend_from_slice(&identity.to_le_bytes());
    assert_eq!(snapshot.structure_get(&work_key), Some(envelope.as_slice()));
    drop(product);

    let repeated = run_agent(&["agent", "migrate-domains"])?;
    assert!(repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("already complete"));
    Ok(())
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn agent_domain_migration_resumes_after_source_lifecycle_deletion() -> Result<(), Box<dyn Error>> {
    let home = TestDirectory::new()?;
    let home_text = path(&home.0);
    let run_agent = |arguments: &[&str]| -> Result<std::process::Output, Box<dyn Error>> {
        Ok(Command::new(env!("CARGO_BIN_EXE_hyphae"))
            .env("HOME", &home_text)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("HYPHAE_NATIVE_API_KEY_FILE")
            .args(arguments)
            .output()?)
    };
    assert!(
        run_agent(&["agent", "setup", "--no-service"])?
            .status
            .success()
    );
    let data = home.0.join(".local/share/hyphae/agent-memory");
    let project = "migration/resume";
    let text = "Resume source cleanup from the durable copy barrier";
    let digest = blake3::Hasher::new()
        .update(b"hyphae-agent-memory")
        .update(&[0])
        .update(project.as_bytes())
        .update(&[0])
        .update(b"work")
        .update(&[0])
        .update(text.as_bytes())
        .finalize();
    let mut identity_bytes = [0_u8; 16];
    identity_bytes.copy_from_slice(&digest.as_bytes()[..16]);
    let identity = u128::from_le_bytes(identity_bytes).max(1);
    let object_id = hyphae_native_product::ObjectId::new(identity)?;
    let envelope = serde_json::to_vec(&serde_json::json!({
        "project": project,
        "scope": "project",
        "kind": "decision",
        "layer": "work",
        "agent": "legacy-agent",
        "harness": "legacy-harness",
        "model": "legacy-model",
        "text": text,
        "expires_at_micros": null,
    }))?;
    let document = ProductDocument {
        object_id,
        text: text.to_owned(),
        doc_values: BTreeMap::from([
            (
                "project".to_owned(),
                ProductDocValue::String(project.to_owned()),
            ),
            (
                "kind".to_owned(),
                ProductDocValue::String("decision".to_owned()),
            ),
            (
                "layer".to_owned(),
                ProductDocValue::String("work".to_owned()),
            ),
            (
                "harness".to_owned(),
                ProductDocValue::String("legacy-harness".to_owned()),
            ),
            (
                "model".to_owned(),
                ProductDocValue::String("legacy-model".to_owned()),
            ),
        ]),
        vectors: BTreeMap::new(),
    };
    let mut product = NativeProduct::open(&data)?;
    product.ingest_search_batch(
        hyphae_native_product::ObjectId::new(13)?,
        &ProductSearchIngestBatch {
            idempotency_id: identity,
            documents: vec![document.clone()],
        },
        0,
        ProductDurability::Strict,
    )?;
    let mut source_key = b"hyphae-memory/".to_vec();
    source_key.extend_from_slice(&13_u128.to_le_bytes());
    source_key.extend_from_slice(&identity.to_le_bytes());
    product.migration_store_public_entry(source_key.clone(), envelope.clone(), None)?;
    product.ingest_search_batch(
        hyphae_native_product::ObjectId::new(22)?,
        &ProductSearchIngestBatch {
            idempotency_id: identity,
            documents: vec![document.clone()],
        },
        0,
        ProductDurability::Strict,
    )?;
    let mut destination_key = b"hyphae-memory/".to_vec();
    destination_key.extend_from_slice(&22_u128.to_le_bytes());
    destination_key.extend_from_slice(&identity.to_le_bytes());
    product.migration_store_public_entry(destination_key.clone(), envelope.clone(), None)?;
    let copy_key = b"hyphae-agent-memory/migration/13-to-domains/v1/copy-complete".to_vec();
    let plan_key = b"hyphae-agent-memory/migration/13-to-domains/v1/plan".to_vec();
    let snapshot = product.snapshot_bounded(0)?;
    let mut document_hasher = blake3::Hasher::new();
    document_hasher.update(b"hyphae-agent-memory-domain-document-v1\0");
    document_hasher.update(&identity.to_le_bytes());
    document_hasher.update(&(text.len() as u64).to_le_bytes());
    document_hasher.update(text.as_bytes());
    for (name, value) in &document.doc_values {
        document_hasher.update(&(name.len() as u64).to_le_bytes());
        document_hasher.update(name.as_bytes());
        document_hasher.update(format!("{value:?}").as_bytes());
    }
    let document_digest = document_hasher.finalize().to_hex().to_string();
    let payload_digest = blake3::hash(&envelope).to_hex().to_string();
    let mut records_hasher = blake3::Hasher::new();
    records_hasher.update(b"hyphae-agent-memory-domain-plan-v1\0");
    for value in [
        identity.to_string(),
        "22".to_owned(),
        document_digest.clone(),
        payload_digest.clone(),
    ] {
        records_hasher.update(value.as_bytes());
        records_hasher.update(&[0]);
    }
    let hex = b"0123456789abcdef";
    let lineage = String::from_utf8(
        snapshot
            .identity()
            .directory_lineage
            .iter()
            .flat_map(|byte| [hex[usize::from(byte >> 4)], hex[usize::from(byte & 0x0f)]])
            .collect(),
    )?;
    let plan = serde_json::to_vec(&serde_json::json!({
        "schema": "hyphae-agent-memory-domain-migration-v1",
        "directory_lineage": lineage,
        "records": [{
            "object_id": identity.to_string(),
            "destination": "22",
            "document_digest": document_digest,
            "payload_digest": payload_digest,
        }],
        "records_digest": records_hasher.finalize().to_hex().to_string(),
    }))?;
    let copy = blake3::hash(&plan).as_bytes().to_vec();
    product.migration_store_public_entry(plan_key, plan, None)?;
    product.migration_store_public_entry(copy_key, copy, None)?;
    product.migration_delete_public_entry(source_key.clone())?;
    drop(product);

    let resumed = run_agent(&["agent", "migrate-domains"])?;
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let product = NativeProduct::open(&data)?;
    let snapshot = product.snapshot_bounded(0)?;
    assert!(snapshot.structure_get(&source_key).is_none());
    assert!(snapshot.structure_get(&destination_key).is_some());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn memory_profile_isolates_projects_and_gates_writes() -> Result<(), Box<dyn Error>> {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run_isolated(&["init", "--data-dir", &data_text])?;
    run_isolated(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.agent_memory",
        "--memory-schema",
    ])?;
    for (collection, name) in [(21, "personal"), (22, "work"), (23, "journal")] {
        run_isolated(&[
            "catalog",
            "--data-dir",
            &data_text,
            "create-search-collection",
            "--database",
            "10",
            "--schema",
            "11",
            "--collection",
            &collection.to_string(),
            "--analyzer",
            "12",
            "--name",
            &format!("main.public.agent_memory_{name}"),
            "--memory-schema",
            "--reuse-schema",
        ])?;
    }
    for collection in [21, 22, 23] {
        run_isolated(&[
            "search",
            "--data-dir",
            &data_text,
            "provision",
            "--collection",
            &collection.to_string(),
        ])?;
    }
    run_isolated(&[
        "search",
        "--data-dir",
        &data_text,
        "provision",
        "--collection",
        "13",
    ])?;
    let owner_key = temporary.0.join("owner.key");
    run_isolated(&[
        "security",
        "--data-dir",
        &data_text,
        "bootstrap",
        "--name",
        "Owner",
        "--key-out",
        &path(&owner_key),
    ])?;
    let fixture = SecurityWriteFixture {
        temporary,
        data: data.clone(),
        owner_key: owner_key.clone(),
        owner_secret: fs::read_to_string(&owner_key)?,
    };
    let probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let address = probe.local_addr()?;
    drop(probe);
    let endpoint = std::env::temp_dir().join(format!("hmp-{}.sock", Uuid::now_v7()));
    let address_text = address.to_string();
    let mut server = spawn_native_serve(
        &data,
        &endpoint,
        &["--native-api-key-auth", "--http-bind", &address_text],
    )?;
    wait_for_authenticated_http_ready(&mut server, &address_text, &fixture.owner_secret)
        .await
        .map_err(|error| std::io::Error::other(format!("HTTP readiness: {error}")))?;
    let _server_guard = ChildGuard(&mut server);

    let handshake = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    ];
    let list = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}});

    // The read-only profile lists exactly recall and status, and refuses
    // the write tools even when named directly.
    let mut messages = handshake.to_vec();
    messages.push(list.clone());
    messages.push(
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "name":"hyphae_memory_store",
        "arguments":{"project":"acme/site","text":"read-only must refuse"}}}),
    );
    let reader = run_mcp_session_with_flags(
        &address_text,
        &owner_key,
        &["--profile", "memory"],
        &messages,
    )?;
    let names: Vec<_> = reader[1]["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(names, ["hyphae_memory_recall", "hyphae_memory_status"]);
    assert_eq!(reader[2]["error"]["code"], -32602);

    // The write profile stores under two projects plus one global memory.
    let store = |id: u64, project: &str, text: &str, scope: Option<&str>| {
        let mut arguments = serde_json::json!({
            "project": project,
            "text": text,
            "kind": "decision",
            "agent": "claude",
            "harness": "claude-code-cli",
            "model": "anthropic/claude-sonnet"
        });
        if let Some(scope) = scope {
            arguments["scope"] = serde_json::json!(scope);
        }
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":"hyphae_memory_store","arguments":arguments}})
    };
    let recall = |id: u64, project: &str| {
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":"hyphae_memory_recall",
            "arguments":{"project": project, "query":"deterministic packaging decision"}}})
    };
    let mut messages = handshake.to_vec();
    messages.push(list.clone());
    messages.push(store(
        3,
        "acme/site",
        "packaging decision: use the deterministic pipeline",
        None,
    ));
    messages.push(store(
        4,
        "acme/other",
        "unrelated note about gardening",
        None,
    ));
    messages.push(store(
        5,
        "acme/site",
        "global packaging constraint for every project",
        Some("global"),
    ));
    messages.push(recall(6, "acme/site"));
    messages.push(recall(7, "acme/other"));
    messages.push(
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{
        "name":"hyphae_memory_status","arguments":{}}}),
    );
    let writer = run_mcp_session_with_flags(
        &address_text,
        &owner_key,
        &["--profile", "memory", "--allow-write"],
        &messages,
    )?;
    let names: Vec<_> = writer[1]["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "hyphae_memory_recall",
            "hyphae_memory_status",
            "hyphae_memory_store",
            "hyphae_memory_journal",
            "hyphae_memory_forget",
        ]
    );
    let stored_id = writer[2]["result"]["structuredContent"]["id"]
        .as_str()
        .ok_or("stored id")?
        .to_owned();
    // Project isolation: acme/site sees its memory plus the global one and
    // never the other project's; acme/other sees only the global memory.
    let site: Vec<_> = writer[5]["result"]["structuredContent"]["memories"]
        .as_array()
        .ok_or("site memories")?
        .iter()
        .map(|memory| memory["text"].as_str().unwrap_or("").to_owned())
        .collect();
    assert!(site.iter().any(|text| text.contains("packaging decision")));
    assert!(
        site.iter()
            .any(|text| text.contains("global packaging constraint"))
    );
    assert!(!site.iter().any(|text| text.contains("gardening")));
    let site_items = writer[5]["result"]["structuredContent"]["memories"]
        .as_array()
        .ok_or("site memory items")?;
    let work = site_items
        .iter()
        .find(|memory| {
            memory["text"]
                .as_str()
                .is_some_and(|text| text.contains("packaging decision"))
        })
        .ok_or("work memory")?;
    assert_eq!(work["layer"], "work");
    assert_eq!(work["harness"], "claude-code-cli");
    assert_eq!(work["model"], "anthropic/claude-sonnet");
    let other: Vec<_> = writer[6]["result"]["structuredContent"]["memories"]
        .as_array()
        .ok_or("other memories")?
        .iter()
        .map(|memory| memory["text"].as_str().unwrap_or("").to_owned())
        .collect();
    assert!(
        !other
            .iter()
            .any(|text| text.contains("packaging decision:"))
    );
    assert!(
        other
            .iter()
            .any(|text| text.contains("global packaging constraint"))
    );
    assert_eq!(writer[7]["result"]["structuredContent"]["status"], "ok");

    // The model journal is a separate first-person layer with exact harness
    // and model provenance. Work-only recall excludes it; journal recall
    // returns it for cross-model reflection.
    let journal = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
    "name":"hyphae_memory_journal","arguments":{
        "project":"acme/site",
        "text":"I noticed the deterministic pipeline reduces release ambiguity.",
        "harness":"codex-cli",
        "model":"openai/codex-model"
    }}});
    let journal_recall = |id: u64, layer: &str| {
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
        "name":"hyphae_memory_recall","arguments":{
            "project":"acme/site",
            "query":"deterministic pipeline ambiguity",
            "layer":layer
        }}})
    };
    let mut messages = handshake.to_vec();
    messages.push(journal);
    messages.push(journal_recall(4, "work"));
    messages.push(journal_recall(5, "journal"));
    let journal_results = run_mcp_session_with_flags(
        &address_text,
        &owner_key,
        &["--profile", "memory", "--allow-write"],
        &messages,
    )?;
    let work_only = journal_results[2]["result"]["structuredContent"]["memories"]
        .as_array()
        .ok_or("work-only memories")?;
    assert!(work_only.iter().all(|memory| memory["layer"] == "work"));
    let journal_only = journal_results[3]["result"]["structuredContent"]["memories"]
        .as_array()
        .ok_or("journal-only memories")?;
    assert_eq!(journal_only.len(), 1);
    assert_eq!(journal_only[0]["layer"], "journal");
    assert_eq!(journal_only[0]["harness"], "codex-cli");
    assert_eq!(journal_only[0]["model"], "openai/codex-model");

    // Forgetting demands the owning project; the wrong project is refused,
    // the right one removes permanently and idempotently.
    let forget = |id: u64, project: &str, memory: &str| {
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":"hyphae_memory_forget",
            "arguments":{"project": project, "id": memory}}})
    };
    let mut messages = handshake.to_vec();
    messages.push(forget(3, "acme/other", &stored_id));
    messages.push(forget(4, "acme/site", &stored_id));
    messages.push(forget(5, "acme/site", &stored_id));
    messages.push(recall(6, "acme/site"));
    let cleanup = run_mcp_session_with_flags(
        &address_text,
        &owner_key,
        &["--profile", "memory", "--allow-write"],
        &messages,
    )?;
    assert_eq!(
        cleanup[1]["result"]["structuredContent"]["error"]["code"],
        "invalid_request"
    );
    assert_eq!(
        cleanup[2]["result"]["structuredContent"]["status"], "forgotten",
        "forget response: {}",
        cleanup[2]
    );
    assert_eq!(
        cleanup[3]["result"]["structuredContent"]["status"],
        "forgotten"
    );
    let remaining: Vec<_> = cleanup[4]["result"]["structuredContent"]["memories"]
        .as_array()
        .ok_or("remaining")?
        .iter()
        .map(|memory| memory["text"].as_str().unwrap_or("").to_owned())
        .collect();
    assert!(
        !remaining
            .iter()
            .any(|text| text.contains("packaging decision:"))
    );
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn native_mcp_memory_tools_store_recall_and_forget_with_a_lifecycle()
-> Result<(), Box<dyn Error>> {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run_isolated(&["init", "--data-dir", &data_text])?;
    run_isolated(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.memories",
    ])?;
    run_isolated(&[
        "search",
        "--data-dir",
        &data_text,
        "provision",
        "--collection",
        "13",
    ])?;
    let owner_key = temporary.0.join("owner.key");
    run_isolated(&[
        "security",
        "--data-dir",
        &data_text,
        "bootstrap",
        "--name",
        "Owner",
        "--key-out",
        &path(&owner_key),
    ])?;
    let fixture = SecurityWriteFixture {
        temporary,
        data: data.clone(),
        owner_key: owner_key.clone(),
        owner_secret: fs::read_to_string(&owner_key)?,
    };
    let probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let address = probe.local_addr()?;
    drop(probe);
    let endpoint = std::env::temp_dir().join(format!("hmm-{}.sock", Uuid::now_v7()));
    let address_text = address.to_string();
    let mut server = spawn_native_serve(
        &data,
        &endpoint,
        &["--native-api-key-auth", "--http-bind", &address_text],
    )?;
    wait_for_authenticated_http_ready(&mut server, &address_text, &fixture.owner_secret)
        .await
        .map_err(|error| std::io::Error::other(format!("HTTP readiness: {error}")))?;
    let _server_guard = ChildGuard(&mut server);

    let handshake = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    ];
    let store = |id: u64, text: &str| {
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":"hyphae_native_memory_store",
            "arguments":{"collection":13,"text":text}}})
    };
    let recall = |id: u64| {
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":"hyphae_native_memory_recall",
            "arguments":{"collection":13,"query":"deterministic retrieval"}}})
    };
    let mut messages = handshake.to_vec();
    messages.push(store(2, "the user prefers deterministic retrieval"));
    messages.push(store(3, "unrelated gardening note"));
    messages.push(recall(4));
    let session =
        run_mcp_session_with_flags(&address_text, &owner_key, &["--allow-ingest"], &messages)?;
    assert_eq!(
        session[1]["result"]["structuredContent"]["status"],
        "stored"
    );
    let memory_id = session[1]["result"]["structuredContent"]["id"]
        .as_str()
        .ok_or("memory id")?
        .to_owned();
    assert_eq!(
        session[2]["result"]["structuredContent"]["status"],
        "stored"
    );
    let recalled = &session[3]["result"]["structuredContent"];
    let memories = recalled["memories"].as_array().ok_or("memories")?;
    assert_eq!(
        memories[0]["text"],
        "the user prefers deterministic retrieval"
    );
    assert_eq!(memories[0]["id"], memory_id.as_str());
    // Without prove the artifacts slot stays empty; the sealed path shares
    // the prove-search plumbing covered by its own session test.
    assert!(recalled["proof"].is_null());

    // Forget removes the lifecycle and the document; recall never
    // surfaces the memory again.
    let forget = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
        "name":"hyphae_native_memory_forget",
        "arguments":{"collection":13,"id":memory_id}}});
    let mut messages = handshake.to_vec();
    messages.push(forget);
    messages.push(recall(6));
    let session =
        run_mcp_session_with_flags(&address_text, &owner_key, &["--allow-ingest"], &messages)?;
    assert_eq!(
        session[1]["result"]["structuredContent"]["status"], "forgotten",
        "forget response: {}",
        session[1]
    );
    let recalled = &session[2]["result"]["structuredContent"];
    assert_eq!(recalled["memories"].as_array().map(Vec::len), Some(0));
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn native_mcp_proves_a_search_and_verifies_the_receipt_trustlessly()
-> Result<(), Box<dyn Error>> {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("data");
    let data_text = path(&data);
    run_isolated(&["init", "--data-dir", &data_text])?;
    run_isolated(&[
        "catalog",
        "--data-dir",
        &data_text,
        "create-search-collection",
        "--database",
        "10",
        "--schema",
        "11",
        "--collection",
        "13",
        "--analyzer",
        "12",
        "--name",
        "main.public.notes",
    ])?;
    run_isolated(&[
        "search",
        "--data-dir",
        &data_text,
        "provision",
        "--collection",
        "13",
    ])?;
    let documents = serde_json::json!([
        {"id":601,"text":"provable retrieval","vectors":{"exact":[0.0,1.0],"ann":[0.0,1.0]}},
        {"id":602,"text":"plain retrieval","vectors":{"exact":[1.0,0.0],"ann":[1.0,0.0]}}
    ])
    .to_string();
    run_isolated(&[
        "search",
        "--data-dir",
        &data_text,
        "ingest",
        "--collection",
        "13",
        "--idempotency-id",
        "1",
        "--documents-json",
        &documents,
    ])?;

    // The CLI generates one search proof to files while still unmanaged.
    let proof_out = temporary.0.join("search.proof");
    let witness_out = temporary.0.join("search.witness");
    let generated = run_isolated(&[
        "proof",
        "generate",
        "--data-dir",
        &data_text,
        "--operation-json",
        r#"{"operation":"search_collection","collection":13,"lexical":{"query":"provable"},"limit":5}"#,
        "--proof-out",
        &path(&proof_out),
        "--witness-out",
        &path(&witness_out),
    ])?;
    assert_eq!(generated["status"], "generated");
    assert_eq!(generated["kind"], "lexical");
    let anchor = generated["anchor"].as_str().ok_or("missing anchor")?;
    let verified = run_isolated(&[
        "proof",
        "verify",
        "--proof",
        &path(&proof_out),
        "--witness",
        &path(&witness_out),
        "--anchor",
        anchor,
    ])?;
    assert_eq!(verified["status"], "verified");
    assert_eq!(verified["scope"], "semantic_reexecution");

    // Bootstrap security and drive the same flow through MCP with a Reader.
    let owner_key = temporary.0.join("owner.key");
    run_isolated(&[
        "security",
        "--data-dir",
        &data_text,
        "bootstrap",
        "--name",
        "Owner",
        "--key-out",
        &path(&owner_key),
    ])?;
    let fixture = SecurityWriteFixture {
        temporary,
        data: data.clone(),
        owner_key: owner_key.clone(),
        owner_secret: fs::read_to_string(&owner_key)?,
    };
    let principal = fixture.owner(&[
        "principal",
        "create",
        "--name",
        "Proof reader",
        "--idempotency-token",
        "9100",
    ])?;
    let principal_id = principal["result_id"]
        .as_str()
        .ok_or("missing principal identity")?
        .to_owned();
    fixture.owner(&[
        "assignment",
        "create-built-in",
        "--principal-id",
        &principal_id,
        "--role",
        "reader",
        "--scope",
        "instance",
        "--idempotency-token",
        "9101",
    ])?;
    fixture.owner(&[
        "principal",
        "set-enabled",
        "--principal-id",
        &principal_id,
        "--enabled",
        "true",
        "--idempotency-token",
        "9102",
    ])?;
    let reader_key = fixture.temporary.0.join("reader.key");
    fixture.issue_reader_key(&principal_id, &reader_key)?;

    let probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let address = probe.local_addr()?;
    drop(probe);
    let endpoint = std::env::temp_dir().join(format!("hmp-{}.sock", Uuid::now_v7()));
    let address_text = address.to_string();
    let mut server = spawn_native_serve(
        &data,
        &endpoint,
        &["--native-api-key-auth", "--http-bind", &address_text],
    )?;
    wait_for_authenticated_http_ready(&mut server, &address_text, &fixture.owner_secret)
        .await
        .map_err(|error| std::io::Error::other(format!("HTTP readiness: {error}")))?;
    let _server_guard = ChildGuard(&mut server);

    let messages = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"hyphae_native_prove_search",
            "arguments":{"collection":13,"lexical":{"query":"provable"},"limit":5}}}),
    ];
    let responses = run_mcp_session(&address_text, &reader_key, &messages)?;
    assert_eq!(responses[1]["result"]["isError"], false);
    let proven = &responses[1]["result"]["structuredContent"];
    assert_eq!(proven["status"], "generated");
    assert_eq!(proven["kind"], "lexical");
    assert_eq!(proven["response"]["hits"].as_array().map(Vec::len), Some(1));
    let proof_hex = proven["proof_hex"].as_str().ok_or("missing proof")?;
    let witness_hex = proven["witness_hex"].as_str().ok_or("missing witness")?;
    let anchor_hex = proven["anchor_hex"].as_str().ok_or("missing anchor")?;

    // The verify tool re-executes the proof trustlessly inside the adapter,
    // and a tampered proof fails closed.
    let mut tampered = proof_hex.to_owned();
    let flipped = if tampered.ends_with('0') { '1' } else { '0' };
    tampered.pop();
    tampered.push(flipped);
    let messages = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"hyphae_native_verify_proof",
            "arguments":{"proof_hex":proof_hex,"witness_hex":witness_hex,"anchor_hex":anchor_hex}}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"hyphae_native_verify_proof",
            "arguments":{"proof_hex":tampered,"witness_hex":witness_hex,"anchor_hex":anchor_hex}}}),
    ];
    let responses = run_mcp_session(&address_text, &reader_key, &messages)?;
    let report = &responses[1]["result"]["structuredContent"];
    assert_eq!(report["status"], "verified");
    assert_eq!(report["scope"], "semantic_reexecution");
    assert_eq!(report["kind"], "lexical");
    assert_eq!(
        responses[2]["result"]["structuredContent"]["error"]["code"],
        "invalid_request"
    );
    let _ignored = fs::remove_file(endpoint);
    Ok(())
}
