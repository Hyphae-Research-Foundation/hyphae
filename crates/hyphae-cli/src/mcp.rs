// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded read-only MCP stdio adapter over managed Native HTTP v2.

use std::{
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::Path,
};

use hyphae_client::v2::{ClientError, HttpTransport, HyphaeClient, RequestOptions};
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
const MCP_CONTRACT_SCHEMA: &str = "hyphae-native-mcp-contract-v1";
const MCP_PROTOCOL: &str = "2025-11-25";
const TOOL_SCHEMA_VERSION: &str = "hyphae-native-mcp-tools-v1";
const TOOL_PAGE_SIZE: usize = 2;
const TOOL_NAMES: [&str; 3] = [
    "hyphae_native_capabilities",
    "hyphae_native_security_status",
    "hyphae_native_security_principals",
];
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 128;
const MAX_TOOL_CURSOR_BYTES: usize = 32;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsListParams {
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsCallParams {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
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

/// Runs one newline-delimited JSON-RPC 2.0 MCP session over stdio.
///
/// In stdin credential mode, the first bounded line is the credential and all
/// following lines are MCP messages. The credential line is never echoed.
///
/// # Errors
///
/// Returns a typed CLI failure for missing or invalid credentials, client
/// construction, fatal standard-I/O, or response serialization failures.
pub(crate) async fn run(
    base_url: &str,
    api_key_file: Option<&Path>,
    api_key_stdin: bool,
) -> Result<(), CliFailure> {
    let mut input = BufReader::new(io::stdin().lock());
    let credential = read_mcp_credential(api_key_file, api_key_stdin, &mut input)?;
    let credential_text = credential.credential()?;
    let transport = HttpTransport::new(base_url)
        .and_then(|transport| transport.bearer_token(credential_text))
        .map_err(startup_client_error)?;
    drop(credential);

    let mut session = Session {
        client: HyphaeClient::new(transport),
        registry: ToolRegistry::load()?,
        initialize_seen: false,
        initialized: false,
    };
    let mut output = BufWriter::new(io::stdout().lock());
    loop {
        let Some(line) = read_bounded_line(&mut input)? else {
            output.flush()?;
            return Ok(());
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(message) => session.handle(message).await,
            Err(_) => Some(rpc_error(&Value::Null, -32700, "Parse error")),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

impl ToolRegistry {
    fn load() -> Result<Self, CliFailure> {
        Self::from_contract(MCP_CONTRACT)
    }

    fn from_contract(source: &str) -> Result<Self, CliFailure> {
        let contract: Value = serde_json::from_str(source)?;
        let protocol = required_string(&contract, "mcp_protocol")?.to_owned();
        let schema_version = required_string(&contract, "tool_schema_version")?.to_owned();
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
        let next_cursor = (end < self.tools.len()).then(|| encode_tool_cursor(end));
        Ok(json!({
            "tools": self.tools[offset..end],
            "nextCursor": next_cursor,
            "_meta": self.metadata(),
        }))
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
    output.len() == 1
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
            == Some("hyphae-native-mcp-tool-error-v1")
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
    async fn handle(&mut self, message: Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(rpc_error(&Value::Null, -32600, "Invalid Request"));
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(rpc_error(&request_id(object), -32600, "Invalid Request"));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Some(rpc_error(&request_id(object), -32600, "Invalid Request"));
        };
        let id = object.get("id").cloned();
        if id
            .as_ref()
            .is_some_and(|value| !value.is_string() && !value.is_i64() && !value.is_u64())
        {
            return Some(rpc_error(&Value::Null, -32600, "Invalid Request"));
        }
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        if !params.is_object() {
            return id.map(|id| rpc_error(&id, -32602, "Invalid params"));
        }
        if id.is_none() {
            self.handle_notification(method);
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        match method {
            "initialize" => Some(self.initialize(&id, &params)),
            "ping" => Some(rpc_result(&id, &json!({}))),
            _ if !self.initialized => Some(rpc_error(&id, -32002, "Server not initialized")),
            "tools/list" => Some(self.list_tools(&id, &params)),
            "tools/call" => Some(self.call_tool(&id, &params).await),
            _ => Some(rpc_error(&id, -32601, "Method not found")),
        }
    }

    fn handle_notification(&mut self, method: &str) {
        if method == "notifications/initialized" && self.initialize_seen {
            self.initialized = true;
        }
    }

    fn initialize(&mut self, id: &Value, params: &Value) -> Value {
        if self.initialize_seen
            || params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .is_none()
            || !params.get("capabilities").is_some_and(Value::is_object)
            || !params.get("clientInfo").is_some_and(Value::is_object)
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

    async fn call_tool(&self, id: &Value, params: &Value) -> Value {
        let params = match serde_json::from_value::<ToolsCallParams>(params.clone()) {
            Ok(params) if params.arguments.is_object() => params,
            _ => return rpc_error(id, -32602, "Invalid params"),
        };
        let Some(tool) = NativeReadTool::parse(&params.name) else {
            return rpc_error(id, -32602, "Unknown tool");
        };
        let result = self.execute(tool, params.arguments).await;
        match result {
            Ok(value) => rpc_result(id, &tool_success(&value, &self.registry)),
            Err(error) => rpc_result(id, &tool_error(&error, &self.registry)),
        }
    }

    async fn execute(
        &self,
        tool: NativeReadTool,
        arguments: Value,
    ) -> Result<Value, Box<ProductError>> {
        let response = match tool {
            NativeReadTool::Capabilities => {
                strict_input::<EmptyInput>(arguments)?;
                self.client.capabilities(RequestOptions::default()).await
            }
            NativeReadTool::SecurityStatus => {
                strict_input::<EmptyInput>(arguments)?;
                self.client.security_status(RequestOptions::default()).await
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
                self.client
                    .security_principal_list(request, RequestOptions::default())
                    .await
            }
        }
        .map_err(normalize_client_error)?;
        response_for(tool, response)
    }
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

fn tool_success(value: &Value, registry: &ToolRegistry) -> Value {
    json!({
        "content": [{ "type": "text", "text": compact_json(value) }],
        "structuredContent": value,
        "isError": false,
        "_meta": registry.metadata(),
    })
}

fn tool_error(error: &ProductError, registry: &ToolRegistry) -> Value {
    let rendered = error_json(error);
    let structured = json!({
        "schema": "hyphae-native-mcp-tool-error-v1",
        "error": rendered["error"],
    });
    json!({
        "content": [{ "type": "text", "text": compact_json(&structured) }],
        "structuredContent": structured,
        "isError": true,
        "_meta": registry.metadata(),
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
    format!("hymcpt1:{offset}")
}

fn decode_tool_cursor(value: &str) -> Option<usize> {
    if value.len() > MAX_TOOL_CURSOR_BYTES {
        return None;
    }
    let offset = value.strip_prefix("hymcpt1:")?.parse::<usize>().ok()?;
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

fn read_bounded_line<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    read_line_with_bound(
        reader,
        MAX_MESSAGE_BYTES,
        "MCP input exceeds the fixed message bound",
    )
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
    use std::io::{BufReader, Cursor};

    use super::{
        MAX_MESSAGE_BYTES, MCP_CONTRACT, ToolRegistry, read_bounded_line, read_mcp_credential,
    };

    #[test]
    fn embedded_contract_is_read_only_versioned_and_paginated()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = ToolRegistry::load()?;
        assert_eq!(registry.protocol, "2025-11-25");
        assert_eq!(registry.schema_version, "hyphae-native-mcp-tools-v1");
        assert_eq!(registry.schema_digest.len(), 64);
        assert_eq!(registry.page_size, 2);
        assert_eq!(registry.tools.len(), 3);
        let first = registry
            .list(&serde_json::json!({}))
            .map_err(|()| "first page")?;
        assert_eq!(first["tools"].as_array().map(Vec::len), Some(2));
        assert_eq!(first["nextCursor"], "hymcpt1:2");
        let second = registry
            .list(&serde_json::json!({ "cursor": "hymcpt1:2" }))
            .map_err(|()| "second page")?;
        assert_eq!(second["tools"].as_array().map(Vec::len), Some(1));
        assert!(second["nextCursor"].is_null());
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
        let oversized = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let mut oversized = BufReader::new(Cursor::new(oversized));
        let Err(error) = read_bounded_line(&mut oversized) else {
            return Err(std::io::Error::other("oversized message was accepted"));
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        Ok(())
    }
}
