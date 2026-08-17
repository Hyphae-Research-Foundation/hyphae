// SPDX-License-Identifier: Apache-2.0

//! Create the restricted Auditor credential used by live Python conformance.

use std::{
    env,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    str,
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_product::{BuiltInRole, NativeProduct, ProductScope};
use serde_json::json;

#[derive(Debug)]
struct Arguments {
    data_dir: PathBuf,
    owner_key_file: PathBuf,
    auditor_key_out: PathBuf,
    metadata_out: PathBuf,
    legacy_bearer_file: Option<PathBuf>,
}

impl Arguments {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut values = env::args_os().skip(1);
        let mut data_dir = None;
        let mut owner_key_file = None;
        let mut auditor_key_out = None;
        let mut metadata_out = None;
        let mut legacy_bearer_file = None;
        while let Some(flag) = values.next() {
            let value = values
                .next()
                .ok_or_else(|| io::Error::other("fixture argument is missing its value"))?;
            match flag.to_str() {
                Some("--data-dir") if data_dir.is_none() => data_dir = Some(value.into()),
                Some("--owner-key-file") if owner_key_file.is_none() => {
                    owner_key_file = Some(value.into());
                }
                Some("--auditor-key-out") if auditor_key_out.is_none() => {
                    auditor_key_out = Some(value.into());
                }
                Some("--metadata-out") if metadata_out.is_none() => {
                    metadata_out = Some(value.into());
                }
                Some("--legacy-bearer-file") if legacy_bearer_file.is_none() => {
                    legacy_bearer_file = Some(value.into());
                }
                _ => return Err(io::Error::other("fixture arguments are invalid").into()),
            }
        }
        Ok(Self {
            data_dir: required(data_dir, "--data-dir")?,
            owner_key_file: required(owner_key_file, "--owner-key-file")?,
            auditor_key_out: required(auditor_key_out, "--auditor-key-out")?,
            metadata_out: required(metadata_out, "--metadata-out")?,
            legacy_bearer_file,
        })
    }
}

fn required(value: Option<PathBuf>, name: &str) -> Result<PathBuf, Box<dyn Error>> {
    value
        .ok_or_else(|| io::Error::other(format!("required fixture argument is missing: {name}")))
        .map_err(Into::into)
}

fn logical_time_micros() -> Result<i64, Box<dyn Error>> {
    let micros = SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros();
    Ok(i64::try_from(micros)?)
}

fn write_metadata(path: &Path, value: &serde_json::Value) -> Result<(), Box<dyn Error>> {
    let encoded = serde_json::to_vec_pretty(value)?;
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    output.write_all(&encoded)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = Arguments::parse()?;
    if let Some(path) = arguments.legacy_bearer_file.as_deref() {
        let mut legacy_bearer = b"python-managed-legacy-bearer-0123456789abcdef".to_vec();
        let result = prepare_legacy_bearer(
            &arguments.data_dir,
            &arguments.owner_key_file,
            path,
            &mut legacy_bearer,
        );
        legacy_bearer.fill(0);
        result?;
    }
    let mut owner_credential = fs::read(&arguments.owner_key_file)?;
    let result = create_auditor_fixture(&arguments, &owner_credential);
    owner_credential.fill(0);
    result
}

fn create_auditor_fixture(
    arguments: &Arguments,
    owner_credential: &[u8],
) -> Result<(), Box<dyn Error>> {
    let owner_credential = str::from_utf8(owner_credential)?;
    let mut product = NativeProduct::open(&arguments.data_dir)?;
    let logical_time = logical_time_micros()?;
    let owner = product.authenticate_api_key(owner_credential, logical_time)?;
    let auditor = product.create_security_principal(
        &owner,
        "Python managed conformance auditor",
        logical_time,
    )?;
    let owner = product.authenticate_api_key(owner_credential, logical_time)?;
    let assignment = product.assign_built_in_role(
        &owner,
        auditor.principal_id,
        BuiltInRole::Auditor,
        ProductScope::Instance,
        logical_time,
    )?;
    let owner = product.authenticate_api_key(owner_credential, logical_time)?;
    product.set_security_principal_enabled(&owner, auditor.principal_id, true, logical_time)?;
    let owner = product.authenticate_api_key(owner_credential, logical_time)?;
    let issued = product.issue_api_key_to_file(
        &owner,
        auditor.principal_id,
        "python-managed-conformance-auditor",
        [BuiltInRole::Auditor],
        BuiltInRole::Auditor.authorization(),
        None,
        &arguments.auditor_key_out,
        logical_time,
    )?;
    write_metadata(
        &arguments.metadata_out,
        &json!({
            "schema": "hyphae-python-managed-v2-fixture-v1",
            "principal_id": auditor.principal_id.to_string(),
            "auditor_assignment_id": assignment.assignment_id.to_string(),
            "key_id": issued.key_id.to_string(),
        }),
    )
}

fn prepare_legacy_bearer(
    data_dir: &Path,
    owner_key_file: &Path,
    legacy_bearer_file: &Path,
    legacy_bearer: &mut [u8],
) -> Result<(), Box<dyn Error>> {
    let mut legacy_output = restricted_output(legacy_bearer_file)?;
    legacy_output.write_all(legacy_bearer)?;
    legacy_output.sync_all()?;
    drop(legacy_output);
    drop(NativeProduct::open(data_dir)?);
    let mut product = NativeProduct::open_offline_owner(data_dir)?;
    let started = product.start_legacy_bearer_migration_offline(
        "Python managed conformance owner",
        "python-managed-conformance-owner",
        legacy_bearer,
        logical_time_micros()?,
    )?;
    let canonical = started.secret.expose_secret();
    let mut output = restricted_output(owner_key_file)?;
    output.write_all(canonical.as_bytes())?;
    output.sync_all()?;
    drop(output);
    product.activate_legacy_bearer_migration_offline(
        started.key_id,
        canonical,
        started.authorization_epoch,
        "Python managed conformance owner",
        "python-managed-conformance-owner",
        legacy_bearer,
        logical_time_micros()?,
    )?;
    Ok(())
}

fn restricted_output(path: &Path) -> Result<fs::File, Box<dyn Error>> {
    let mut output_options = OpenOptions::new();
    output_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        output_options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{Foundation::GENERIC_WRITE, Storage::FileSystem::READ_CONTROL};

        output_options
            .access_mode(GENERIC_WRITE | READ_CONTROL)
            .share_mode(0);
    }
    let output = output_options.open(path)?;
    #[cfg(windows)]
    if let Err(error) = hyphae_native_product::restrict_windows_credential_file(path, &output) {
        drop(output);
        let _ignored = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(output)
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use windows_permissions::{
        LocalBox, SecurityDescriptor, Sid,
        constants::{AccessRights, AceFlags, AceType, SeObjectType, SecurityInformation},
        utilities, wrappers,
    };

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    impl AsRef<Path> for TestDirectory {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    fn unique_test_path(parent: &Path) -> PathBuf {
        parent.join(format!(
            "hyphae-v2-fixture-windows-acl-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn fixture_auditor_key_strips_inherited_parent_aces() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = env::var_os("SystemDrive")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .map(|path| unique_test_path(&path))
            .and_then(|path| fs::create_dir(&path).ok().map(|()| path));
        let root = match root {
            Some(path) => path,
            None => {
                let path = unique_test_path(&env::temp_dir());
                fs::create_dir(&path)?;
                path
            }
        };
        let root = TestDirectory(root);
        install_inheritable_test_acl(root.as_ref())?;

        let inherited_control = root.0.join("inherited-control.key");
        let inherited_control_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&inherited_control)?;
        drop(inherited_control_file);
        assert!(file_has_inherited_world_ace(&inherited_control)?);
        fs::remove_file(inherited_control)?;

        let arguments = Arguments {
            data_dir: root.0.join("data"),
            owner_key_file: root.0.join("owner.key"),
            auditor_key_out: root.0.join("auditor.key"),
            metadata_out: root.0.join("fixture.json"),
            legacy_bearer_file: Some(root.0.join("legacy-bearer.key")),
        };
        drop(NativeProduct::create(&arguments.data_dir)?);
        let mut legacy_bearer = b"python-managed-legacy-bearer-0123456789abcdef".to_vec();
        prepare_legacy_bearer(
            &arguments.data_dir,
            &arguments.owner_key_file,
            arguments
                .legacy_bearer_file
                .as_deref()
                .ok_or("missing legacy bearer path")?,
            &mut legacy_bearer,
        )?;
        legacy_bearer.fill(0);
        let mut owner_credential = fs::read(&arguments.owner_key_file)?;
        let result = create_auditor_fixture(&arguments, &owner_credential);
        owner_credential.fill(0);
        result?;

        assert_restricted_file_acl(&arguments.auditor_key_out)?;
        Ok(())
    }

    fn install_inheritable_test_acl(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let current_sid = utilities::current_process_sid()?;
        let current = current_sid.to_string();
        let system = "S-1-5-18";
        let sddl = if current == system {
            format!("D:P(A;OICI;FA;;;{system})(A;OICI;FR;;;WD)")
        } else {
            format!("D:P(A;OICI;FA;;;{current})(A;OICI;FA;;;{system})(A;OICI;FR;;;WD)")
        };
        let descriptor: LocalBox<SecurityDescriptor> = sddl.parse()?;
        let dacl = descriptor
            .dacl()
            .ok_or("test security descriptor has no DACL")?;
        wrappers::SetNamedSecurityInfo(
            path.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None,
            None,
            Some(dacl),
            None,
        )?;
        wrappers::SetNamedSecurityInfo(
            path.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Owner,
            Some(current_sid.as_ref()),
            None,
            None,
            None,
        )?;
        Ok(())
    }

    fn file_has_inherited_world_ace(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
        let file = fs::File::open(path)?;
        let descriptor = wrappers::GetSecurityInfo(
            &file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        )?;
        let dacl = descriptor.dacl().ok_or("control file has no DACL")?;
        let world: LocalBox<Sid> = "S-1-1-0".parse()?;
        Ok((0..dacl.len()).any(|index| {
            dacl.get_ace(index).is_some_and(|ace| {
                ace.flags().contains(AceFlags::Inherited) && ace.sid() == Some(world.as_ref())
            })
        }))
    }

    fn assert_restricted_file_acl(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let file = fs::File::open(path)?;
        hyphae_native_product::validate_windows_restricted_file(&file)?;
        let descriptor = wrappers::GetSecurityInfo(
            &file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Owner | SecurityInformation::Dacl,
        )?;
        let current_sid = utilities::current_process_sid()?;
        let current = current_sid.to_string();
        let system_sid: LocalBox<Sid> = "S-1-5-18".parse()?;
        assert_eq!(descriptor.owner(), Some(current_sid.as_ref()));
        let dacl = descriptor.dacl().ok_or("auditor key has no DACL")?;
        assert_eq!(dacl.len(), if current == "S-1-5-18" { 1 } else { 2 });
        for index in 0..dacl.len() {
            let ace = dacl.get_ace(index).ok_or("auditor key ACE is missing")?;
            assert_eq!(ace.ace_type(), AceType::ACCESS_ALLOWED_ACE_TYPE);
            assert!(ace.flags().is_empty());
            assert_eq!(ace.mask(), AccessRights::FileAllAccess);
            assert!(
                ace.sid() == Some(current_sid.as_ref()) || ace.sid() == Some(system_sid.as_ref())
            );
        }
        Ok(())
    }
}
