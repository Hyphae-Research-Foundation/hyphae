// SPDX-License-Identifier: Apache-2.0
//! Real-socket authority and memory behavior integration checks.
#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::Shutdown,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    root: PathBuf,
    children: Vec<Child>,
    token: String,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let root = PathBuf::from("/tmp").join(format!("hmp-{}", uuid::Uuid::now_v7().simple()));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let root = root.canonicalize()?;
        for name in ["d", "c", "s", "r"] {
            fs::create_dir(root.join(name))?;
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            root,
            children: Vec::new(),
            token: String::new(),
        })
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hyphae"));
        command
            .current_dir(&self.root)
            .env("XDG_DATA_HOME", self.root.join("d"))
            .env("XDG_CONFIG_HOME", self.root.join("c"))
            .env("XDG_STATE_HOME", self.root.join("s"))
            .env("XDG_RUNTIME_DIR", self.root.join("r"))
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("HYPHAE_NATIVE_API_KEY_FILE")
            .env_remove("HYPHAE_BASE_URL")
            .env_remove("HYPHAE_DATA_DIR")
            .env_remove("HYPHAE_ENDPOINT");
        command
    }
    fn operator(&self, operation: &str, arguments: &Value) -> TestResult<Value> {
        let mut child = self
            .command()
            .args(["agent", "ui"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("missing operator stdin")?
            .write_all(&serde_json::to_vec(
                &json!({"schema":"hyphae-omarchy-control-v1",
                "operation":operation,"arguments":arguments}),
            )?)?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "operator failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(response["ok"], true, "{response}");
        Ok(response["result"].clone())
    }
    fn start_backend(&mut self) -> TestResult {
        self.operator("setup", &json!({"enable_service":false}))?;
        let status = self.operator("status", &json!({}))?;
        let endpoint = status["endpoint"]
            .as_str()
            .ok_or("missing native endpoint")?;
        let process = self
            .command()
            .args(["serve", "--data-dir"])
            .arg(self.root.join("d/hyphae/agent-memory"))
            .args(["--endpoint", endpoint, "--native-api-key-auth"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.children.push(process);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if Path::new(endpoint).exists()
                && self.operator("status", &json!({}))?["service_active"] == true
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err("native fixture did not become ready".into())
    }
    fn config(&self) -> PathBuf {
        self.root.join("c/panel/client.json")
    }
    fn socket(&self) -> PathBuf {
        self.root.join("r/panel/ui.sock")
    }
    fn initialize(&mut self) -> TestResult {
        let output = self
            .command()
            .args(["memory-panel", "init", "--config"])
            .arg(self.config())
            .arg("--socket")
            .arg(self.socket())
            .output()?;
        assert!(
            output.status.success(),
            "panel init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let config: Value = serde_json::from_slice(&fs::read(self.config())?)?;
        config["token"]
            .as_str()
            .ok_or("missing panel credential")?
            .clone_into(&mut self.token);
        Ok(())
    }
    fn start_panel(&mut self) -> TestResult {
        let process = self
            .command()
            .args(["memory-panel", "serve", "--config"])
            .arg(self.config())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.children.push(process);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.socket().exists() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err("panel socket did not appear".into())
    }
    fn packet(&self, operation: &str, arguments: &Value) -> Value {
        json!({"schema":"hyphae-memory-panel-v1","id":1,
            "token":self.token,"operation":operation,"arguments":arguments})
    }
    fn raw(&self, bytes: &[u8]) -> TestResult<Value> {
        let mut stream = UnixStream::connect(self.socket())?;
        stream.set_read_timeout(Some(Duration::from_secs(130)))?;
        stream.write_all(bytes)?;
        stream.shutdown(Shutdown::Write)?;
        let mut reply = Vec::new();
        stream.take(1024 * 1024 + 1).read_to_end(&mut reply)?;
        serde_json::from_slice(&reply).map_err(Into::into)
    }
    fn call(&self, operation: &str, arguments: &Value) -> TestResult<Value> {
        self.raw(&serde_json::to_vec(&self.packet(operation, arguments))?)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for child in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_authority_boundary(fixture: &Fixture) -> TestResult {
    let canary = fixture.root.join("c/opencode/config.json");
    fs::create_dir_all(canary.parent().ok_or("canary parent")?)?;
    fs::write(&canary, b"independently managed configuration\n")?;
    let key = fixture.root.join("c/hyphae/credentials/operator.key");
    let original_key = fs::read(&key)?;
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
        "proxy",
        "structure_set",
    ] {
        let args = match operation {
            "configure" | "disconnect" => json!({"host":"opencode","access":"write"}),
            "setup" => json!({"enable_service":true}),
            "restore" => json!({"backup":fixture.root.join("missing"),"confirm":true}),
            "remove" => json!({"confirm":true}),
            "semantic" => json!({"enabled":false}),
            "pause" => json!({"paused":true}),
            "verify" => json!({"proof":key,"witness":key,"anchor":"00"}),
            "proxy" => json!({"operation":"configure","arguments":{"host":"opencode"}}),
            "structure_set" => json!({"key":"hyphae-agent-policy/v1","value":"changed"}),
            _ => json!({}),
        };
        let response = fixture.call(operation, &args)?;
        assert_eq!(
            response["error"]["code"], "forbidden_operation",
            "{operation}: {response}"
        );
    }
    assert_eq!(fs::read(&key)?, original_key);
    assert_eq!(fs::read(canary)?, b"independently managed configuration\n");
    let mut packet = fixture.packet(
        "store",
        &json!({"project":"p","text":"unauthorized","kind":"fact"}),
    );
    packet["token"] = json!(format!("hypm1_{}", "00".repeat(32)));
    assert_eq!(
        fixture.raw(&serde_json::to_vec(&packet)?)?["error"]["code"],
        "unauthorized"
    );
    assert_eq!(
        fixture.call("backup", &json!({"destination":"/tmp/unrestricted"}))?["error"]["code"],
        "invalid_request"
    );
    assert_eq!(
        fixture.call(
            "store",
            &json!({"project":"p","text":"fact","kind":"fact","harness":"spoofed"})
        )?["error"]["code"],
        "invalid_request"
    );
    assert_eq!(
        fixture.raw(&vec![b' '; 65_537])?["error"]["code"],
        "invalid_request"
    );
    Ok(())
}

fn assert_memory_behavior(fixture: &Fixture) -> TestResult {
    let stored = fixture.call(
        "store",
        &json!({"project":"p","text":"aurora durable memory boundary","kind":"decision"}),
    )?;
    assert_eq!(stored["ok"], true, "{stored}");
    let memory = stored["result"]["id"].clone();
    let recall = fixture.call(
        "recall",
        &json!({"project":"p","query":"aurora","prove":true}),
    )?;
    assert_eq!(recall["ok"], true, "{recall}");
    assert_eq!(recall["result"]["memories"][0]["id"], memory);
    assert_eq!(recall["result"]["proof"]["status"], "verified");
    assert!(!recall.to_string().contains("proof_path"));
    assert!(!recall.to_string().contains("witness_path"));
    assert_eq!(
        fs::read_dir(fixture.root.join("s/hyphae/proofs"))?.count(),
        0
    );
    assert_eq!(
        fixture.call("recall", &json!({"project":"other","query":"aurora"}))?["result"]["memories"],
        json!([])
    );
    let status = fixture.call("status", &json!({}))?;
    assert_eq!(status["result"]["connected"], true, "{status}");
    assert!(status["result"].get("endpoint").is_none());
    assert!(status["result"].get("token").is_none());
    let backup = fixture.call("backup", &json!({}))?;
    assert_eq!(backup["ok"], true, "{backup}");
    assert!(backup["result"].get("path").is_none());
    assert_eq!(
        fixture.call("backups", &json!({}))?["result"]["backups"][0]["id"],
        backup["result"]["id"]
    );
    assert_eq!(
        fixture.call("forget", &json!({"project":"p","id":memory}))?["ok"],
        true
    );
    assert_eq!(
        fixture.call("recall", &json!({"project":"p","query":"aurora"}))?["result"]["memories"],
        json!([])
    );
    Ok(())
}

#[test]
fn dedicated_socket_preserves_operator_boundary_and_real_memory_behavior() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.start_backend()?;
    fixture.initialize()?;
    fixture.start_panel()?;
    assert_authority_boundary(&fixture)?;
    assert_memory_behavior(&fixture)
}

#[test]
fn provisioned_identity_and_existing_socket_cannot_be_overwritten() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.initialize()?;
    let config = fs::read(fixture.config())?;
    let repeated = fixture
        .command()
        .args(["memory-panel", "init", "--config"])
        .arg(fixture.config())
        .arg("--socket")
        .arg(fixture.socket())
        .output()?;
    assert!(!repeated.status.success());
    assert_eq!(fs::read(fixture.config())?, config);
    fixture.start_panel()?;
    let repeated = fixture
        .command()
        .args(["memory-panel", "serve", "--config"])
        .arg(fixture.config())
        .output()?;
    assert!(!repeated.status.success());
    assert_eq!(
        fixture.call("configure", &json!({}))?["error"]["code"],
        "forbidden_operation"
    );
    fs::set_permissions(fixture.config(), fs::Permissions::from_mode(0o644))?;
    let unsafe_config = fixture
        .command()
        .args(["memory-panel", "serve", "--config"])
        .arg(fixture.config())
        .output()?;
    assert!(!unsafe_config.status.success());
    Ok(())
}
