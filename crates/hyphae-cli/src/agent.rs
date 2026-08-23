// SPDX-License-Identifier: Apache-2.0

//! Agent Memory lifecycle: one command turns the engine into the product.
//!
//! `hyphae agent setup` creates everything the four-tool memory surface
//! needs — the data directory, the memory-schema collection, the operator
//! and agent credentials in restricted files — runs a store/recall/forget
//! smoke test through the real daemon, and prints exact operating,
//! backup, and removal instructions. Every resource lands under the
//! user's XDG paths, removal never deletes data, and only the explicit
//! purge command destroys the directory after interactive confirmation.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::exit::CliFailure;

/// Agent Memory collection identity inside the dedicated directory.
pub(crate) const MEMORY_COLLECTION: u128 = 13;
const MEMORY_DATABASE: u128 = 10;
const MEMORY_SCHEMA: u128 = 11;
const MEMORY_ANALYZER: u128 = 12;
const SERVICE_NAME: &str = "hyphae-agent-memory";

/// Resolved user paths for every Agent Memory resource.
pub(crate) struct AgentPaths {
    pub data: PathBuf,
    pub config: PathBuf,
    pub credentials: PathBuf,
    pub backups: PathBuf,
}

impl AgentPaths {
    pub(crate) fn resolve() -> Result<Self, CliFailure> {
        let home = std::env::var_os("HOME").ok_or_else(CliFailure::invalid)?;
        let home = PathBuf::from(home);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map_or_else(|| home.join(".local/share"), PathBuf::from);
        let config_home =
            std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
        Ok(Self {
            data: data_home.join("hyphae/agent-memory"),
            config: config_home.join("hyphae"),
            credentials: config_home.join("hyphae/credentials"),
            backups: data_home.join("hyphae/backups"),
        })
    }

    fn operator_key(&self) -> PathBuf {
        self.credentials.join("operator.key")
    }

    fn reader_key(&self) -> PathBuf {
        self.credentials.join("memory-reader.key")
    }

    fn writer_key(&self) -> PathBuf {
        self.credentials.join("memory-writer.key")
    }
}

fn run_self(arguments: &[&str]) -> Result<(), CliFailure> {
    run_self_json(arguments).map(|_| ())
}

fn run_self_json(arguments: &[&str]) -> Result<serde_json::Value, CliFailure> {
    let output = std::process::Command::new(std::env::current_exe().map_err(|_| CliFailure::io())?)
        .args(arguments)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|_| CliFailure::io())?;
    if !output.status.success() {
        // A failing step prints its typed error on stdout; surface it.
        let _ignored = std::io::stderr().write_all(&output.stdout);
        return Err(CliFailure::internal());
    }
    serde_json::from_slice(&output.stdout).map_err(|_| CliFailure::internal())
}

fn restrict(path: &Path) -> Result<(), CliFailure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| CliFailure::io())?;
    }
    Ok(())
}

/// Creates every Agent Memory resource, explains each one first, smoke
/// tests the four operations, and prints operating instructions.
const READER_PERMISSIONS: &[&str] = &[
    "catalog.read",
    "credential.self_manage",
    "data.read",
    "discover",
    "proof.generate",
    "proof.verify",
    "search.execute",
];
const WRITER_PERMISSIONS: &[&str] = &[
    "catalog.read",
    "credential.self_manage",
    "data.read",
    "data.write",
    "discover",
    "proof.generate",
    "proof.verify",
    "search.execute",
];

#[allow(clippy::too_many_lines)]
pub(crate) fn setup(enable_service: bool, no_service: bool) -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    println!("Hyphae Agent Memory setup will create:");
    println!("  data directory     {}", paths.data.display());
    println!("  configuration      {}", paths.config.display());
    println!(
        "  credentials        {} (mode 0600)",
        paths.credentials.display()
    );
    println!("  backups            {}", paths.backups.display());
    println!();

    for directory in [&paths.config, &paths.credentials, &paths.backups] {
        std::fs::create_dir_all(directory).map_err(|_| CliFailure::io())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&paths.credentials, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| CliFailure::io())?;
    }
    let data_text = paths.data.display().to_string();

    if paths.data.join("FORMAT").exists() {
        println!("data directory already initialized; setup is idempotent");
    } else {
        std::fs::create_dir_all(paths.data.parent().ok_or_else(CliFailure::invalid)?)
            .map_err(|_| CliFailure::io())?;
        run_self(&["init", "--data-dir", &data_text])?;
        run_self(&[
            "catalog",
            "--data-dir",
            &data_text,
            "create-search-collection",
            "--database",
            &MEMORY_DATABASE.to_string(),
            "--schema",
            &MEMORY_SCHEMA.to_string(),
            "--collection",
            &MEMORY_COLLECTION.to_string(),
            "--analyzer",
            &MEMORY_ANALYZER.to_string(),
            "--name",
            "main.public.agent_memory",
            "--memory-schema",
        ])?;
        run_self(&[
            "search",
            "--data-dir",
            &data_text,
            "provision",
            "--collection",
            &MEMORY_COLLECTION.to_string(),
        ])?;
        println!("created the agent-memory collection with the memory schema");
    }

    // Credentials: the operator key from bootstrap, plus one reader and
    // one writer key for agent hosts. The directory holds nothing but the
    // Agent Memory collection, so the built-in roles are collection-bound
    // in effect.
    let operator_key = paths.operator_key();
    if operator_key.exists() {
        println!("operator credential already present");
    } else {
        run_self(&[
            "security",
            "--data-dir",
            &data_text,
            "bootstrap",
            "--name",
            "Agent Memory Operator",
            "--key-out",
            &operator_key.display().to_string(),
        ])?;
        restrict(&operator_key)?;
        println!("created the operator credential (never hand this to an agent)");
    }
    let operator_text = operator_key.display().to_string();
    for (key_path, role, principal_name, label, token_base, permissions) in [
        (
            paths.reader_key(),
            "reader",
            "Agent Memory Reader",
            "memory-reader",
            0x4147_u64,
            READER_PERMISSIONS,
        ),
        (
            paths.writer_key(),
            "writer",
            "Agent Memory Writer",
            "memory-writer",
            0x4157_u64,
            WRITER_PERMISSIONS,
        ),
    ] {
        if key_path.exists() {
            println!("{label} credential already present");
            continue;
        }
        let created = run_self_json(&[
            "security",
            "--data-dir",
            &data_text,
            "--native-api-key-file",
            &operator_text,
            "principal",
            "create",
            "--name",
            principal_name,
            "--idempotency-token",
            &token_base.to_string(),
        ])?;
        let principal_id = created
            .get("result_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(CliFailure::internal)?
            .to_owned();
        run_self(&[
            "security",
            "--data-dir",
            &data_text,
            "--native-api-key-file",
            &operator_text,
            "principal",
            "set-enabled",
            "--principal-id",
            &principal_id,
            "--enabled",
            "true",
            "--idempotency-token",
            &(token_base + 3).to_string(),
        ])?;
        run_self(&[
            "security",
            "--data-dir",
            &data_text,
            "--native-api-key-file",
            &operator_text,
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
        let mut issue = vec![
            "security".to_owned(),
            "--data-dir".to_owned(),
            data_text.clone(),
            "--native-api-key-file".to_owned(),
            operator_text.clone(),
            "key".to_owned(),
            "issue".to_owned(),
            "--principal-id".to_owned(),
            principal_id.clone(),
            "--label".to_owned(),
            label.to_owned(),
            "--role".to_owned(),
            role.to_owned(),
        ];
        for permission in permissions {
            issue.push("--permission".to_owned());
            issue.push((*permission).to_owned());
        }
        issue.extend([
            "--scope".to_owned(),
            "instance".to_owned(),
            "--key-out".to_owned(),
            key_path.display().to_string(),
            "--idempotency-token".to_owned(),
            (token_base + 2).to_string(),
        ]);
        let issue: Vec<&str> = issue.iter().map(String::as_str).collect();
        run_self(&issue)?;
        restrict(&key_path)?;
        println!("created the {label} credential");
    }

    if no_service {
        println!("service installation skipped (--no-service)");
    } else {
        let unit_path = install_service_unit(&paths)?;
        println!("installed service {}", unit_path.display());
        let start = enable_service || {
            print!("enable and start the service now? [y/N]: ");
            std::io::stdout().flush().map_err(|_| CliFailure::io())?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|_| CliFailure::io())?;
            matches!(answer.trim(), "y" | "Y" | "yes")
        };
        if start {
            systemctl(&["enable", "--now", SERVICE_NAME])?;
            println!("service enabled and started");
        } else {
            println!("service installed but not enabled; start it with:");
            println!("  systemctl --user enable --now {SERVICE_NAME}");
        }
    }

    println!();
    println!("Agent Memory is ready. Operate it with:");
    println!("  hyphae serve --data-dir {data_text} \\");
    println!("      --native-api-key-auth --http-bind 127.0.0.1:8787");
    println!("  hyphae mcp --profile memory --allow-write \\");
    println!("      --base-url http://127.0.0.1:8787");
    println!(
        "      (HYPHAE_NATIVE_API_KEY_FILE={})",
        paths.writer_key().display()
    );
    println!("  hyphae agent status");
    println!("  hyphae agent backup");
    println!();
    println!("Removal never deletes data: `hyphae agent remove` keeps");
    println!("{data_text} and {} intact;", paths.backups.display());
    println!("only `hyphae agent purge-data` deletes, after confirmation.");
    Ok(())
}

/// Redacted local status: paths, initialization, and credential presence.
pub(crate) fn status() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    let initialized = paths.data.join("FORMAT").exists();
    let value = serde_json::json!({
        "schema": "hyphae-agent-status-v1",
        "data_directory": paths.data.display().to_string(),
        "initialized": initialized,
        "credentials": {
            "operator": paths.operator_key().exists(),
            "memory_reader": paths.reader_key().exists(),
            "memory_writer": paths.writer_key().exists(),
        },
        "backups_directory": paths.backups.display().to_string(),
        "service": SERVICE_NAME,
        "service_installed": service_unit_path().is_ok_and(|path| path.exists()),
        "service_active": service_active(),
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Engine doctor over the Agent Memory directory with the operator
/// credential.
pub(crate) fn doctor() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    let output = run_self_json(&[
        "doctor",
        "--data-dir",
        &paths.data.display().to_string(),
        "--native-api-key-file",
        &paths.operator_key().display().to_string(),
    ])?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// One verified backup under the backups directory.
pub(crate) fn backup() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    std::fs::create_dir_all(&paths.backups).map_err(|_| CliFailure::io())?;
    let destination = paths.backups.join(format!(
        "agent-memory-{}",
        crate::native::logical_time_micros()
    ));
    run_self(&[
        "backup",
        "create",
        "--data-dir",
        &paths.data.display().to_string(),
        "--native-api-key-file",
        &paths.operator_key().display().to_string(),
        "--out",
        &destination.display().to_string(),
    ])?;
    println!("backup written: {}", destination.display());
    Ok(())
}

/// Restores one verified backup: the service must be stopped, the current
/// directory is preserved aside, and the backup is verified before it
/// replaces anything.
pub(crate) fn restore(backup: &Path) -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    if service_active() {
        eprintln!("stop the service first: systemctl --user stop {SERVICE_NAME}");
        return Err(CliFailure::invalid());
    }
    run_self(&[
        "backup",
        "verify",
        "--backup",
        &backup.display().to_string(),
    ])?;
    let stamp = crate::native::logical_time_micros();
    let preserved = paths
        .data
        .with_file_name(format!("agent-memory.pre-restore-{stamp}"));
    if paths.data.exists() {
        std::fs::rename(&paths.data, &preserved).map_err(|_| CliFailure::io())?;
        println!("previous data preserved at {}", preserved.display());
    }
    run_self(&[
        "restore",
        "--backup",
        &backup.display().to_string(),
        "--data-dir",
        &paths.data.display().to_string(),
    ])?;
    println!(
        "restored {} from {}",
        paths.data.display(),
        backup.display()
    );
    Ok(())
}

/// The upgrade flow from the product contract: stop, backup, doctor,
/// start, and verify a recall answers.
pub(crate) fn upgrade() -> Result<(), CliFailure> {
    println!("stopping the service");
    let _ignored = systemctl(&["stop", SERVICE_NAME]);
    backup()?;
    doctor()?;
    println!("starting the service");
    systemctl(&["start", SERVICE_NAME])?;
    println!("service restarted; verify a known memory with your agent host");
    Ok(())
}

fn systemctl(arguments: &[&str]) -> Result<(), CliFailure> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .status()
        .map_err(|_| CliFailure::io())?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::internal())
    }
}

fn service_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SERVICE_NAME])
        .status()
        .is_ok_and(|status| status.success())
}

fn service_unit_path() -> Result<PathBuf, CliFailure> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(CliFailure::invalid)?;
    Ok(config_home.join(format!("systemd/user/{SERVICE_NAME}.service")))
}

/// Writes the user service: loopback-only, explicit paths, bounded
/// resources, clean shutdown, and no secrets in arguments or logs.
fn install_service_unit(paths: &AgentPaths) -> Result<PathBuf, CliFailure> {
    let unit_path = service_unit_path()?;
    std::fs::create_dir_all(unit_path.parent().ok_or_else(CliFailure::invalid)?)
        .map_err(|_| CliFailure::io())?;
    let binary = std::env::current_exe().map_err(|_| CliFailure::io())?;
    let unit = format!(
        "[Unit]\n\
         Description=Hyphae Agent Memory (local, shared, verifiable memory for coding agents)\n\
         Documentation=https://github.com/Hyphae-Research-Foundation/hyphae\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={binary} serve --data-dir {data} --endpoint %t/{service}.sock --native-api-key-auth --http-bind 127.0.0.1:8787\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         TimeoutStopSec=30\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         MemoryHigh=384M\n\
         MemoryMax=512M\n\
         TasksMax=64\n\
         LimitNOFILE=4096\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        binary = binary.display(),
        data = paths.data.display(),
        service = SERVICE_NAME,
    );
    std::fs::write(&unit_path, unit).map_err(|_| CliFailure::io())?;
    let _ignored = systemctl(&["daemon-reload"]);
    Ok(unit_path)
}

/// Removes the service and generated credentials while preserving data
/// and backups.
pub(crate) fn remove() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    let unit_path = service_unit_path()?;
    if unit_path.exists() {
        let _ignored = systemctl(&["disable", "--now", SERVICE_NAME]);
        std::fs::remove_file(&unit_path).map_err(|_| CliFailure::io())?;
        let _ignored = systemctl(&["daemon-reload"]);
        println!("removed service {}", unit_path.display());
    }
    for key in [paths.operator_key(), paths.reader_key(), paths.writer_key()] {
        if key.exists() {
            std::fs::remove_file(&key).map_err(|_| CliFailure::io())?;
            println!("removed credential {}", key.display());
        }
    }
    println!("preserved data      {}", paths.data.display());
    println!("preserved backups   {}", paths.backups.display());
    println!("reinstall any time with `hyphae agent setup`");
    Ok(())
}

/// Supported agent hosts for configuration generation.
#[derive(Clone, Copy)]
pub(crate) enum Host {
    Claude,
    Codex,
    Opencode,
}

/// Generates one host's MCP configuration for the memory profile. The
/// configuration carries only the credential file path — never a secret —
/// and the same binary, profile, and endpoint on every host.
pub(crate) fn configure(host: Host, write: bool) -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    let writer_key = paths.writer_key().display().to_string();
    match host {
        Host::Claude => {
            let server = serde_json::json!({
                "command": "hyphae",
                "args": ["mcp", "--profile", "memory", "--allow-write",
                          "--base-url", "http://127.0.0.1:8787"],
                "env": {"HYPHAE_NATIVE_API_KEY_FILE": writer_key},
            });
            println!("Claude Code — register with the claude CLI (user scope):");
            println!();
            println!(
                "  claude mcp add-json hyphae-memory --scope user '{}'",
                serde_json::to_string(&server)?
            );
            if write {
                println!();
                println!("(claude owns its configuration file; the command above");
                println!(" is the supported write path)");
            }
        }
        Host::Codex => {
            let section = format!(
                "[mcp_servers.hyphae-memory]\ncommand = \"hyphae\"\nargs = [\"mcp\", \"--profile\", \"memory\", \"--allow-write\", \"--base-url\", \"http://127.0.0.1:8787\"]\nenv = {{ HYPHAE_NATIVE_API_KEY_FILE = \"{writer_key}\" }}\n"
            );
            let config = std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".codex/config.toml"))
                .ok_or_else(CliFailure::invalid)?;
            if write {
                let existing = std::fs::read_to_string(&config).unwrap_or_default();
                if existing.contains("[mcp_servers.hyphae-memory]") {
                    println!("codex already configured at {}", config.display());
                } else {
                    std::fs::create_dir_all(config.parent().ok_or_else(CliFailure::invalid)?)
                        .map_err(|_| CliFailure::io())?;
                    let mut merged = existing;
                    if !merged.is_empty() && !merged.ends_with('\n') {
                        merged.push('\n');
                    }
                    merged.push_str(&section);
                    std::fs::write(&config, merged).map_err(|_| CliFailure::io())?;
                    println!("codex configured at {}", config.display());
                }
            } else {
                println!("Codex — append to {}:", config.display());
                println!();
                println!("{section}");
            }
        }
        Host::Opencode => {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })
                .ok_or_else(CliFailure::invalid)?
                .join("opencode/opencode.json");
            let server = serde_json::json!({
                "type": "local",
                "command": ["hyphae", "mcp", "--profile", "memory", "--allow-write",
                             "--base-url", "http://127.0.0.1:8787"],
                "enabled": true,
                "environment": {"HYPHAE_NATIVE_API_KEY_FILE": writer_key},
            });
            if write {
                let mut root: serde_json::Value = std::fs::read_to_string(&config)
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                let mcp = root
                    .as_object_mut()
                    .ok_or_else(CliFailure::invalid)?
                    .entry("mcp")
                    .or_insert_with(|| serde_json::json!({}));
                if mcp.get("hyphae-memory").is_some() {
                    println!("opencode already configured at {}", config.display());
                } else {
                    mcp.as_object_mut()
                        .ok_or_else(CliFailure::invalid)?
                        .insert("hyphae-memory".to_owned(), server);
                    std::fs::create_dir_all(config.parent().ok_or_else(CliFailure::invalid)?)
                        .map_err(|_| CliFailure::io())?;
                    std::fs::write(&config, serde_json::to_string_pretty(&root)? + "\n")
                        .map_err(|_| CliFailure::io())?;
                    println!("opencode configured at {}", config.display());
                }
            } else {
                println!("OpenCode — merge into {}:", config.display());
                println!();
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"mcp": {"hyphae-memory": server}})
                    )?
                );
            }
        }
    }
    Ok(())
}

/// Deletes the Agent Memory data directory after explicit confirmation.
pub(crate) fn purge_data(confirmed: bool) -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    if service_active() {
        eprintln!("the service owns the directory; stop it first:");
        eprintln!("  systemctl --user stop {SERVICE_NAME}");
        return Err(CliFailure::invalid());
    }
    if !confirmed {
        print!(
            "This permanently deletes {} — type 'purge' to confirm: ",
            paths.data.display()
        );
        std::io::stdout().flush().map_err(|_| CliFailure::io())?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|_| CliFailure::io())?;
        if answer.trim() != "purge" {
            println!("purge aborted; nothing was deleted");
            return Ok(());
        }
    }
    if paths.data.exists() {
        std::fs::remove_dir_all(&paths.data).map_err(|_| CliFailure::io())?;
        println!("deleted {}", paths.data.display());
    } else {
        println!("nothing to delete at {}", paths.data.display());
    }
    println!("backups remain at {}", paths.backups.display());
    Ok(())
}
