// SPDX-License-Identifier: Apache-2.0

//! Bounded MCP stdio adapter over managed Native HTTP v2: read-only by
//! default, with one explicitly opted-in bounded ingest tool.

use std::{
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::Path,
};

use hyphae_client::v2::{
    CancellationToken, ClientError, HttpTransport, HyphaeClient, RequestOptions,
};
use hyphae_contracts::NATIVE_MCP_V2;
use hyphae_native_product::{
    BoundedSearchQuery, MAX_API_KEY_CREDENTIAL_BYTES, ObjectId, ProductError, ProductErrorCode,
    ProductLexicalBranch, ProductResponse, ProductSearchFilter, ProductSearchIngestBatch,
    ProductSearchRequest, ProductVector, ProductVectorBranch, SecurityCursor,
    SecurityPrincipalListRequest,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    exit::{CliFailure, error_json},
    native_client::{ApiKeyBuffer, authorization_denied, read_api_key_file},
    response_json,
};

const MCP_CONTRACT: &str = NATIVE_MCP_V2;
const MCP_CONTRACT_SCHEMA: &str = "hyphae-native-mcp-contract-v2";
const MCP_PROTOCOL: &str = "2025-06-18";
const TOOL_SCHEMA_VERSION: &str = "hyphae-native-mcp-tools-v4";
const TOOL_PAGE_SIZE: usize = 100;
const TOOL_NAMES: [&str; 11] = [
    "hyphae_native_capabilities",
    "hyphae_native_security_status",
    "hyphae_native_security_principals",
    "hyphae_native_search_lexical",
    "hyphae_native_search_collection",
    "hyphae_native_prove_search",
    "hyphae_native_verify_proof",
    "hyphae_native_search_ingest",
    "hyphae_native_memory_store",
    "hyphae_native_memory_recall",
    "hyphae_native_memory_forget",
];
/// Write-scoped tools, absent unless the operator opts in.
const WRITE_TOOL_NAMES: [&str; 3] = [
    "hyphae_native_search_ingest",
    "hyphae_native_memory_store",
    "hyphae_native_memory_forget",
];
/// Memory texts are bounded so one recall stays inside the message bound.
const MAX_MEMORY_TEXT_BYTES: usize = 4 * 1024;
/// Memory TTLs are bounded to ten years.
const MAX_MEMORY_TTL_SECONDS: u64 = 10 * 366 * 24 * 60 * 60;
const MAX_MEMORY_RECALL_LIMIT: usize = 64;
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 128;
const MAX_TOOL_CURSOR_BYTES: usize = 32;
const CHANNEL_CAPACITY: usize = 1;
const SERVER_BUSY: i32 = -32001;
const RESPONSE_TOO_LARGE: i32 = -32003;

#[derive(Clone, Copy)]
enum NativeTool {
    Capabilities,
    SecurityStatus,
    SecurityPrincipals,
    SearchLexical,
    SearchCollection,
    ProveSearch,
    VerifyProof,
    SearchIngest,
    MemoryStore,
    MemoryRecall,
    MemoryForget,
    ProfileMemoryStore(u128),
    ProfileMemoryRecall(u128),
    ProfileMemoryForget(u128),
    ProfileMemoryStatus(u128),
}

impl NativeTool {
    fn parse(name: &str, allow_ingest: bool) -> Option<Self> {
        match name {
            "hyphae_native_capabilities" => Some(Self::Capabilities),
            "hyphae_native_security_status" => Some(Self::SecurityStatus),
            "hyphae_native_security_principals" => Some(Self::SecurityPrincipals),
            "hyphae_native_search_lexical" => Some(Self::SearchLexical),
            "hyphae_native_search_collection" => Some(Self::SearchCollection),
            "hyphae_native_prove_search" => Some(Self::ProveSearch),
            "hyphae_native_verify_proof" => Some(Self::VerifyProof),
            "hyphae_native_search_ingest" if allow_ingest => Some(Self::SearchIngest),
            "hyphae_native_memory_store" if allow_ingest => Some(Self::MemoryStore),
            "hyphae_native_memory_recall" => Some(Self::MemoryRecall),
            "hyphae_native_memory_forget" if allow_ingest => Some(Self::MemoryForget),
            _ => None,
        }
    }
}

/// The Agent Memory four-tool registry: recall and status always; store
/// and forget only when the operator allows writes. The registry never
/// advertises a tool the profile cannot execute.
#[allow(clippy::too_many_lines)]
fn memory_registry(allow_write: bool) -> Result<ToolRegistry, CliFailure> {
    let error_branch = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["schema", "error"],
        "properties": {
            "schema": {"const": "hyphae-native-mcp-tool-error-v2"},
            "error": {"type": "object"},
        },
    });
    let tool = |name: &str, read_only: bool, description: &str, input: Value, success: Value| {
        json!({
            "name": name,
            "description": description,
            "annotations": {
                "readOnlyHint": read_only,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false,
            },
            "execution": {"taskSupport": "forbidden"},
            "inputSchema": input,
            "outputSchema": {"type": "object", "oneOf": [success, error_branch.clone()]},
        })
    };
    let memory_item = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "score", "project", "scope", "kind", "agent", "text", "expires_at_micros"],
        "properties": {
            "id": {"type": "string", "pattern": "^[0-9]+$"},
            "score": {"type": "number"},
            "project": {"type": ["string", "null"]},
            "scope": {"type": ["string", "null"]},
            "kind": {"type": ["string", "null"]},
            "agent": {"type": ["string", "null"]},
            "text": {"type": ["string", "null"]},
            "expires_at_micros": {"type": ["integer", "null"]},
        },
    });
    let mut tools = vec![
        tool(
            "hyphae_memory_recall",
            true,
            "Recall stored memories for one project (global memories included) by bounded lexical retrieval. Expired or forgotten memories never return; with prove the response carries the sealed proof, witness, and anchor for offline verification.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["project", "query"],
                "properties": {
                    "project": {"type": "string", "minLength": 1, "maxLength": 256},
                    "query": {"type": "string", "minLength": 1, "maxLength": 4096},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 64},
                    "kind": {"type": "string", "enum": MEMORY_KINDS},
                    "prove": {"type": "boolean"},
                },
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["memories", "expired_filtered", "proof"],
                "properties": {
                    "memories": {"type": "array", "maxItems": 64, "items": memory_item},
                    "expired_filtered": {"type": "integer", "minimum": 0},
                    "proof": {"type": ["object", "null"]},
                },
            }),
        ),
        tool(
            "hyphae_memory_status",
            true,
            "Redacted Agent Memory service status: collection identity, memory count, and contract versions. Never memory content, never credentials.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["status", "profile", "collection", "memories", "product_api_version"],
                "properties": {
                    "status": {"const": "ok"},
                    "profile": {"const": "memory"},
                    "collection": {"type": "string", "pattern": "^[0-9]+$"},
                    "memories": {},
                    "product_api_version": {"type": "integer"},
                },
            }),
        ),
    ];
    if allow_write {
        tools.push(tool(
            "hyphae_memory_store",
            false,
            "Store one bounded memory under its project: the identity derives from the project and text, an optional TTL bounds its life, and global scope shares it with every project. Listed and callable only when the adapter allows writes.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["project", "text"],
                "properties": {
                    "project": {"type": "string", "minLength": 1, "maxLength": 256},
                    "text": {"type": "string", "minLength": 1, "maxLength": 4096},
                    "kind": {"type": "string", "enum": MEMORY_KINDS},
                    "scope": {"type": "string", "enum": ["project", "global"]},
                    "agent": {"type": "string", "minLength": 1, "maxLength": 64},
                    "ttl": {"type": "integer", "minimum": 1, "maximum": 316_224_000},
                },
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["status", "id", "scope", "expires_at_micros"],
                "properties": {
                    "status": {"const": "stored"},
                    "id": {"type": "string", "pattern": "^[0-9]+$"},
                    "scope": {"type": "string", "enum": ["project", "global"]},
                    "expires_at_micros": {"type": ["integer", "null"]},
                },
            }),
        ));
        tools.push(tool(
            "hyphae_memory_forget",
            false,
            "Forget one memory permanently by its id after proving the caller names the owning project. Forgetting is idempotent and no recall can surface the memory again. Listed and callable only when the adapter allows writes.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["project", "id"],
                "properties": {
                    "project": {"type": "string", "minLength": 1, "maxLength": 256},
                    "id": {"type": "string", "pattern": "^[0-9]+$"},
                },
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["status", "id"],
                "properties": {
                    "status": {"const": "forgotten"},
                    "id": {"type": "string", "pattern": "^[0-9]+$"},
                },
            }),
        ));
    }
    let serialized = serde_json::to_string(&tools)?;
    Ok(ToolRegistry {
        protocol: MCP_PROTOCOL.to_owned(),
        schema_version: "hyphae-agent-memory-mcp-v1".to_owned(),
        schema_digest: blake3::hash(serialized.as_bytes()).to_hex().to_string(),
        page_size: TOOL_PAGE_SIZE,
        tools,
    })
}

struct ToolRegistry {
    protocol: String,
    schema_version: String,
    schema_digest: String,
    page_size: usize,
    tools: Vec<Value>,
}

impl ToolRegistry {
    /// Restricts the listed registry to the read-only subset.
    fn without_ingest(mut self) -> Self {
        self.tools.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_none_or(|name| !WRITE_TOOL_NAMES.contains(&name))
        });
        self
    }
}

struct Session {
    client: HyphaeClient,
    registry: ToolRegistry,
    allow_ingest: bool,
    profile: Profile,
    initialize_seen: bool,
    initialized: bool,
}

enum SessionAction {
    None,
    Response(Value),
    ToolCall(ToolCall),
}

struct ToolCall {
    id: Value,
    tool: NativeTool,
    arguments: Value,
}

struct ActiveToolCall {
    id: Value,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<Value>,
}

enum InputFrame {
    Message(Value),
    ParseError,
    End,
    Error(io::Error),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsListParams {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default, rename = "_meta")]
    _meta: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsCallParams {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
    #[serde(default, rename = "_meta")]
    _meta: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrincipalListInput {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_security_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LexicalSearchInput {
    index: u64,
    kind: String,
    query: String,
    #[serde(default = "default_fuzzy_distance")]
    max_distance: u8,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

/// Branch-combination method selector accepted by the search tools.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FusionMethodInputValue {
    WeightedScore,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectionSearchInput {
    pub(crate) collection: u64,
    #[serde(default)]
    lexical: Option<LexicalBranchInput>,
    #[serde(default)]
    vectors: Vec<VectorBranchInput>,
    #[serde(default)]
    filter: Option<Value>,
    #[serde(default)]
    sort: Vec<Value>,
    #[serde(default)]
    facets: Vec<Value>,
    #[serde(default)]
    aggregations: Vec<Value>,
    #[serde(default = "default_search_limit")]
    limit: usize,
    #[serde(default)]
    fusion: Option<FusionMethodInputValue>,
    #[serde(default)]
    parent_dedupe: Option<ParentDedupeInput>,
}

/// First-k-per-parent deduplication accepted by the search tools.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParentDedupeInput {
    field: String,
    first_k: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LexicalBranchInput {
    query: String,
    #[serde(default = "default_search_limit")]
    candidate_limit: usize,
    #[serde(default = "default_branch_weight")]
    weight: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VectorBranchInput {
    target: String,
    values: Vec<f32>,
    #[serde(default = "default_search_limit")]
    candidate_limit: usize,
    #[serde(default = "default_branch_weight")]
    weight: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
struct VerifyProofInput {
    proof_hex: String,
    witness_hex: String,
    anchor_hex: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchIngestInput {
    collection: u64,
    idempotency_id: u64,
    documents: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryStoreInput {
    collection: u64,
    text: String,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryRecallInput {
    collection: u64,
    query: String,
    #[serde(default = "default_memory_recall_limit")]
    limit: usize,
    #[serde(default)]
    prove: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryForgetInput {
    collection: u64,
    id: String,
}

fn default_memory_recall_limit() -> usize {
    8
}

fn default_search_limit() -> usize {
    10
}

fn default_branch_weight() -> u32 {
    1
}

fn default_fuzzy_distance() -> u8 {
    1
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelledParams {
    request_id: Value,
    #[serde(default, rename = "reason")]
    _reason: Option<String>,
}

/// Runs one newline-delimited JSON-RPC 2.0 MCP session over stdio.
///
/// In stdin credential mode, the first bounded line is the credential and all
/// following lines are MCP messages. The credential line is never echoed.
///
/// # Errors
///
/// Returns a typed CLI failure for missing or invalid credentials, client
/// construction, fatal standard-I/O, or response serialization failures.
#[allow(clippy::too_many_lines)]
/// Tool surface selection for one adapter process.
#[derive(Clone, Copy)]
pub(crate) enum Profile {
    /// The full native tool registry.
    Full,
    /// The Agent Memory four-tool surface over one fixed collection.
    Memory {
        /// Expose the write-scoped store and forget tools.
        allow_write: bool,
        /// Agent Memory collection identity.
        collection: u128,
    },
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run(
    base_url: &str,
    api_key_file: Option<&Path>,
    api_key_stdin: bool,
    allow_ingest: bool,
    profile: Profile,
) -> Result<(), CliFailure> {
    let mut input = BufReader::new(io::stdin());
    let credential = read_mcp_credential(api_key_file, api_key_stdin, &mut input)?;
    let credential_text = credential.credential()?;
    let transport = HttpTransport::new(base_url)
        .and_then(|transport| transport.bearer_token(credential_text))
        .and_then(|transport| transport.response_bytes(MAX_MESSAGE_BYTES))
        .map_err(startup_client_error)?;
    drop(credential);

    let registry = match profile {
        Profile::Full => {
            let registry = ToolRegistry::load()?;
            if allow_ingest {
                registry
            } else {
                registry.without_ingest()
            }
        }
        Profile::Memory { allow_write, .. } => memory_registry(allow_write)?,
    };
    let mut session = Session {
        client: HyphaeClient::new(transport),
        registry,
        allow_ingest,
        profile,
        initialize_seen: false,
        initialized: false,
    };
    let (input_sender, mut input_receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    let _input_thread = std::thread::spawn(move || {
        read_input(input, &input_sender);
    });
    let (output_sender, output_receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    let (output_done_sender, mut output_done_receiver) = tokio::sync::mpsc::channel(1);
    let _output_thread = std::thread::spawn(move || {
        let result = write_output(BufWriter::new(io::stdout()), output_receiver);
        let _ignored = output_done_sender.blocking_send(result);
    });
    let mut active: Option<ActiveToolCall> = None;

    loop {
        let frame = if let Some(call) = active.as_mut() {
            tokio::select! {
                frame = input_receiver.recv() => frame,
                output = output_done_receiver.recv() => {
                    return Err(output.unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::BrokenPipe, "MCP output closed"))).map_or_else(CliFailure::from, |()| CliFailure::io()));
                }
                completed = &mut call.task => {
                    let id = call.id.clone();
                    let response = completed.unwrap_or_else(|_| {
                        rpc_error(&id, -32603, "Internal error")
                    });
                    active = None;
                    send_response(&output_sender, &response)?;
                    continue;
                }
            }
        } else {
            tokio::select! {
                frame = input_receiver.recv() => frame,
                output = output_done_receiver.recv() => {
                    return Err(output.unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::BrokenPipe, "MCP output closed"))).map_or_else(CliFailure::from, |()| CliFailure::io()));
                }
            }
        };
        let Some(frame) = frame else {
            break;
        };
        match frame {
            InputFrame::Message(message) => {
                if cancel_active_call(&message, active.as_ref()) {
                    continue;
                }
                match session.handle(&message) {
                    SessionAction::None => {}
                    SessionAction::Response(response) => {
                        send_response(&output_sender, &response)?;
                    }
                    SessionAction::ToolCall(call) => {
                        if active.is_some() {
                            send_response(&output_sender, &saturated(&call.id))?;
                        } else {
                            active = Some(start_tool_call(
                                session.client.clone(),
                                &session.registry,
                                call,
                            ));
                        }
                    }
                }
            }
            InputFrame::ParseError => {
                send_response(
                    &output_sender,
                    &rpc_error(&Value::Null, -32700, "Parse error"),
                )?;
            }
            InputFrame::End => break,
            InputFrame::Error(error) => return Err(error.into()),
        }
    }

    if let Some(call) = active {
        call.cancellation.cancel();
        let mut task = call.task;
        if tokio::time::timeout(std::time::Duration::from_secs(1), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
    drop(output_sender);
    match tokio::time::timeout(
        std::time::Duration::from_secs(1),
        output_done_receiver.recv(),
    )
    .await
    {
        Ok(Some(result)) => result?,
        Ok(None) | Err(_) => return Err(CliFailure::io()),
    }
    Ok(())
}

fn read_input<R: BufRead>(mut input: R, sender: &tokio::sync::mpsc::Sender<InputFrame>) {
    loop {
        let frame = match read_bounded_line(&mut input) {
            Ok(Some(line)) if line.iter().all(u8::is_ascii_whitespace) => continue,
            Ok(Some(line)) => serde_json::from_slice::<Value>(&line)
                .map_or(InputFrame::ParseError, InputFrame::Message),
            Ok(None) => InputFrame::End,
            Err(error) => InputFrame::Error(error),
        };
        let terminal = matches!(&frame, InputFrame::End | InputFrame::Error(_));
        let mut pending = Some(frame);
        loop {
            let Some(frame) = pending.take() else {
                return;
            };
            match sender.try_send(frame) {
                Ok(()) => break,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                Err(tokio::sync::mpsc::error::TrySendError::Full(frame)) => {
                    pending = Some(frame);
                    std::thread::park_timeout(std::time::Duration::from_millis(10));
                }
            }
        }
        if terminal {
            return;
        }
    }
}

fn write_output<W: Write>(
    mut output: BufWriter<W>,
    mut receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> io::Result<()> {
    while let Some(message) = receiver.blocking_recv() {
        output.write_all(&message)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    output.flush()
}

fn send_response(
    sender: &tokio::sync::mpsc::Sender<Vec<u8>>,
    response: &Value,
) -> Result<(), CliFailure> {
    let mut encoded = serde_json::to_vec(&response)?;
    if encoded.len() > MAX_MESSAGE_BYTES {
        let id = response.get("id").cloned().unwrap_or(Value::Null);
        encoded = serde_json::to_vec(&rpc_error(
            &id,
            RESPONSE_TOO_LARGE,
            "Response exceeds the fixed message bound",
        ))?;
    }
    sender.try_send(encoded).map_err(|_| CliFailure::io())
}

impl ToolRegistry {
    fn load() -> Result<Self, CliFailure> {
        Self::from_contract(MCP_CONTRACT)
    }

    fn from_contract(source: &str) -> Result<Self, CliFailure> {
        let contract: Value = serde_json::from_str(source)?;
        let protocol = required_string(&contract, "mcp_protocol")?.to_owned();
        let schema_version = required_string(&contract, "tool_schema_version")?.to_owned();
        let expected_limits = json!({
            "input_bytes": MAX_MESSAGE_BYTES,
            "output_bytes": MAX_MESSAGE_BYTES,
            "active_tool_calls": 1,
            "pending_responses": 1,
        });
        let expected_cancellation = json!({
            "method": "notifications/cancelled",
            "idempotent": true,
        });
        let page_size = contract
            .get("tool_page_size")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value == TOOL_PAGE_SIZE)
            .ok_or_else(CliFailure::invalid)?;
        let tools = contract
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(CliFailure::invalid)?;
        if required_string(&contract, "schema")? != MCP_CONTRACT_SCHEMA
            || protocol != MCP_PROTOCOL
            || schema_version != TOOL_SCHEMA_VERSION
            || contract.get("resource_limits") != Some(&expected_limits)
            || contract.get("cancellation") != Some(&expected_cancellation)
            || tools.len() != TOOL_NAMES.len()
            || tools
                .iter()
                .zip(TOOL_NAMES)
                .any(|(tool, expected_name)| !valid_tool_contract(tool, expected_name))
        {
            return Err(CliFailure::invalid());
        }
        Ok(Self {
            protocol,
            schema_version,
            schema_digest: blake3::hash(MCP_CONTRACT.as_bytes()).to_hex().to_string(),
            page_size,
            tools,
        })
    }

    fn metadata(&self) -> Value {
        json!({
            "hyphaeToolSchemaVersion": self.schema_version,
            "hyphaeToolSchemaDigest": self.schema_digest,
        })
    }

    fn list(&self, params: &Value) -> Result<Value, ()> {
        let params = serde_json::from_value::<ToolsListParams>(params.clone()).map_err(|_| ())?;
        let offset = match params.cursor.as_deref() {
            None => 0,
            Some(cursor) => decode_tool_cursor(cursor).ok_or(())?,
        };
        if offset >= self.tools.len() || (offset != 0 && offset % self.page_size != 0) {
            return Err(());
        }
        let end = offset.saturating_add(self.page_size).min(self.tools.len());
        let mut page = json!({
            "tools": self.tools[offset..end],
            "_meta": self.metadata(),
        });
        if end < self.tools.len() {
            page["nextCursor"] = Value::String(encode_tool_cursor(end));
        }
        Ok(page)
    }
}

fn valid_tool_contract(tool: &Value, expected_name: &str) -> bool {
    let Some(tool) = tool.as_object() else {
        return false;
    };
    let expected_annotations = json!({
        "readOnlyHint": !WRITE_TOOL_NAMES.contains(&expected_name),
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false,
    });
    if tool
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>()
        != [
            "annotations",
            "description",
            "execution",
            "inputSchema",
            "name",
            "outputSchema",
        ]
        .into_iter()
        .collect()
        || tool.get("name").and_then(Value::as_str) != Some(expected_name)
        || tool.get("annotations") != Some(&expected_annotations)
        || tool.get("execution") != Some(&json!({ "taskSupport": "forbidden" }))
        || tool
            .get("inputSchema")
            .and_then(Value::as_object)
            .and_then(|schema| schema.get("additionalProperties"))
            != Some(&Value::Bool(false))
    {
        return false;
    }

    let Some(output) = tool.get("outputSchema").and_then(Value::as_object) else {
        return false;
    };
    let Some(branches) = output.get("oneOf").and_then(Value::as_array) else {
        return false;
    };
    output.len() == 2
        && output.get("type").and_then(Value::as_str) == Some("object")
        && branches.len() == 2
        && branches[0].pointer("/additionalProperties") == Some(&Value::Bool(false))
        && valid_error_schema(&branches[1])
        && schema_is_redacted(tool.get("inputSchema").unwrap_or(&Value::Null))
        && schema_is_redacted(tool.get("outputSchema").unwrap_or(&Value::Null))
}

fn valid_error_schema(schema: &Value) -> bool {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return false;
    };
    let Some(error) = properties.get("error") else {
        return false;
    };
    let required = json!([
        "code",
        "category",
        "message",
        "retry",
        "transaction_state",
        "request_id",
        "trace_id",
        "object_id",
        "transaction_id",
    ]);
    schema.get("additionalProperties") == Some(&Value::Bool(false))
        && schema.get("required") == Some(&json!(["schema", "error"]))
        && properties.len() == 2
        && properties
            .get("schema")
            .and_then(|schema| schema.get("const"))
            .and_then(Value::as_str)
            == Some("hyphae-native-mcp-tool-error-v2")
        && error.get("additionalProperties") == Some(&Value::Bool(false))
        && error.get("required") == Some(&required)
        && error
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.len() == 9)
}

fn schema_is_redacted(value: &Value) -> bool {
    const FORBIDDEN: [&str; 6] = [
        "api_key",
        "credential",
        "credential_hash",
        "key_hash",
        "key_material",
        "secret",
    ];
    match value {
        Value::Array(values) => values.iter().all(schema_is_redacted),
        Value::Object(values) => values
            .iter()
            .all(|(key, value)| !FORBIDDEN.contains(&key.as_str()) && schema_is_redacted(value)),
        _ => true,
    }
}

impl Session {
    fn handle(&mut self, message: &Value) -> SessionAction {
        let Some(object) = message.as_object() else {
            return SessionAction::Response(rpc_error(&Value::Null, -32600, "Invalid Request"));
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return SessionAction::Response(rpc_error(
                &request_id(object),
                -32600,
                "Invalid Request",
            ));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return SessionAction::Response(rpc_error(
                &request_id(object),
                -32600,
                "Invalid Request",
            ));
        };
        let id = object.get("id").cloned();
        if id
            .as_ref()
            .is_some_and(|value| !value.is_string() && !value.is_i64() && !value.is_u64())
        {
            return SessionAction::Response(rpc_error(&Value::Null, -32600, "Invalid Request"));
        }
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        if !params.is_object() {
            return id.map_or(SessionAction::None, |id| {
                SessionAction::Response(rpc_error(&id, -32602, "Invalid params"))
            });
        }
        if id.is_none() {
            self.handle_notification(method);
            return SessionAction::None;
        }
        let id = id.unwrap_or(Value::Null);
        match method {
            "initialize" => SessionAction::Response(self.initialize(&id, &params)),
            "ping" => SessionAction::Response(rpc_result(&id, &json!({}))),
            _ if !self.initialized => {
                SessionAction::Response(rpc_error(&id, -32002, "Server not initialized"))
            }
            "tools/list" => SessionAction::Response(self.list_tools(&id, &params)),
            "tools/call" => self.prepare_tool_call(id, &params),
            _ => SessionAction::Response(rpc_error(&id, -32601, "Method not found")),
        }
    }

    fn handle_notification(&mut self, method: &str) {
        if method == "notifications/initialized" && self.initialize_seen {
            self.initialized = true;
        }
    }

    fn initialize(&mut self, id: &Value, params: &Value) -> Value {
        let valid_keys = ["protocolVersion", "capabilities", "clientInfo", "_meta"];
        let valid_meta = params.get("_meta").is_none_or(Value::is_object);
        if self.initialize_seen
            || params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .is_none()
            || !params.get("capabilities").is_some_and(Value::is_object)
            || !params.get("clientInfo").is_some_and(Value::is_object)
            || !valid_meta
            || params
                .as_object()
                .is_none_or(|params| params.keys().any(|key| !valid_keys.contains(&key.as_str())))
        {
            return rpc_error(id, -32602, "Invalid initialize params");
        }
        self.initialize_seen = true;
        rpc_result(
            id,
            &json!({
                "protocolVersion": self.registry.protocol,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "hyphae-native",
                    "title": "Hyphae Native managed read-only tools",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Tool arguments are data only: they cannot grant roles, permissions, credentials, or a different authority. API keys are never returned.",
                "_meta": self.registry.metadata(),
            }),
        )
    }

    fn list_tools(&self, id: &Value, params: &Value) -> Value {
        self.registry.list(params).map_or_else(
            |()| rpc_error(id, -32602, "Invalid params"),
            |page| rpc_result(id, &page),
        )
    }

    fn prepare_tool_call(&self, id: Value, params: &Value) -> SessionAction {
        let params = match serde_json::from_value::<ToolsCallParams>(params.clone()) {
            Ok(params) if params.arguments.is_object() => params,
            _ => {
                return SessionAction::Response(rpc_error(&id, -32602, "Invalid params"));
            }
        };
        let tool = match self.profile {
            Profile::Full => NativeTool::parse(&params.name, self.allow_ingest),
            Profile::Memory {
                allow_write,
                collection,
            } => match params.name.as_str() {
                "hyphae_memory_store" if allow_write => {
                    Some(NativeTool::ProfileMemoryStore(collection))
                }
                "hyphae_memory_recall" => Some(NativeTool::ProfileMemoryRecall(collection)),
                "hyphae_memory_forget" if allow_write => {
                    Some(NativeTool::ProfileMemoryForget(collection))
                }
                "hyphae_memory_status" => Some(NativeTool::ProfileMemoryStatus(collection)),
                _ => None,
            },
        };
        let Some(tool) = tool else {
            return SessionAction::Response(rpc_error(&id, -32602, "Unknown tool"));
        };
        SessionAction::ToolCall(ToolCall {
            id,
            tool,
            arguments: params.arguments,
        })
    }
}

fn start_tool_call(
    client: HyphaeClient,
    registry: &ToolRegistry,
    call: ToolCall,
) -> ActiveToolCall {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let metadata = registry.metadata();
    let id = call.id.clone();
    let task_id = id.clone();
    let task = tokio::spawn(async move {
        let result = execute_tool(client, call.tool, call.arguments, task_cancellation).await;
        match result {
            Ok(value) => rpc_result(&task_id, &tool_success(&value, &metadata)),
            Err(error) => rpc_result(&task_id, &tool_error(&error, &metadata)),
        }
    });
    ActiveToolCall {
        id,
        cancellation,
        task,
    }
}

#[allow(clippy::too_many_lines)]
async fn execute_tool(
    client: HyphaeClient,
    tool: NativeTool,
    arguments: Value,
    cancellation: CancellationToken,
) -> Result<Value, Box<ProductError>> {
    let mut options = RequestOptions {
        cancellation,
        ..RequestOptions::default()
    };
    options.limits.max_request_bytes = MAX_MESSAGE_BYTES;
    options.limits.max_response_bytes = MAX_MESSAGE_BYTES;
    let response = match tool {
        NativeTool::Capabilities => {
            strict_input::<EmptyInput>(arguments)?;
            client.capabilities(options).await
        }
        NativeTool::SecurityStatus => {
            strict_input::<EmptyInput>(arguments)?;
            client.security_status(options).await
        }
        NativeTool::SecurityPrincipals => {
            let input = strict_input::<PrincipalListInput>(arguments)?;
            let cursor = input
                .cursor
                .as_deref()
                .map(parse_security_cursor)
                .transpose()?;
            let request = SecurityPrincipalListRequest::new(cursor, input.limit)
                .map_err(|_| invalid_request())?;
            client.security_principal_list(request, options).await
        }
        NativeTool::SearchLexical => {
            let input = strict_input::<LexicalSearchInput>(arguments)?;
            let index = ObjectId::new(u128::from(input.index)).map_err(|_| invalid_request())?;
            let query = match input.kind.as_str() {
                "term" => BoundedSearchQuery::Term(input.query),
                "phrase" => BoundedSearchQuery::Phrase(input.query),
                "prefix" => BoundedSearchQuery::Prefix(input.query),
                "fuzzy" => BoundedSearchQuery::Fuzzy {
                    term: input.query,
                    max_distance: input.max_distance,
                },
                _ => return Err(invalid_request()),
            };
            client.search(index, query, input.limit, options).await
        }
        NativeTool::SearchCollection => {
            let input = strict_input::<CollectionSearchInput>(arguments)?;
            let collection =
                ObjectId::new(u128::from(input.collection)).map_err(|_| invalid_request())?;
            let request = collection_search_request(input)?;
            client.search_collection(collection, request, options).await
        }
        NativeTool::ProveSearch => {
            let input = strict_input::<CollectionSearchInput>(arguments)?;
            let collection =
                ObjectId::new(u128::from(input.collection)).map_err(|_| invalid_request())?;
            let request = collection_search_request(input)?;
            client
                .prove(
                    hyphae_native_product::ProductOperation::SearchCollection {
                        collection,
                        request,
                    },
                    hyphae_native_product::proof::NativeProofGenerationLimits::default(),
                    options,
                )
                .await
        }
        NativeTool::VerifyProof => {
            let input = strict_input::<VerifyProofInput>(arguments)?;
            return verify_proof_locally(&input);
        }
        NativeTool::SearchIngest => {
            let input = strict_input::<SearchIngestInput>(arguments)?;
            if input.idempotency_id == 0 {
                return Err(invalid_request());
            }
            let collection =
                ObjectId::new(u128::from(input.collection)).map_err(|_| invalid_request())?;
            let documents = input
                .documents
                .into_iter()
                .map(|value| {
                    serde_json::from_value(value)
                        .map_err(|_| invalid_request())
                        .and_then(|document| {
                            crate::product_document(document).map_err(|_| invalid_request())
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let batch = ProductSearchIngestBatch {
                idempotency_id: u128::from(input.idempotency_id),
                documents,
            };
            client.search_ingest(collection, batch, options).await
        }
        NativeTool::MemoryStore => {
            let input = strict_input::<MemoryStoreInput>(arguments)?;
            return memory_store(&client, input, options).await;
        }
        NativeTool::MemoryRecall => {
            let input = strict_input::<MemoryRecallInput>(arguments)?;
            return memory_recall(&client, input, options).await;
        }
        NativeTool::MemoryForget => {
            let input = strict_input::<MemoryForgetInput>(arguments)?;
            return memory_forget(&client, input, options).await;
        }
        NativeTool::ProfileMemoryStore(collection) => {
            let input = strict_input::<ProfileStoreInput>(arguments)?;
            return profile_memory_store(&client, collection, input, options).await;
        }
        NativeTool::ProfileMemoryRecall(collection) => {
            let input = strict_input::<ProfileRecallInput>(arguments)?;
            return profile_memory_recall(&client, collection, input, options).await;
        }
        NativeTool::ProfileMemoryForget(collection) => {
            let input = strict_input::<ProfileForgetInput>(arguments)?;
            return profile_memory_forget(&client, collection, input, options).await;
        }
        NativeTool::ProfileMemoryStatus(collection) => {
            strict_input::<EmptyInput>(arguments)?;
            return profile_memory_status(&client, collection, options).await;
        }
    }
    .map_err(normalize_client_error)?;
    response_for(tool, response)
}

/// Content-derived memory identity: the first sixteen digest bytes.
fn memory_identity(text: &str) -> u128 {
    let digest = blake3::hash(text.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    let identity = u128::from_le_bytes(bytes);
    if identity == 0 { 1 } else { identity }
}

/// Lifecycle key owning one memory's recallability and TTL.
fn memory_key(collection: u128, identity: u128) -> Vec<u8> {
    let mut key = b"hyphae-memory/".to_vec();
    key.extend_from_slice(&collection.to_le_bytes());
    key.extend_from_slice(&identity.to_le_bytes());
    key
}

/// Stores one bounded memory: the text ingests into the collection under
/// its content-derived identity, and a scalar lifecycle key carries the
/// text and the optional TTL. A memory is recallable exactly while its
/// lifecycle key lives.
async fn memory_store(
    client: &HyphaeClient,
    input: MemoryStoreInput,
    options: RequestOptions,
) -> Result<Value, Box<ProductError>> {
    if input.text.is_empty()
        || input.text.len() > MAX_MEMORY_TEXT_BYTES
        || input
            .ttl_seconds
            .is_some_and(|ttl| ttl == 0 || ttl > MAX_MEMORY_TTL_SECONDS)
    {
        return Err(invalid_request());
    }
    let identity = memory_identity(&input.text);
    let collection = ObjectId::new(u128::from(input.collection)).map_err(|_| invalid_request())?;
    let object_id = ObjectId::new(identity).map_err(|_| invalid_request())?;
    let batch = ProductSearchIngestBatch {
        idempotency_id: identity,
        documents: vec![hyphae_native_product::ProductDocument {
            object_id,
            text: input.text.clone(),
            doc_values: std::collections::BTreeMap::new(),
            vectors: std::collections::BTreeMap::new(),
        }],
    };
    client
        .search_ingest(collection, batch, options.clone())
        .await
        .map_err(normalize_client_error)?;
    let expires_at_micros = input.ttl_seconds.map(|ttl| {
        crate::native::logical_time_micros()
            .saturating_add(i64::try_from(ttl.saturating_mul(1_000_000)).unwrap_or(i64::MAX))
    });
    client
        .structure_set(
            memory_key(collection.get(), identity),
            input.text.into_bytes(),
            expires_at_micros,
            options,
        )
        .await
        .map_err(normalize_client_error)?;
    Ok(json!({
        "status": "stored",
        "id": identity.to_string(),
        "expires_at_micros": expires_at_micros,
    }))
}

/// Recalls memories by bounded lexical retrieval, keeping only hits whose
/// lifecycle key still lives — expired or forgotten memories never return.
/// With `prove`, the retrieval itself is sealed and the artifacts ride the
/// response; the lifecycle filter is applied after the proved search.
async fn memory_recall(
    client: &HyphaeClient,
    input: MemoryRecallInput,
    options: RequestOptions,
) -> Result<Value, Box<ProductError>> {
    if input.query.is_empty() || !(1..=MAX_MEMORY_RECALL_LIMIT).contains(&input.limit) {
        return Err(invalid_request());
    }
    let collection = ObjectId::new(u128::from(input.collection)).map_err(|_| invalid_request())?;
    let request = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: input.query,
            candidate_limit: 1_000,
            weight: 1,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        aggregations: Vec::new(),
        limit: input.limit,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
    };
    let (result, proof) = if input.prove {
        let response = client
            .prove(
                hyphae_native_product::ProductOperation::SearchCollection {
                    collection,
                    request,
                },
                hyphae_native_product::proof::NativeProofGenerationLimits::default(),
                options.clone(),
            )
            .await
            .map_err(normalize_client_error)?;
        let ProductResponse::Proven { response, artifact } = response else {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::Internal,
            )));
        };
        let ProductResponse::IntegratedSearch(result) = *response else {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::Internal,
            )));
        };
        (
            result,
            Some(json!({
                "proof_hex": crate::encode_hex(&artifact.proof_bytes),
                "witness_hex": crate::encode_hex(&artifact.witness_bytes),
                "anchor_hex": crate::encode_hex(&artifact.trusted_anchor.digest()),
            })),
        )
    } else {
        let response = client
            .search_collection(collection, request, options.clone())
            .await
            .map_err(normalize_client_error)?;
        let ProductResponse::IntegratedSearch(result) = response else {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::Internal,
            )));
        };
        (result, None)
    };
    let mut memories = Vec::new();
    let mut filtered = 0_usize;
    for hit in &result.hits {
        let lifecycle = client
            .structure_get(
                memory_key(collection.get(), hit.object_id.get()),
                options.clone(),
            )
            .await
            .map_err(normalize_client_error)?;
        match lifecycle {
            ProductResponse::StructureValue(Some(bytes)) => {
                memories.push(json!({
                    "id": hit.object_id.get().to_string(),
                    "score": hit.score,
                    "text": String::from_utf8_lossy(&bytes),
                }));
            }
            ProductResponse::StructureValue(None) => filtered += 1,
            _ => {
                return Err(Box::new(ProductError::from_code(
                    ProductErrorCode::Internal,
                )));
            }
        }
    }
    Ok(json!({
        "memories": memories,
        "expired_filtered": filtered,
        "proof": proof,
    }))
}

const MEMORY_KINDS: [&str; 5] = ["decision", "command", "constraint", "fact", "note"];
const GLOBAL_PROJECT: &str = "_global";
const MAX_PROJECT_BYTES: usize = 256;
const MAX_MEMORY_TTL_SECONDS_PROFILE: u64 = 10 * 366 * 24 * 60 * 60;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileStoreInput {
    project: String,
    text: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    ttl: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileRecallInput {
    project: String,
    query: String,
    #[serde(default = "default_memory_recall_limit")]
    limit: usize,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    prove: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileForgetInput {
    project: String,
    id: String,
}

fn valid_project(project: &str) -> bool {
    !project.is_empty() && project.len() <= MAX_PROJECT_BYTES
}

/// Content-derived envelope identity: project and text fix the memory.
fn envelope_identity(project: &str, text: &str) -> u128 {
    let digest = blake3::Hasher::new()
        .update(b"hyphae-agent-memory")
        .update(&[0])
        .update(project.as_bytes())
        .update(&[0])
        .update(text.as_bytes())
        .finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    match u128::from_le_bytes(bytes) {
        0 => 1,
        identity => identity,
    }
}

/// Stores one bounded envelope memory under project isolation.
async fn profile_memory_store(
    client: &HyphaeClient,
    collection: u128,
    input: ProfileStoreInput,
    options: RequestOptions,
) -> Result<Value, Box<ProductError>> {
    let kind = input.kind.as_deref().unwrap_or("note");
    let scope = input.scope.as_deref().unwrap_or("project");
    if !valid_project(&input.project)
        || input.project == GLOBAL_PROJECT
        || input.text.is_empty()
        || input.text.len() > MAX_MEMORY_TEXT_BYTES
        || !MEMORY_KINDS.contains(&kind)
        || !matches!(scope, "project" | "global")
        || input
            .agent
            .as_ref()
            .is_some_and(|agent| agent.is_empty() || agent.len() > 64)
        || input
            .ttl
            .is_some_and(|ttl| ttl == 0 || ttl > MAX_MEMORY_TTL_SECONDS_PROFILE)
    {
        return Err(invalid_request());
    }
    let effective_project = if scope == "global" {
        GLOBAL_PROJECT
    } else {
        input.project.as_str()
    };
    let identity = envelope_identity(effective_project, &input.text);
    let collection = ObjectId::new(collection).map_err(|_| invalid_request())?;
    let object_id = ObjectId::new(identity).map_err(|_| invalid_request())?;
    let mut doc_values = std::collections::BTreeMap::new();
    doc_values.insert(
        "project".to_owned(),
        hyphae_native_product::ProductDocValue::String(effective_project.to_owned()),
    );
    doc_values.insert(
        "kind".to_owned(),
        hyphae_native_product::ProductDocValue::String(kind.to_owned()),
    );
    let batch = ProductSearchIngestBatch {
        idempotency_id: identity,
        documents: vec![hyphae_native_product::ProductDocument {
            object_id,
            text: input.text.clone(),
            doc_values,
            vectors: std::collections::BTreeMap::new(),
        }],
    };
    client
        .search_ingest(collection, batch, options.clone())
        .await
        .map_err(normalize_client_error)?;
    let expires_at_micros = input.ttl.map(|ttl| {
        crate::native::logical_time_micros()
            .saturating_add(i64::try_from(ttl.saturating_mul(1_000_000)).unwrap_or(i64::MAX))
    });
    let envelope = json!({
        "project": input.project,
        "scope": scope,
        "kind": kind,
        "agent": input.agent,
        "text": input.text,
        "expires_at_micros": expires_at_micros,
    });
    client
        .structure_set(
            memory_key(collection.get(), identity),
            serde_json::to_vec(&envelope).map_err(|_| invalid_request())?,
            expires_at_micros,
            options,
        )
        .await
        .map_err(normalize_client_error)?;
    Ok(json!({
        "status": "stored",
        "id": identity.to_string(),
        "scope": scope,
        "expires_at_micros": expires_at_micros,
    }))
}

/// Recalls memories for one project (plus global memories), keeping only
/// hits whose lifecycle envelope still lives.
#[allow(clippy::too_many_lines)]
async fn profile_memory_recall(
    client: &HyphaeClient,
    collection: u128,
    input: ProfileRecallInput,
    options: RequestOptions,
) -> Result<Value, Box<ProductError>> {
    if !valid_project(&input.project)
        || input.query.is_empty()
        || !(1..=MAX_MEMORY_RECALL_LIMIT).contains(&input.limit)
        || input
            .kind
            .as_deref()
            .is_some_and(|kind| !MEMORY_KINDS.contains(&kind))
    {
        return Err(invalid_request());
    }
    let collection = ObjectId::new(collection).map_err(|_| invalid_request())?;
    let mut clauses = vec![ProductSearchFilter::In {
        field: "project".to_owned(),
        values: vec![
            hyphae_native_product::ProductDocValue::String(input.project.clone()),
            hyphae_native_product::ProductDocValue::String(GLOBAL_PROJECT.to_owned()),
        ],
    }];
    if let Some(kind) = &input.kind {
        clauses.push(ProductSearchFilter::Compare {
            field: "kind".to_owned(),
            operator: hyphae_native_product::ProductSearchOperator::Equal,
            value: hyphae_native_product::ProductDocValue::String(kind.clone()),
        });
    }
    let request = ProductSearchRequest {
        lexical: Some(ProductLexicalBranch {
            query: input.query,
            candidate_limit: 1_000,
            weight: 1,
        }),
        vectors: Vec::new(),
        filter: ProductSearchFilter::All(clauses),
        sort: Vec::new(),
        facets: Vec::new(),
        aggregations: Vec::new(),
        limit: input.limit,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
    };
    let (result, proof) = if input.prove {
        let response = client
            .prove(
                hyphae_native_product::ProductOperation::SearchCollection {
                    collection,
                    request,
                },
                hyphae_native_product::proof::NativeProofGenerationLimits::default(),
                options.clone(),
            )
            .await
            .map_err(normalize_client_error)?;
        let ProductResponse::Proven { response, artifact } = response else {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::Internal,
            )));
        };
        let ProductResponse::IntegratedSearch(result) = *response else {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::Internal,
            )));
        };
        (
            result,
            Some(json!({
                "proof_hex": crate::encode_hex(&artifact.proof_bytes),
                "witness_hex": crate::encode_hex(&artifact.witness_bytes),
                "anchor_hex": crate::encode_hex(&artifact.trusted_anchor.digest()),
            })),
        )
    } else {
        let response = client
            .search_collection(collection, request, options.clone())
            .await
            .map_err(normalize_client_error)?;
        let ProductResponse::IntegratedSearch(result) = response else {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::Internal,
            )));
        };
        (result, None)
    };
    let mut memories = Vec::new();
    let mut expired = 0_usize;
    for hit in &result.hits {
        let lifecycle = client
            .structure_get(
                memory_key(collection.get(), hit.object_id.get()),
                options.clone(),
            )
            .await
            .map_err(normalize_client_error)?;
        match lifecycle {
            ProductResponse::StructureValue(Some(bytes)) => {
                let Ok(envelope) = serde_json::from_slice::<Value>(&bytes) else {
                    expired += 1;
                    continue;
                };
                memories.push(json!({
                    "id": hit.object_id.get().to_string(),
                    "score": hit.score,
                    "project": envelope.get("project"),
                    "scope": envelope.get("scope"),
                    "kind": envelope.get("kind"),
                    "agent": envelope.get("agent"),
                    "text": envelope.get("text"),
                    "expires_at_micros": envelope.get("expires_at_micros"),
                }));
            }
            ProductResponse::StructureValue(None) => expired += 1,
            _ => {
                return Err(Box::new(ProductError::from_code(
                    ProductErrorCode::Internal,
                )));
            }
        }
    }
    Ok(json!({
        "memories": memories,
        "expired_filtered": expired,
        "proof": proof,
    }))
}

/// Forgets one envelope memory permanently after proving the caller names
/// the owning project.
async fn profile_memory_forget(
    client: &HyphaeClient,
    collection: u128,
    input: ProfileForgetInput,
    options: RequestOptions,
) -> Result<Value, Box<ProductError>> {
    if !valid_project(&input.project) {
        return Err(invalid_request());
    }
    let identity: u128 = input.id.parse().map_err(|_| invalid_request())?;
    let collection_id = ObjectId::new(collection).map_err(|_| invalid_request())?;
    let lifecycle = client
        .structure_get(memory_key(collection, identity), options.clone())
        .await
        .map_err(normalize_client_error)?;
    let ProductResponse::StructureValue(Some(bytes)) = lifecycle else {
        // Forgetting an absent memory is idempotent.
        return Ok(json!({"status": "forgotten", "id": identity.to_string()}));
    };
    let Ok(envelope) = serde_json::from_slice::<Value>(&bytes) else {
        // A tombstoned lifecycle may stay briefly visible with an empty
        // value; forgetting it again is idempotent.
        return Ok(json!({"status": "forgotten", "id": identity.to_string()}));
    };
    let owner = envelope.get("project").and_then(Value::as_str);
    let scope = envelope.get("scope").and_then(Value::as_str);
    if owner != Some(input.project.as_str())
        && !(scope == Some("global") && input.project == GLOBAL_PROJECT)
    {
        return Err(invalid_request());
    }
    memory_forget(
        client,
        MemoryForgetInput {
            collection: u64::try_from(collection_id.get()).map_err(|_| invalid_request())?,
            id: identity.to_string(),
        },
        options,
    )
    .await
}

/// Redacted service and collection status: counts and identity only.
async fn profile_memory_status(
    client: &HyphaeClient,
    collection: u128,
    options: RequestOptions,
) -> Result<Value, Box<ProductError>> {
    let capabilities = client
        .capabilities(options.clone())
        .await
        .map_err(normalize_client_error)?;
    let ProductResponse::Capabilities(capabilities) = capabilities else {
        return Err(Box::new(ProductError::from_code(
            ProductErrorCode::Internal,
        )));
    };
    let collection_id = ObjectId::new(collection).map_err(|_| invalid_request())?;
    let request = ProductSearchRequest {
        lexical: None,
        vectors: Vec::new(),
        filter: ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        aggregations: vec![hyphae_native_product::ProductNamedAggregation {
            name: "memories".to_owned(),
            aggregation: hyphae_native_product::ProductAggregation::Count,
        }],
        limit: 1,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
    };
    let response = client
        .search_collection(collection_id, request, options)
        .await
        .map_err(normalize_client_error)?;
    let ProductResponse::IntegratedSearch(result) = response else {
        return Err(Box::new(ProductError::from_code(
            ProductErrorCode::Internal,
        )));
    };
    let memories = result
        .aggregations
        .iter()
        .find(|aggregation| aggregation.name == "memories")
        .map_or(Value::Null, |aggregation| match &aggregation.value {
            hyphae_native_product::ProductAggregationValue::Count(count) => json!(count),
            other => json!(format!("{other:?}")),
        });
    Ok(json!({
        "status": "ok",
        "profile": "memory",
        "collection": collection.to_string(),
        "memories": memories,
        "product_api_version": capabilities.product_api_version,
    }))
}

/// Forgets one memory permanently: the lifecycle key and the document
/// leave together, and recall can never surface it again.
async fn memory_forget(
    client: &HyphaeClient,
    input: MemoryForgetInput,
    options: RequestOptions,
) -> Result<Value, Box<ProductError>> {
    let identity: u128 = input.id.parse().map_err(|_| invalid_request())?;
    let collection = ObjectId::new(u128::from(input.collection)).map_err(|_| invalid_request())?;
    let object_id = ObjectId::new(identity).map_err(|_| invalid_request())?;
    // The lifecycle key tombstones through the public scalar surface: an
    // immediately expired set makes it invisible to every recall, and the
    // active-expiry scheduler reclaims it.
    client
        .structure_set(
            memory_key(collection.get(), identity),
            Vec::new(),
            Some(crate::native::logical_time_micros()),
            options.clone(),
        )
        .await
        .map_err(normalize_client_error)?;
    // The forget retry identity is derived from the memory identity but
    // distinct from the store's ingest identity, so an exact forget retry
    // replays while never colliding with the original ingest.
    let mut forget_identity = [0_u8; 16];
    forget_identity.copy_from_slice(
        &blake3::Hasher::new()
            .update(b"hyphae-memory-forget")
            .update(&identity.to_le_bytes())
            .finalize()
            .as_bytes()[..16],
    );
    let forget_identity = match u128::from_le_bytes(forget_identity) {
        0 => 1,
        value => value,
    };
    client
        .search_document_delete(
            collection,
            hyphae_native_product::ProductSearchDocumentDelete {
                idempotency_id: forget_identity,
                object_id,
            },
            options,
        )
        .await
        .map_err(normalize_client_error)?;
    Ok(json!({"status": "forgotten", "id": identity.to_string()}))
}

pub(crate) fn collection_search_request(
    input: CollectionSearchInput,
) -> Result<ProductSearchRequest, Box<ProductError>> {
    Ok(ProductSearchRequest {
        lexical: input.lexical.map(|branch| ProductLexicalBranch {
            query: branch.query,
            candidate_limit: branch.candidate_limit,
            weight: branch.weight,
        }),
        vectors: input
            .vectors
            .into_iter()
            .map(|branch| {
                Ok(ProductVectorBranch {
                    target: branch.target,
                    query: ProductVector::new(branch.values).map_err(|_| invalid_request())?,
                    candidate_limit: branch.candidate_limit,
                    weight: branch.weight,
                    execution: None,
                })
            })
            .collect::<Result<_, Box<ProductError>>>()?,
        filter: input
            .filter
            .map(|value| crate::product_search_filter(value).map_err(|_| invalid_request()))
            .transpose()?
            .unwrap_or(ProductSearchFilter::MatchAll),
        sort: input
            .sort
            .into_iter()
            .map(|value| crate::product_search_sort(value).map_err(|_| invalid_request()))
            .collect::<Result<_, _>>()?,
        facets: input
            .facets
            .into_iter()
            .map(|value| crate::product_facet(value).map_err(|_| invalid_request()))
            .collect::<Result<_, _>>()?,
        aggregations: input
            .aggregations
            .into_iter()
            .map(|value| crate::product_aggregation(value).map_err(|_| invalid_request()))
            .collect::<Result<_, _>>()?,
        limit: input.limit,
        fusion: input.fusion.map(|method| match method {
            FusionMethodInputValue::WeightedScore => {
                hyphae_native_product::ProductFusionMethod::WeightedScore
            }
        }),
        parent_dedupe: input.parent_dedupe.map(|dedupe| {
            hyphae_native_product::ProductParentDedupe {
                field: dedupe.field,
                first_k: dedupe.first_k,
            }
        }),
        rerank: None,
        highlight: None,
    })
}

/// Verifies one sealed proof and witness completely inside the adapter
/// process; verification is trustless and never contacts the daemon.
fn verify_proof_locally(input: &VerifyProofInput) -> Result<Value, Box<ProductError>> {
    use hyphae_native_product::proof::{
        ExternalTrustedAnchor, NativeVerificationLimits, verify_native_proof_offline,
    };
    let proof = crate::decode_hex_bytes(&input.proof_hex).map_err(|_| invalid_request())?;
    let witness = crate::decode_hex_bytes(&input.witness_hex).map_err(|_| invalid_request())?;
    let anchor = crate::decode_hex::<32>(&input.anchor_hex).map_err(|_| invalid_request())?;
    let report = verify_native_proof_offline(
        &proof,
        &witness,
        ExternalTrustedAnchor::new(anchor),
        &NativeVerificationLimits::default(),
    )
    .map_err(|_| invalid_request())?;
    let scope = if report.semantic_reexecution_performed {
        "semantic_reexecution"
    } else {
        "artifact_integrity"
    };
    Ok(json!({
        "status": "verified",
        "scope": scope,
        "kind": crate::proof_kind(report.kind),
        "anchor_digest": crate::encode_hex(&report.anchor_digest),
        "proof_digest": crate::encode_hex(&report.proof_digest),
        "witness_digest": crate::encode_hex(&report.witness_digest),
        "request_digest": crate::encode_hex(&report.request_digest),
        "result_digest": crate::encode_hex(&report.result_digest),
        "evidence_digest": crate::encode_hex(&report.evidence_digest),
        "file_count": report.file_count,
        "directory_count": report.directory_count,
        "total_file_bytes": report.total_file_bytes,
        "semantic_reexecution_performed": report.semantic_reexecution_performed,
    }))
}

fn cancel_active_call(message: &Value, active: Option<&ActiveToolCall>) -> bool {
    let Some(object) = message.as_object() else {
        return false;
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || object.get("id").is_some()
        || object.get("method").and_then(Value::as_str) != Some("notifications/cancelled")
    {
        return false;
    }
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    let Ok(params) = serde_json::from_value::<CancelledParams>(params) else {
        return true;
    };
    if valid_request_id(&params.request_id)
        && let Some(call) = active.filter(|call| call.id == params.request_id)
    {
        call.cancellation.cancel();
    }
    true
}

fn saturated(id: &Value) -> Value {
    rpc_error(
        id,
        SERVER_BUSY,
        "Server busy: one tool call is already active",
    )
}

fn strict_input<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, Box<ProductError>> {
    serde_json::from_value(value).map_err(|_| invalid_request())
}

fn parse_security_cursor(value: &str) -> Result<SecurityCursor, Box<ProductError>> {
    if value.len() > MAX_CURSOR_BYTES {
        return Err(invalid_request());
    }
    SecurityCursor::from_token(value).map_err(|_| invalid_request())
}

fn response_for(tool: NativeTool, response: ProductResponse) -> Result<Value, Box<ProductError>> {
    let expected = matches!(
        (tool, &response),
        (NativeTool::Capabilities, ProductResponse::Capabilities(_))
            | (
                NativeTool::SecurityStatus,
                ProductResponse::SecurityStatus(_)
            )
            | (
                NativeTool::SecurityPrincipals,
                ProductResponse::SecurityPrincipalPage(_)
            )
            | (NativeTool::SearchLexical, ProductResponse::Search(_))
            | (
                NativeTool::SearchCollection,
                ProductResponse::IntegratedSearch(_)
            )
            | (NativeTool::SearchIngest, ProductResponse::SearchIngested(_))
            | (NativeTool::ProveSearch, ProductResponse::Proven { .. })
    );
    if let (NativeTool::ProveSearch, ProductResponse::Proven { response, artifact }) =
        (tool, &response)
    {
        return Ok(json!({
            "status": "generated",
            "kind": crate::proof_kind(artifact.proof.content().kind),
            "response": response_json((**response).clone()),
            "proof_hex": crate::encode_hex(&artifact.proof_bytes),
            "witness_hex": crate::encode_hex(&artifact.witness_bytes),
            "anchor_hex": crate::encode_hex(&artifact.trusted_anchor.digest()),
            "proof_bytes": artifact.proof_bytes.len(),
            "witness_bytes": artifact.witness_bytes.len(),
        }));
    }
    if !expected {
        return Err(Box::new(ProductError::from_code(
            ProductErrorCode::Internal,
        )));
    }
    Ok(response_json(response))
}

fn invalid_request() -> Box<ProductError> {
    Box::new(ProductError::from_code(ProductErrorCode::InvalidRequest))
}

fn read_mcp_credential<R: BufRead>(
    api_key_file: Option<&Path>,
    api_key_stdin: bool,
    input: &mut R,
) -> Result<ApiKeyBuffer, CliFailure> {
    match (api_key_file, api_key_stdin) {
        (Some(path), false) => read_api_key_file(path),
        (None, true) => {
            let line = read_credential_line(input)?.ok_or_else(authorization_denied)?;
            ApiKeyBuffer::from_bytes(line)
        }
        _ => Err(authorization_denied()),
    }
}

fn normalize_client_error(error: ClientError) -> Box<ProductError> {
    match error {
        ClientError::Product(error) => error,
        ClientError::Cancelled => Box::new(ProductError::from_code(ProductErrorCode::Cancelled)),
        ClientError::Protocol(_) => Box::new(ProductError::from_code(ProductErrorCode::Corruption)),
        ClientError::Http(_) | ClientError::Local(_) => {
            Box::new(ProductError::from_code(ProductErrorCode::Unavailable))
        }
        ClientError::UnexpectedResponse => {
            Box::new(ProductError::from_code(ProductErrorCode::Internal))
        }
    }
}

fn startup_client_error(error: ClientError) -> CliFailure {
    CliFailure::from(normalize_client_error(error))
}

fn tool_success(value: &Value, metadata: &Value) -> Value {
    // Hosts read structuredContent; the text mirror omits bulk hex payloads
    // so one artifact-bearing result never doubles past the message budget.
    let text_value = match value.as_object() {
        Some(object) if object.keys().any(|key| key.ends_with("_hex")) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !key.ends_with("_hex"))
                .map(|(key, entry)| (key.clone(), entry.clone()))
                .collect(),
        ),
        _ => value.clone(),
    };
    json!({
        "content": [{ "type": "text", "text": compact_json(&text_value) }],
        "structuredContent": value,
        "isError": false,
        "_meta": metadata,
    })
}

fn tool_error(error: &ProductError, metadata: &Value) -> Value {
    let rendered = error_json(error);
    let structured = json!({
        "schema": "hyphae-native-mcp-tool-error-v2",
        "error": rendered["error"],
    });
    json!({
        "content": [{ "type": "text", "text": compact_json(&structured) }],
        "structuredContent": structured,
        "isError": false,
        "_meta": metadata,
    })
}

fn required_string<'value>(value: &'value Value, field: &str) -> Result<&'value str, CliFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(CliFailure::invalid)
}

fn empty_object() -> Value {
    json!({})
}

const fn default_security_limit() -> usize {
    100
}

fn encode_tool_cursor(offset: usize) -> String {
    format!("hymcpt2:{offset}")
}

fn decode_tool_cursor(value: &str) -> Option<usize> {
    if value.len() > MAX_TOOL_CURSOR_BYTES {
        return None;
    }
    let offset = value.strip_prefix("hymcpt2:")?.parse::<usize>().ok()?;
    (encode_tool_cursor(offset) == value).then_some(offset)
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

fn rpc_result(id: &Value, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn request_id(object: &serde_json::Map<String, Value>) -> Value {
    object.get("id").cloned().unwrap_or(Value::Null)
}

fn valid_request_id(value: &Value) -> bool {
    value.is_string() || value.is_i64() || value.is_u64()
}

fn read_bounded_line<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let line = read_line_with_bound(
        reader,
        MAX_MESSAGE_BYTES + 1,
        "MCP input exceeds the fixed message bound",
    )?;
    if line.as_ref().is_some_and(|line| {
        line.len()
            .saturating_sub(usize::from(line.last() == Some(&b'\n')))
            > MAX_MESSAGE_BYTES
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MCP input exceeds the fixed message bound",
        ));
    }
    Ok(line)
}

fn read_credential_line<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    read_line_with_bound(
        reader,
        MAX_API_KEY_CREDENTIAL_BYTES + 2,
        "Native API-key input exceeds the fixed credential bound",
    )
}

fn read_line_with_bound<R: BufRead>(
    reader: &mut R,
    maximum_bytes: usize,
    error_message: &'static str,
) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(consumed) > maximum_bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidData, error_message));
        }
        line.extend_from_slice(&available[..consumed]);
        let complete = available.get(consumed.wrapping_sub(1)) == Some(&b'\n');
        reader.consume(consumed);
        if complete {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        io::{BufReader, Cursor},
        pin::Pin,
    };

    use hyphae_client::v2::{
        ClientError, HyphaeClient, ProductOperation, ProductResponse, RequestOptions, Transport,
    };
    use serde_json::json;

    use super::{
        ActiveToolCall, MAX_MESSAGE_BYTES, MCP_CONTRACT, Session, SessionAction, ToolCall,
        ToolRegistry, cancel_active_call, read_bounded_line, read_mcp_credential, saturated,
        send_response,
    };

    #[derive(Clone)]
    struct CancellationTransport;

    impl Transport for CancellationTransport {
        fn execute(
            &self,
            _operation: ProductOperation,
            options: RequestOptions,
        ) -> Pin<Box<dyn Future<Output = Result<ProductResponse, ClientError>> + Send + '_>>
        {
            Box::pin(async move {
                while !options.cancellation.is_cancelled() {
                    tokio::task::yield_now().await;
                }
                Err(ClientError::Product(Box::new(
                    hyphae_native_product::ProductError::from_code(
                        hyphae_native_product::ProductErrorCode::Cancelled,
                    ),
                )))
            })
        }
    }

    #[test]
    fn embedded_contract_is_read_only_versioned_and_paginated()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = ToolRegistry::load()?;
        assert_eq!(registry.protocol, "2025-06-18");
        assert_eq!(registry.schema_version, "hyphae-native-mcp-tools-v4");
        assert_eq!(registry.schema_digest.len(), 64);
        assert_eq!(registry.page_size, 100);
        assert_eq!(registry.tools.len(), 11);
        let first = registry
            .list(&serde_json::json!({}))
            .map_err(|()| "first page")?;
        assert_eq!(first["tools"].as_array().map(Vec::len), Some(11));
        let read_only = ToolRegistry::load()?.without_ingest();
        assert_eq!(read_only.tools.len(), 8);
        assert!(read_only.tools.iter().all(|tool| {
            tool["name"]
                .as_str()
                .is_none_or(|name| !super::WRITE_TOOL_NAMES.contains(&name))
        }));
        assert!(first.get("nextCursor").is_none());
        assert!(
            registry
                .list(&serde_json::json!({ "hostExtension": true }))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn embedded_contract_rejects_boundary_and_redaction_drift()
    -> Result<(), Box<dyn std::error::Error>> {
        let original: serde_json::Value = serde_json::from_str(MCP_CONTRACT)?;

        let mut page_size = original.clone();
        page_size["tool_page_size"] = serde_json::json!(3);
        assert!(ToolRegistry::from_contract(&serde_json::to_string(&page_size)?).is_err());

        let mut hints = original.clone();
        hints["tools"][0]["annotations"]["idempotentHint"] = serde_json::json!(false);
        assert!(ToolRegistry::from_contract(&serde_json::to_string(&hints)?).is_err());

        let mut error_schema = original.clone();
        let branches = error_schema["tools"][0]["outputSchema"]["oneOf"]
            .as_array_mut()
            .ok_or("missing output branches")?;
        branches.pop();
        assert!(ToolRegistry::from_contract(&serde_json::to_string(&error_schema)?).is_err());

        let mut root_type = original.clone();
        root_type["tools"][0]["outputSchema"]
            .as_object_mut()
            .ok_or("missing output schema")?
            .remove("type");
        assert!(ToolRegistry::from_contract(&serde_json::to_string(&root_type)?).is_err());

        let mut secret = original;
        secret["tools"][1]["outputSchema"]["oneOf"][0]["properties"]["api_key"] =
            serde_json::json!({ "type": "string" });
        assert!(ToolRegistry::from_contract(&serde_json::to_string(&secret)?).is_err());
        Ok(())
    }

    #[test]
    fn stdin_credential_consumes_only_the_first_bounded_line()
    -> Result<(), Box<dyn std::error::Error>> {
        let key = format!("hyp1_{}_{}", "a".repeat(32), "b".repeat(64));
        let mut input = BufReader::new(Cursor::new(format!("{key}\n{{}}\n")));
        let credential = read_mcp_credential(None, true, &mut input)?;
        assert_eq!(credential.credential()?, key);
        assert_eq!(read_bounded_line(&mut input)?, Some(b"{}\n".to_vec()));
        Ok(())
    }

    #[test]
    fn stdio_lines_are_bounded() -> Result<(), std::io::Error> {
        let mut valid = BufReader::new(Cursor::new(b"{}\n"));
        assert_eq!(read_bounded_line(&mut valid)?, Some(b"{}\n".to_vec()));
        let exact = vec![b'x'; MAX_MESSAGE_BYTES];
        let mut exact_input = BufReader::new(Cursor::new(exact.clone()));
        assert_eq!(read_bounded_line(&mut exact_input)?, Some(exact));
        let oversized = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let mut oversized = BufReader::new(Cursor::new(oversized));
        let Err(error) = read_bounded_line(&mut oversized) else {
            return Err(std::io::Error::other("oversized message was accepted"));
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_notification_is_direct_and_idempotent()
    -> Result<(), Box<dyn std::error::Error>> {
        let cancellation = hyphae_client::v2::CancellationToken::new();
        let active = ActiveToolCall {
            id: json!(41),
            cancellation,
            task: tokio::spawn(std::future::pending()),
        };
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 41, "reason": "test"}
        });
        assert!(cancel_active_call(&notification, Some(&active)));
        assert!(active.cancellation.is_cancelled());
        assert!(cancel_active_call(&notification, Some(&active)));
        active.task.abort();
        assert!(cancel_active_call(
            &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":999}}),
            None,
        ));
        Ok(())
    }

    #[test]
    fn saturation_rejects_instead_of_queueing() {
        let response = saturated(&json!(42));
        assert_eq!(response["id"], 42);
        assert_eq!(response["error"]["code"], super::SERVER_BUSY);
    }

    #[tokio::test]
    async fn oversized_output_is_replaced_by_a_bounded_rpc_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        send_response(
            &sender,
            &json!({
                "jsonrpc": "2.0",
                "id": 55,
                "result": "x".repeat(MAX_MESSAGE_BYTES),
            }),
        )?;
        let encoded = receiver.recv().await.ok_or("missing bounded response")?;
        assert!(encoded.len() <= MAX_MESSAGE_BYTES);
        let response: serde_json::Value = serde_json::from_slice(&encoded)?;
        assert_eq!(response["id"], 55);
        assert_eq!(response["error"]["code"], super::RESPONSE_TOO_LARGE);
        Ok(())
    }

    #[tokio::test]
    async fn session_recovers_after_cancelled_tool_call() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut session = Session {
            client: HyphaeClient::new(CancellationTransport),
            registry: ToolRegistry::load()?.without_ingest(),
            allow_ingest: false,
            profile: super::Profile::Full,
            initialize_seen: true,
            initialized: true,
        };
        let SessionAction::ToolCall(call) = session.handle(&json!({
            "jsonrpc":"2.0",
            "id":7,
            "method":"tools/call",
            "params":{"name":"hyphae_native_capabilities","arguments":{}}
        })) else {
            return Err("tool call was not admitted".into());
        };
        let active = super::start_tool_call(session.client.clone(), &session.registry, call);
        assert!(cancel_active_call(
            &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}),
            Some(&active),
        ));
        let cancelled =
            tokio::time::timeout(std::time::Duration::from_secs(1), active.task).await??;
        assert_eq!(
            cancelled["result"]["structuredContent"]["error"]["code"],
            "cancelled"
        );

        let SessionAction::Response(ping) = session.handle(&json!({
            "jsonrpc":"2.0",
            "id":8,
            "method":"ping",
            "params":{}
        })) else {
            return Err("session did not recover after cancellation".into());
        };
        assert_eq!(ping["id"], 8);
        assert_eq!(ping["result"], json!({}));

        #[allow(clippy::manual_let_else)]
        let ToolCall { id, .. } = match session.handle(&json!({
            "jsonrpc":"2.0",
            "id":9,
            "method":"tools/call",
            "params":{"name":"hyphae_native_capabilities","arguments":{}}
        })) {
            SessionAction::ToolCall(call) => call,
            _ => return Err("subsequent tool call was not admitted".into()),
        };
        assert_eq!(id, 9);
        Ok(())
    }
}
