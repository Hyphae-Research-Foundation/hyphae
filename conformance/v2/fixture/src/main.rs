// SPDX-License-Identifier: AGPL-3.0-only

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
}

impl Arguments {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut values = env::args_os().skip(1);
        let mut data_dir = None;
        let mut owner_key_file = None;
        let mut auditor_key_out = None;
        let mut metadata_out = None;
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
                _ => return Err(io::Error::other("fixture arguments are invalid").into()),
            }
        }
        Ok(Self {
            data_dir: required(data_dir, "--data-dir")?,
            owner_key_file: required(owner_key_file, "--owner-key-file")?,
            auditor_key_out: required(auditor_key_out, "--auditor-key-out")?,
            metadata_out: required(metadata_out, "--metadata-out")?,
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
