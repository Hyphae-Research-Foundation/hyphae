// SPDX-License-Identifier: Apache-2.0

//! Independent, authenticated memory data interface with no operator dispatch.

use crate::exit::CliFailure;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) const SCHEMA: &str = "hyphae-memory-panel-v1";
const CONNECTION_SCHEMA: &str = "hyphae-memory-panel-connection-v1";
const REQUEST_LIMIT: usize = 64 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;
const OPERATIONS: [&str; 8] = [
    "status", "projects", "recall", "list", "store", "forget", "backups", "backup",
];

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Connection {
    schema: String,
    endpoint: PathBuf,
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    id: u64,
    token: String,
    operation: String,
    arguments: Value,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Recall {
    project: String,
    #[serde(default)]
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_layer")]
    layer: String,
    #[serde(default)]
    prove: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Store {
    project: String,
    text: String,
    kind: String,
    #[serde(default = "default_layer")]
    layer: String,
    #[serde(default = "default_scope")]
    scope: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Forget {
    project: String,
    id: String,
}

fn default_limit() -> usize {
    20
}
fn default_mode() -> String {
    "lexical".into()
}
fn default_layer() -> String {
    "work".into()
}
fn default_scope() -> String {
    "project".into()
}

fn error(id: Option<u64>, code: &str, message: &str) -> Value {
    json!({"schema":SCHEMA,"id":id,"ok":false,"error":{"code":code,"message":message}})
}

fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.contains('\0')
}

fn arguments(operation: &str, value: Value) -> Result<Value, CliFailure> {
    match operation {
        "status" | "projects" | "backups" | "backup" => {
            if value.as_object().is_none_or(|object| !object.is_empty()) {
                return Err(CliFailure::invalid());
            }
            Ok(json!({}))
        }
        "recall" | "list" => {
            let input: Recall = serde_json::from_value(value)?;
            if !bounded(&input.project, 256)
                || input.query.len() > 4096
                || !(1..=100).contains(&input.limit)
                || !matches!(input.mode.as_str(), "lexical" | "hybrid" | "semantic")
                || !matches!(
                    input.layer.as_str(),
                    "work" | "personal" | "journal" | "all"
                )
            {
                return Err(CliFailure::invalid());
            }
            Ok(serde_json::to_value(input)?)
        }
        "store" => {
            let input: Store = serde_json::from_value(value)?;
            if !bounded(&input.project, 256)
                || !bounded(&input.text, 2000)
                || !matches!(input.kind.as_str(), "fact" | "decision" | "constraint")
                || !matches!(input.layer.as_str(), "work" | "personal")
                || !matches!(input.scope.as_str(), "project" | "global")
            {
                return Err(CliFailure::invalid());
            }
            let mut input = serde_json::to_value(input)?;
            input["harness"] = json!("memory-panel");
            input["model"] = json!("user");
            Ok(input)
        }
        "forget" => {
            let input: Forget = serde_json::from_value(value)?;
            if !bounded(&input.project, 256)
                || !bounded(&input.id, 40)
                || !input.id.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(CliFailure::invalid());
            }
            Ok(serde_json::to_value(input)?)
        }
        _ => Err(CliFailure::invalid()),
    }
}

fn token_valid(token: &str) -> bool {
    token.len() == 70
        && token.starts_with("hypm1_")
        && token.as_bytes()[6..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

async fn respond(bytes: &[u8], token: &str) -> Value {
    let Ok(request) = serde_json::from_slice::<Request>(bytes) else {
        return error(None, "invalid_request", "Invalid memory request.");
    };
    let id = Some(request.id);
    if request.schema != SCHEMA {
        return error(id, "invalid_request", "Unsupported memory interface.");
    }
    // BLAKE3 Hash equality compares the fixed-length digests in constant time.
    if !token_valid(&request.token)
        || blake3::hash(request.token.as_bytes()) != blake3::hash(token.as_bytes())
    {
        return error(
            id,
            "unauthorized",
            "A dedicated memory client credential is required.",
        );
    }
    if !OPERATIONS.contains(&request.operation.as_str()) {
        return error(
            id,
            "forbidden_operation",
            "This identity grants only memory data operations.",
        );
    }
    let Ok(input) = arguments(&request.operation, request.arguments) else {
        return error(id, "invalid_request", "Invalid memory operation arguments.");
    };
    match dispatch(&request.operation, input).await {
        Ok(result) => json!({"schema":SCHEMA,"id":request.id,"ok":true,"result":result}),
        Err(failure) => error(
            id,
            failure.error().code().as_str(),
            "The memory operation could not be completed.",
        ),
    }
}

async fn dispatch(operation: &str, input: Value) -> Result<Value, CliFailure> {
    use hyphae_client::v2::RequestOptions;
    match operation {
        "status" => {
            let counts = crate::mcp::control_memory("status", json!({})).await?;
            let policy = crate::agent_policy::Policy::load()?;
            Ok(json!({"connected":true,"memories":counts["memories"],
                "semantic_enabled":policy.semantic.enabled,"operations":OPERATIONS}))
        }
        "projects" => crate::mcp::control_projects().await,
        "recall" | "list" => {
            let mut result = crate::mcp::control_memory(operation, input).await?;
            if result.get("proof").is_some_and(|proof| !proof.is_null()) {
                let proof = result["proof"].take();
                result["proof"] = verify_generated_proof(&proof)?;
            }
            Ok(result)
        }
        "store" | "forget" => crate::mcp::control_memory(operation, input).await,
        "backups" => {
            let mut backups = Vec::new();
            let directory = crate::agent::AgentPaths::resolve()?.backups.join("panel");
            if let Ok(entries) = std::fs::read_dir(directory) {
                for entry in entries.take(1000) {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if entry.file_type()?.is_dir() && backup_name(&name) {
                        backups.push(json!({"id":name,"label":name}));
                    }
                }
            }
            backups.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
            Ok(json!({"backups":backups}))
        }
        "backup" => {
            let directory = crate::agent::AgentPaths::resolve()?.backups.join("panel");
            private_directory(&directory)?;
            let name = format!("agent-memory-{}", crate::native::logical_time_micros());
            let request = hyphae_native_product::BackupRequest::new(directory.join(&name))
                .map_err(|_| CliFailure::invalid())?;
            let result = crate::agent_control::operator_client()?
                .backup(request, RequestOptions::default())
                .await
                .map_err(|_| CliFailure::io())?;
            let hyphae_native_product::ProductResponse::Backup(info) = result else {
                return Err(CliFailure::internal());
            };
            Ok(
                json!({"id":name,"checkpoint_digest":crate::encode_hex(&info.checkpoint_digest),
                "message":"Verified memory backup created."}),
            )
        }
        _ => Err(CliFailure::invalid()),
    }
}

fn backup_name(name: &str) -> bool {
    name.strip_prefix("agent-memory-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn verify_generated_proof(proof: &Value) -> Result<Value, CliFailure> {
    use hyphae_native_product::proof::{
        ExternalTrustedAnchor, NativeVerificationLimits, verify_native_proof_offline,
    };
    let path = |name| {
        proof[name]
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(CliFailure::internal)
    };
    let proof_path = path("proof_path")?;
    let witness_path = path("witness_path")?;
    let result = (|| {
        let bytes = std::fs::read(&proof_path)?;
        let witness = std::fs::read(&witness_path)?;
        let anchor = crate::decode_hex::<32>(
            proof["anchor_hex"]
                .as_str()
                .ok_or_else(CliFailure::internal)?,
        )?;
        let verified = verify_native_proof_offline(
            &bytes,
            &witness,
            ExternalTrustedAnchor::new(anchor),
            &NativeVerificationLimits::default(),
        )?;
        if !verified.semantic_reexecution_performed || crate::proof_kind(verified.kind) != "memory"
        {
            return Err(CliFailure::invalid());
        }
        Ok(
            json!({"status":"verified","scope":"semantic_reexecution","kind":"memory",
            "anchor_digest":crate::encode_hex(&verified.anchor_digest),
            "proof_digest":crate::encode_hex(&verified.proof_digest),
            "witness_digest":crate::encode_hex(&verified.witness_digest)}),
        )
    })();
    // These paths came from the server's own proof operation, never the client.
    std::fs::remove_file(proof_path)?;
    std::fs::remove_file(witness_path)?;
    result
}

#[cfg(unix)]
fn private_directory(path: &Path) -> Result<(), CliFailure> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !path.is_absolute() {
        return Err(CliFailure::invalid());
    }
    if !path.exists() {
        std::fs::create_dir_all(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || path.canonicalize()? != path
    {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_directory(_path: &Path) -> Result<(), CliFailure> {
    Err(CliFailure::invalid())
}

#[cfg(unix)]
pub(crate) fn initialize(config: &Path, endpoint: &Path) -> Result<(), CliFailure> {
    use std::io::Read as _;
    private_directory(config.parent().ok_or_else(CliFailure::invalid)?)?;
    private_directory(endpoint.parent().ok_or_else(CliFailure::invalid)?)?;
    if !endpoint.is_absolute() || endpoint.as_os_str().len() > 100 {
        return Err(CliFailure::invalid());
    }
    let mut entropy = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
    let connection = Connection {
        schema: CONNECTION_SCHEMA.into(),
        endpoint: endpoint.to_owned(),
        token: format!("hypm1_{}", crate::encode_hex(&entropy)),
    };
    entropy.fill(0);
    crate::native_client::reserve_restricted_api_key_file(config)?
        .write_secret(&serde_json::to_vec(&connection)?)?;
    println!("Memory client configuration created: {}", config.display());
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn initialize(_config: &Path, _endpoint: &Path) -> Result<(), CliFailure> {
    Err(CliFailure::invalid())
}

#[cfg(unix)]
pub(crate) async fn serve(config: &Path) -> Result<(), CliFailure> {
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt},
        sync::Arc,
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
        sync::Semaphore,
    };
    let metadata = std::fs::symlink_metadata(config)?;
    if !metadata.is_file()
        || metadata.len() > 4096
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(CliFailure::invalid());
    }
    let connection: Connection = serde_json::from_slice(&std::fs::read(config)?)?;
    if connection.schema != CONNECTION_SCHEMA || !token_valid(&connection.token) {
        return Err(CliFailure::invalid());
    }
    private_directory(
        connection
            .endpoint
            .parent()
            .ok_or_else(CliFailure::invalid)?,
    )?;
    // Bind refuses all existing entries, including live/stale sockets and links.
    let listener = UnixListener::bind(&connection.endpoint)?;
    std::fs::set_permissions(&connection.endpoint, std::fs::Permissions::from_mode(0o600))?;
    let token = Arc::new(connection.token);
    let permits = Arc::new(Semaphore::new(4));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let incoming = tokio::select! {
            accepted = listener.accept() => Some(accepted?),
            _ = tokio::signal::ctrl_c() => None,
            _ = terminate.recv() => None,
        };
        let Some((mut stream, _)) = incoming else {
            break;
        };
        if stream.peer_cred()?.uid() != rustix::process::geteuid().as_raw() {
            continue;
        }
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            let reply = serde_json::to_vec(&error(None, "busy", "The memory service is busy."))?;
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(&reply)).await;
            continue;
        };
        let token = Arc::clone(&token);
        tokio::spawn(async move {
            let _permit = permit;
            let mut bytes = Vec::new();
            let read = tokio::time::timeout(
                Duration::from_secs(5),
                (&mut stream)
                    .take((REQUEST_LIMIT + 1) as u64)
                    .read_to_end(&mut bytes),
            )
            .await;
            let response = match read {
                Ok(Ok(_)) if bytes.len() <= REQUEST_LIMIT => {
                    tokio::time::timeout(Duration::from_secs(120), respond(&bytes, &token))
                        .await
                        .unwrap_or_else(|_| {
                            error(None, "timeout", "The memory operation timed out.")
                        })
                }
                _ => error(
                    None,
                    "invalid_request",
                    "The request exceeded its input limit or deadline.",
                ),
            };
            if let Ok(mut reply) = serde_json::to_vec(&response) {
                if reply.len() > RESPONSE_LIMIT {
                    reply = serde_json::to_vec(&error(
                        None,
                        "limit_exceeded",
                        "The memory response exceeded its limit.",
                    ))
                    .unwrap_or_default();
                }
                let _ =
                    tokio::time::timeout(Duration::from_secs(5), stream.write_all(&reply)).await;
            }
        });
    }
    drop(listener);
    std::fs::remove_file(connection.endpoint)?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn serve(_config: &Path) -> Result<(), CliFailure> {
    Err(CliFailure::invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn credentials_cannot_reach_operator_or_arbitrary_commands()
    -> Result<(), serde_json::Error> {
        let token = format!("hypm1_{}", "ab".repeat(32));
        for operation in [
            "configure",
            "disconnect",
            "setup",
            "service_start",
            "restore",
            "remove",
            "semantic",
            "pause",
            "install",
            "install_model",
            "agents",
            "verify",
            "agent",
            "execute",
            "proxy",
            "security",
            "structure_set",
        ] {
            let response = respond(
                &serde_json::to_vec(&json!({
                    "schema":SCHEMA,"id":1,"token":token,"operation":operation,"arguments":{}
                }))?,
                &token,
            )
            .await;
            assert_eq!(
                response["error"]["code"], "forbidden_operation",
                "{operation}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn authentication_precedes_any_backend_access() -> Result<(), serde_json::Error> {
        let token = format!("hypm1_{}", "ab".repeat(32));
        let response = respond(
            &serde_json::to_vec(&json!({
                "schema":SCHEMA,"id":1,"token":format!("hypm1_{}", "cd".repeat(32)),
                "operation":"store","arguments":{}
            }))?,
            &token,
        )
        .await;
        assert_eq!(response["error"]["code"], "unauthorized");
        Ok(())
    }

    #[test]
    fn data_arguments_cannot_smuggle_paths_commands_or_provenance() {
        for operation in OPERATIONS {
            assert!(
                arguments(
                    operation,
                    json!({"command":"agent configure","path":"/tmp/control"})
                )
                .is_err()
            );
        }
        assert!(
            arguments(
                "store",
                json!({"project":"p","text":"fact","kind":"fact","harness":"codex"})
            )
            .is_err()
        );
        assert!(arguments("recall", json!({"project":"p","query":"x","limit":101})).is_err());
        assert!(arguments("forget", json!({"project":"p","id":"../../control"})).is_err());
    }
}
