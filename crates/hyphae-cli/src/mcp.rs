// SPDX-License-Identifier: Apache-2.0

//! Bounded read-only MCP stdio adapter over managed Native HTTP v2.

use std::{
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::Path,
};

use hyphae_client::v2::{
    CancellationToken, ClientError, HttpTransport, HyphaeClient, RequestOptions,
};
use hyphae_contracts::NATIVE_MCP_V2;
use hyphae_native_product::{
    MAX_API_KEY_CREDENTIAL_BYTES, ProductError, ProductErrorCode, ProductResponse, SecurityCursor,
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
const TOOL_SCHEMA_VERSION: &str = "hyphae-native-mcp-tools-v2";
const TOOL_PAGE_SIZE: usize = 100;
const TOOL_NAMES: [&str; 3] = [
    "hyphae_native_capabilities",
    "hyphae_native_security_status",
    "hyphae_native_security_principals",
];
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 128;
const MAX_TOOL_CURSOR_BYTES: usize = 32;
const CHANNEL_CAPACITY: usize = 1;
const SERVER_BUSY: i32 = -32001;
const RESPONSE_TOO_LARGE: i32 = -32003;

#[derive(Clone, Copy)]
enum NativeReadTool {
    Capabilities,
    SecurityStatus,
    SecurityPrincipals,
}

impl NativeReadTool {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "hyphae_native_capabilities" => Some(Self::Capabilities),
            "hyphae_native_security_status" => Some(Self::SecurityStatus),
            "hyphae_native_security_principals" => Some(Self::SecurityPrincipals),
            _ => None,
        }
    }
}

struct ToolRegistry {
    protocol: String,
    schema_version: String,
    schema_digest: String,
    page_size: usize,
    tools: Vec<Value>,
}

struct Session {
    client: HyphaeClient,
    registry: ToolRegistry,
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
    tool: NativeReadTool,
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
pub(crate) async fn run(
    base_url: &str,
    api_key_file: Option<&Path>,
    api_key_stdin: bool,
) -> Result<(), CliFailure> {
    let mut input = BufReader::new(io::stdin());
    let credential = read_mcp_credential(api_key_file, api_key_stdin, &mut input)?;
    let credential_text = credential.credential()?;
    let transport = HttpTransport::new(base_url)
        .and_then(|transport| transport.bearer_token(credential_text))
        .and_then(|transport| transport.response_bytes(MAX_MESSAGE_BYTES))
        .map_err(startup_client_error)?;
    drop(credential);

    let mut session = Session {
        client: HyphaeClient::new(transport),
        registry: ToolRegistry::load()?,
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
        "readOnlyHint": true,
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
            "tools/call" => Self::prepare_tool_call(id, &params),
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

    fn prepare_tool_call(id: Value, params: &Value) -> SessionAction {
        let params = match serde_json::from_value::<ToolsCallParams>(params.clone()) {
            Ok(params) if params.arguments.is_object() => params,
            _ => {
                return SessionAction::Response(rpc_error(&id, -32602, "Invalid params"));
            }
        };
        let Some(tool) = NativeReadTool::parse(&params.name) else {
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

async fn execute_tool(
    client: HyphaeClient,
    tool: NativeReadTool,
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
        NativeReadTool::Capabilities => {
            strict_input::<EmptyInput>(arguments)?;
            client.capabilities(options).await
        }
        NativeReadTool::SecurityStatus => {
            strict_input::<EmptyInput>(arguments)?;
            client.security_status(options).await
        }
        NativeReadTool::SecurityPrincipals => {
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
    }
    .map_err(normalize_client_error)?;
    response_for(tool, response)
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

fn response_for(
    tool: NativeReadTool,
    response: ProductResponse,
) -> Result<Value, Box<ProductError>> {
    let expected = matches!(
        (tool, &response),
        (
            NativeReadTool::Capabilities,
            ProductResponse::Capabilities(_)
        ) | (
            NativeReadTool::SecurityStatus,
            ProductResponse::SecurityStatus(_)
        ) | (
            NativeReadTool::SecurityPrincipals,
            ProductResponse::SecurityPrincipalPage(_)
        )
    );
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
    json!({
        "content": [{ "type": "text", "text": compact_json(value) }],
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
        assert_eq!(registry.schema_version, "hyphae-native-mcp-tools-v2");
        assert_eq!(registry.schema_digest.len(), 64);
        assert_eq!(registry.page_size, 100);
        assert_eq!(registry.tools.len(), 3);
        let first = registry
            .list(&serde_json::json!({}))
            .map_err(|()| "first page")?;
        assert_eq!(first["tools"].as_array().map(Vec::len), Some(3));
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
            registry: ToolRegistry::load()?,
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
