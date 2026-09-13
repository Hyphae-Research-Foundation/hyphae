// SPDX-License-Identifier: Apache-2.0

//! Versioned, bounded operator interface used by the Omarchy QML plugin.

use crate::{agent::AgentPaths, agent_policy::Policy, exit::CliFailure};
use hyphae_client::v2::{HyphaeClient, RequestOptions};
use hyphae_native_product::NativeProduct;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read as _, path::PathBuf, time::Duration};

const SCHEMA: &str = "hyphae-omarchy-control-v1";
const MAX_BYTES: u64 = 1024 * 1024;
pub(crate) const POLICY_KEY: &[u8] = b"hyphae-agent-policy/v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(default)]
    id: Option<u64>,
    schema: String,
    operation: String,
    arguments: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pause {
    paused: bool,
    #[serde(default)]
    project: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostArguments {
    host: String,
    #[serde(default = "read_access")]
    access: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup {
    #[serde(default)]
    enable_service: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    confirm: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Restore {
    backup: PathBuf,
    confirm: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verify {
    proof: PathBuf,
    witness: PathBuf,
    anchor: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Semantic {
    enabled: bool,
    #[serde(default)]
    model_dir: Option<PathBuf>,
    #[serde(default)]
    mode: Option<crate::agent_policy::SemanticSearchMode>,
}

fn read_access() -> String {
    "read".into()
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, CliFailure> {
    Ok(serde_json::from_value(value)?)
}

pub(crate) async fn run() -> Result<(), CliFailure> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let request = if bytes.len() as u64 <= MAX_BYTES {
        serde_json::from_slice::<Request>(&bytes).ok()
    } else {
        None
    };
    let response = match request {
        Some(request) if request.schema == SCHEMA && request.arguments.is_object() => {
            match execute(&request.operation, request.arguments).await {
                Ok(result) => json!({"schema":SCHEMA,"id":request.id,"ok":true,"result":result}),
                Err(error) => json!({"schema":SCHEMA,"id":request.id,"ok":false,"error":{
                    "code":error.error().code().as_str(), "message": if error.error().code() == hyphae_native_product::ProductErrorCode::LimitExceeded && matches!(request.operation.as_str(), "recall" | "list" | "verify") {
                        "The query or its complete proof exceeds the runtime limits. Recall without verification remains available."
                    } else { failure_message(&request.operation) },
                }}),
            }
        }
        _ => {
            json!({"schema":SCHEMA,"ok":false,"error":{"code":"invalid_request","message":"Invalid memory control request."}})
        }
    };
    let encoded = serde_json::to_string(&response)?;
    if encoded.len() as u64 > MAX_BYTES {
        return Err(CliFailure::invalid());
    }
    println!("{encoded}");
    Ok(())
}

fn failure_message(operation: &str) -> &'static str {
    match operation {
        "recall" | "list" => {
            "Memory recall is unavailable. Check the service, selected project and runtime version."
        }
        "verify" => {
            "The memory proof could not be verified. Check the proof, witness and independently retained anchor."
        }
        "configure" => {
            "The agent configuration could not be installed. An existing entry may require review."
        }
        "remove" | "disconnect" => {
            "A managed agent entry may have been edited or its CLI may be unavailable. Review the agent configuration; memory data is preserved."
        }
        "semantic" => {
            "Semantic setup could not finish. Check the local model and stop any independently started memory daemon before migration."
        }
        "restore" => {
            "Restore could not finish. The previous directory is preserved; check the backup and service state."
        }
        _ => {
            "The memory operation could not be completed. Refresh its status and run the health check."
        }
    }
}

pub(crate) async fn run_self(arguments: &[&str]) -> Result<Vec<u8>, CliFailure> {
    let output = tokio::process::Command::new(std::env::current_exe()?)
        .args(arguments)
        .env_remove("HYPHAE_NATIVE_API_KEY_FILE")
        .env_remove("HYPHAE_BASE_URL")
        .output()
        .await?;
    if !output.status.success() || output.stdout.len() as u64 > MAX_BYTES {
        return Err(CliFailure::invalid());
    }
    Ok(output.stdout)
}

pub(crate) fn operator_client() -> Result<HyphaeClient, CliFailure> {
    let paths = AgentPaths::resolve()?;
    let key = crate::native_client::read_api_key_file(&paths.credentials.join("operator.key"))?;
    HyphaeClient::local_authenticated(crate::agent::memory_endpoint(&paths), key.credential()?)
        .map_err(|_| CliFailure::io())
}

/// Wait through startup using IPC only; opening the directory here would
/// race the daemon for its exclusive lock after systemd starts the process.
pub(crate) async fn wait_for_service() -> Result<(), CliFailure> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let client = operator_client()?;
        let response = tokio::time::timeout(
            Duration::from_millis(500),
            client.structure_get(POLICY_KEY.to_vec(), RequestOptions::default()),
        )
        .await;
        if matches!(
            response,
            Ok(Ok(hyphae_native_product::ProductResponse::StructureValue(
                _
            )))
        ) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(hyphae_native_product::ProductError::from_code(
                hyphae_native_product::ProductErrorCode::Unavailable,
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(crate) async fn persist_policy() -> Result<(), CliFailure> {
    commit_policy(&Policy::current().await?).await
}

pub(crate) async fn commit_policy(policy: &Policy) -> Result<(), CliFailure> {
    policy.validate()?;
    let bytes = serde_json::to_vec(policy)?;
    let paths = AgentPaths::resolve()?;
    if crate::agent::local_endpoint_present(&paths) {
        operator_client()?
            .structure_set(POLICY_KEY.to_vec(), bytes, None, RequestOptions::default())
            .await
            .map_err(|_| CliFailure::io())?;
    } else {
        NativeProduct::open(&paths.data)?.migration_store_public_entry(
            POLICY_KEY.to_vec(),
            bytes,
            None,
        )?;
    }
    policy.save()
}

pub(crate) fn restore_policy() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    let product = NativeProduct::open(&paths.data)?;
    let snapshot = product.snapshot_bounded(crate::native::logical_time_micros())?;
    let policy = match snapshot.structure_get(POLICY_KEY) {
        Some(bytes) => serde_json::from_slice::<Policy>(bytes)?,
        None => Policy::default(),
    };
    policy.save()
}

#[allow(clippy::too_many_lines)]
async fn execute(operation: &str, arguments: Value) -> Result<Value, CliFailure> {
    match operation {
        "status" => {
            let _: Empty = parse(arguments)?;
            status().await
        }
        "projects" => {
            let _: Empty = parse(arguments)?;
            crate::mcp::control_projects().await
        }
        "agents" => {
            let _: Empty = parse(arguments)?;
            crate::agent::host_status()
        }
        "backups" => {
            let _: Empty = parse(arguments)?;
            backups()
        }
        "recall" | "list" | "forget" | "store" => {
            crate::mcp::control_memory(operation, arguments).await
        }
        "pause" => {
            let args: Pause = parse(arguments)?;
            let mut policy = Policy::load()?;
            if let Some(project) = args.project {
                if args.paused {
                    policy.paused_projects.insert(project);
                } else {
                    policy.paused_projects.remove(&project);
                }
            } else {
                policy.capture_enabled = !args.paused;
            }
            policy.save()?;
            Ok(json!({"message":if args.paused {"Capture paused."} else {"Capture resumed."}}))
        }
        "configure" => {
            let args: HostArguments = parse(arguments)?;
            host(&args.host)?;
            if !matches!(args.access.as_str(), "read" | "write") {
                return Err(CliFailure::invalid());
            }
            run_self(&[
                "agent",
                "configure",
                &args.host,
                "--access",
                &args.access,
                "--apply",
            ])
            .await?;
            Ok(
                json!({"message":"Agent connected. Review any hook trust request in the agent before starting a session."}),
            )
        }
        "disconnect" => {
            let args: HostArguments = parse(arguments)?;
            crate::agent::disconnect_host(host(&args.host)?)?;
            Ok(
                json!({"message":"Managed agent integration removed. Other configuration is preserved."}),
            )
        }
        "setup" => {
            let args: Setup = parse(arguments)?;
            let flag = if args.enable_service {
                "--enable-service"
            } else {
                "--no-service"
            };
            run_self(&["agent", "setup", flag]).await?;
            if args.enable_service {
                wait_for_service().await?;
            }
            let mut policy = Policy::load()?;
            policy.manage_services = args.enable_service;
            policy.save()?;
            persist_policy().await?;
            if args.enable_service {
                crate::agent_semantic::activate_worker().await?;
            }
            Ok(json!({"message":"Local memory configured. Connect your agents in the Agents tab."}))
        }
        "service_start" => {
            let _: Empty = parse(arguments)?;
            let active = crate::agent::service_active();
            if active {
                crate::agent::systemctl(&["stop", "hyphae-agent-memory"])?;
            }
            let activated = async {
                persist_policy().await?;
                run_self(&["agent", "backup"]).await?;
                run_self(&["agent", "setup", "--enable-service"]).await?;
                wait_for_service().await?;
                let mut policy = Policy::current().await?;
                policy.manage_services = true;
                commit_policy(&policy).await?;
                crate::agent::reconnect_managed_hosts()?;
                crate::agent_semantic::activate_worker().await
            }
            .await;
            if activated.is_err() && active {
                let _ = crate::agent::start_memory_service();
            }
            activated?;
            Ok(
                json!({"message":"Memory runtime activated and managed agents reconnected. Review changed hook definitions in Codex before the next session."}),
            )
        }
        "doctor" => {
            let _: Empty = parse(arguments)?;
            let report: Value = serde_json::from_slice(&run_self(&["agent", "doctor"]).await?)?;
            Ok(
                json!({"message":format!("Health check: {}", report["status"].as_str().unwrap_or("unknown")),"report":report}),
            )
        }
        "backup" => {
            let _: Empty = parse(arguments)?;
            persist_policy().await?;
            run_self(&["agent", "backup"]).await?;
            Ok(json!({"message":"Verified backup created."}))
        }
        "restore" => {
            let args: Restore = parse(arguments)?;
            let root = AgentPaths::resolve()?.backups.canonicalize()?;
            let backup = args.backup.canonicalize()?;
            if !args.confirm || !backup.starts_with(root) {
                return Err(CliFailure::invalid());
            }
            let active = crate::agent::service_active();
            if active {
                crate::agent::systemctl(&["stop", "hyphae-agent-memory"])?;
            }
            let restored = async {
                run_self(&[
                    "agent",
                    "restore",
                    "--backup",
                    backup.to_str().ok_or_else(CliFailure::invalid)?,
                ])
                .await?;
                restore_policy()?;
                if crate::agent::prepare_restored_credentials()? {
                    run_self(&["agent", "setup", "--no-service"]).await?;
                }
                Ok::<(), CliFailure>(())
            }
            .await;
            let restart = if active {
                crate::agent::start_memory_service()
            } else {
                Ok(())
            };
            // Stopping memory also stops its PartOf embedding service. Restore
            // both from the current profile even when backup validation fails.
            let worker = if active && restart.is_ok() {
                crate::agent_semantic::activate_worker().await
            } else {
                Ok(())
            };
            restored?;
            restart?;
            worker?;
            Ok(json!({"message":"Backup restored. Restart agent sessions to reconnect."}))
        }
        "verify" => {
            let args: Verify = parse(arguments)?;
            let output = run_self(&[
                "proof",
                "verify",
                "--proof",
                args.proof.to_str().ok_or_else(CliFailure::invalid)?,
                "--witness",
                args.witness.to_str().ok_or_else(CliFailure::invalid)?,
                "--anchor",
                &args.anchor,
            ])
            .await?;
            let result: Value = serde_json::from_slice(&output)?;
            if result["scope"] != "semantic_reexecution" || result["kind"] != "memory" {
                return Err(CliFailure::invalid());
            }
            Ok(result)
        }
        "semantic" => {
            let args: Semantic = parse(arguments)?;
            crate::agent_semantic::configure(args.enabled, args.model_dir, args.mode).await
        }
        "remove" => {
            if !parse::<Confirm>(arguments)?.confirm {
                return Err(CliFailure::invalid());
            }
            run_self(&["agent", "remove"]).await?;
            Ok(json!({"message":"Integration removed. Memories and backups are preserved."}))
        }
        _ => Err(CliFailure::invalid()),
    }
}

fn host(value: &str) -> Result<crate::agent::Host, CliFailure> {
    match value {
        "claude" => Ok(crate::agent::Host::Claude),
        "codex" => Ok(crate::agent::Host::Codex),
        "opencode" => Ok(crate::agent::Host::Opencode),
        "pi" => Ok(crate::agent::Host::Pi),
        _ => Err(CliFailure::invalid()),
    }
}

async fn status() -> Result<Value, CliFailure> {
    let policy = Policy::load()?;
    let paths = AgentPaths::resolve()?;
    let initialized = paths.data.join("FORMAT").exists()
        && paths.credentials.join("operator.key").is_file()
        && paths.reader_key().is_file()
        && paths.writer_key().is_file();
    let spool = std::env::var_os("XDG_STATE_HOME")
        .map_or_else(
            || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"),
            PathBuf::from,
        )
        .join("hyphae/agent-hooks/pending");
    let pending = std::fs::read_dir(spool).map_or(0, |items| items.flatten().take(1025).count());
    let mut value = json!({"installed":true,"initialized":initialized,"service_active":false,
        "capture_paused":!policy.capture_enabled,"paused_projects":policy.paused_projects,
        "pending_captures":pending,"pending_embeddings":0,"memories":0,
        "semantic_enabled":policy.semantic.enabled,"semantic_mode":policy.semantic.search_mode.as_str(),"semantic_ready":false,"control_version":1,
        "endpoint":crate::agent::memory_endpoint(&paths),"protocol_minor":7,"runtime_version":env!("CARGO_PKG_VERSION")});
    if initialized {
        let probe = tokio::time::timeout(Duration::from_millis(800), crate::mcp::control_memory("recall",
            json!({"project":"_hyphae_health_probe","query":"health","limit":1,"mode":"lexical"}))).await;
        value["service_active"] = json!(matches!(probe, Ok(Ok(_))));
        if let Ok(Ok(counts)) = tokio::time::timeout(
            Duration::from_millis(800),
            crate::mcp::control_memory("status", json!({})),
        )
        .await
        {
            value["memories"] = counts["memories"].clone();
        }
    }
    if policy.semantic.enabled {
        if let Ok(Ok(model)) = tokio::time::timeout(
            Duration::from_millis(300),
            crate::agent_semantic::request("status", &[], &policy),
        )
        .await
        {
            value["semantic_ready"] =
                json!(model["fingerprint"].as_str() == policy.model_fingerprint());
        }
        if let Ok(Ok(pending)) = tokio::time::timeout(
            Duration::from_millis(800),
            crate::agent_semantic::pending_count(&policy),
        )
        .await
        {
            value["pending_embeddings"] = json!(pending);
        }
    }
    Ok(value)
}

fn backups() -> Result<Value, CliFailure> {
    let paths = AgentPaths::resolve()?;
    let mut entries = Vec::new();
    if let Ok(directory) = std::fs::read_dir(paths.backups) {
        for entry in directory.flatten().take(1000) {
            if entry.file_type()?.is_dir()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("agent-memory-")
            {
                entries.push(json!({"label":entry.file_name().to_string_lossy(),"path":entry.path().display().to_string()}));
            }
        }
    }
    entries.sort_by(|a, b| b["label"].as_str().cmp(&a["label"].as_str()));
    Ok(json!({"backups":entries}))
}
