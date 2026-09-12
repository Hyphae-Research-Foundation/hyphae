// SPDX-License-Identifier: Apache-2.0

//! Owner-only, bounded Linux IPC for the existing attested CPU model.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::{
        linux::fs::MetadataExt,
        unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{AttestedModel, MAX_TEXTS, MAX_TEXT_BYTES};

const MAX_REQUEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    id: u64,
    operation: String,
    #[serde(default)]
    texts: Vec<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    expected_model: Option<String>,
}

struct SocketGuard {
    path: PathBuf,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.st_ino() == self.inode && metadata.file_type().is_socket()
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn serve(model_dir: &Path, endpoint: &Path) -> Result<()> {
    let parent = endpoint
        .parent()
        .context("socket needs a private parent directory")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    let own_uid = fs::metadata("/proc/self")?.st_uid();
    if !parent_metadata.is_dir()
        || parent_metadata.st_uid() != own_uid
        || parent_metadata.permissions().mode() & 0o077 != 0
    {
        bail!("socket parent must be a user-owned private directory (0700)");
    }
    if let Ok(metadata) = fs::symlink_metadata(endpoint) {
        if !metadata.file_type().is_socket() || metadata.st_uid() != own_uid {
            bail!("refusing to replace a non-socket or foreign endpoint");
        }
        if UnixStream::connect(endpoint).is_ok() {
            bail!("embedding worker is already running");
        }
        fs::remove_file(endpoint)?;
    }
    let model = AttestedModel::load(model_dir)?;
    let listener = UnixListener::bind(endpoint)?;
    fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600))?;
    let _socket = SocketGuard {
        path: endpoint.to_owned(),
        inode: fs::symlink_metadata(endpoint)?.st_ino(),
    };
    // One model and one inference operation at a time. Kernel socket admission
    // is bounded, and a silent client cannot hold the reader indefinitely.
    for accepted in listener.incoming() {
        let mut stream = match accepted {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let response = match read_request(&stream) {
            Ok(request) => match execute(&model, &request) {
                Ok(result) => {
                    json!({"schema":"hyphae-embed-response-v1","id":request.id,"ok":true,"result":result})
                }
                Err(_) => error(request.id, "invalid_request"),
            },
            Err(_) => error(0, "invalid_request"),
        };
        if let Ok(mut bytes) = serde_json::to_vec(&response) {
            if bytes.len() > MAX_RESPONSE_BYTES {
                bytes = serde_json::to_vec(&error(0, "response_too_large"))?;
            }
            bytes.push(b'\n');
            let _ = stream.write_all(&bytes);
        }
    }
    Ok(())
}

fn error(id: u64, code: &str) -> Value {
    json!({"schema":"hyphae-embed-response-v1","id":id,"ok":false,"error":{"code":code}})
}

fn read_request(stream: &UnixStream) -> Result<Request> {
    let mut bytes = Vec::new();
    BufReader::new(stream)
        .take(MAX_REQUEST_BYTES + 1)
        .read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_REQUEST_BYTES || bytes.last() != Some(&b'\n') {
        bail!("bounded newline-delimited request required");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn execute(model: &AttestedModel, request: &Request) -> Result<Value> {
    if request.schema != "hyphae-embed-request-v1" || request.id == 0 {
        bail!("invalid worker contract");
    }
    let manifest = model.manifest();
    if request
        .expected_model
        .as_deref()
        .is_some_and(|expected| Some(expected) != manifest["fingerprint"].as_str())
    {
        bail!("model fingerprint differs");
    }
    if request.operation == "status" {
        if !request.texts.is_empty() || request.query.is_some() {
            bail!("status has no model input");
        }
        return Ok(manifest);
    }
    if request.texts.is_empty()
        || request.texts.len() > MAX_TEXTS
        || request.texts.iter().any(|text| text.len() > MAX_TEXT_BYTES)
        || request
            .query
            .as_ref()
            .is_some_and(|query| query.len() > MAX_TEXT_BYTES)
    {
        bail!("model input exceeds its bound");
    }
    match request.operation.as_str() {
        "embed" if request.query.is_none() => {
            let (vectors, attestation) = model.embed(&request.texts)?;
            Ok(
                json!({"schema":"hyphae-embed-output-v1","target":model.target,
                "dimensions":model.dimensions,"vectors":vectors,
                "attestation_hex":super::hex(&attestation),"model":manifest}),
            )
        }
        "rerank" => {
            let (scores, attestation) = model.rerank(
                request.query.as_deref().context("query required")?,
                &request.texts,
            )?;
            Ok(
                json!({"schema":"hyphae-rerank-output-v1","target":model.target,
                "scores":scores,"attestation_hex":super::hex(&attestation),"model":manifest}),
            )
        }
        _ => bail!("unsupported model operation"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_worker_request_rejects_unknown_fields_and_partial_frames() -> Result<()> {
        let (mut sender, receiver) = UnixStream::pair()?;
        sender.write_all(b"{\"schema\":\"hyphae-embed-request-v1\",\"id\":1,\"operation\":\"status\",\"role\":\"owner\"}\n")?;
        assert!(read_request(&receiver).is_err());
        let (mut sender, receiver) = UnixStream::pair()?;
        sender.write_all(b"{}")?;
        sender.shutdown(std::net::Shutdown::Write)?;
        assert!(read_request(&receiver).is_err());
        Ok(())
    }
}
