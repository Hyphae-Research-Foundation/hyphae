// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded MCP stdio adapter over the public Hyphae HTTP client.

use std::{
    error::Error,
    future::Future,
    io::{self, BufRead, BufReader, BufWriter, Write},
};

use hyphae_client::HyphaeClient;
use hyphae_contracts::{
    CAPABILITIES_SCHEMA_V1, COMMIT_RECEIPT_SCHEMA_V1, DEFINE_LEXICAL_INDEX_REQUEST_SCHEMA_V1,
    DEFINE_VECTOR_SPACE_REQUEST_SCHEMA_V1, DELETE_REQUEST_SCHEMA_V1,
    DELETE_VECTORS_REQUEST_SCHEMA_V1, EXACT_RETRIEVAL_REQUEST_SCHEMA_V1,
    EXACT_RETRIEVAL_RESPONSE_SCHEMA_V1, GET_REQUEST_SCHEMA_V1, GET_RESPONSE_SCHEMA_V1,
    HYBRID_RETRIEVAL_REQUEST_SCHEMA_V1, HYBRID_RETRIEVAL_RESPONSE_SCHEMA_V1,
    LEXICAL_RETRIEVAL_REQUEST_SCHEMA_V1, LEXICAL_RETRIEVAL_RESPONSE_SCHEMA_V1,
    PUT_REQUEST_SCHEMA_V1, PUT_VECTORS_REQUEST_SCHEMA_V1, QUERY_REQUEST_SCHEMA_V1,
    QUERY_RESPONSE_SCHEMA_V1,
    v1::{
        DefineLexicalIndexRequestV1, DefineVectorSpaceRequestV1, DeleteRequestV1,
        DeleteVectorsRequestV1, ExactRetrievalRequestV1, GetRequestV1, HybridRetrievalRequestV1,
        LexicalRetrievalRequestV1, PutRequestV1, PutVectorsRequestV1, QueryRequestV1,
    },
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const MCP_PROTOCOL: &str = "2025-11-25";
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const EMPTY_INPUT_SCHEMA: &str =
    r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const CAPABILITIES_OUTPUT_SCHEMA: &str = CAPABILITIES_SCHEMA_V1;
const PUT_INPUT_SCHEMA: &str = PUT_REQUEST_SCHEMA_V1;
const DELETE_INPUT_SCHEMA: &str = DELETE_REQUEST_SCHEMA_V1;
const GET_INPUT_SCHEMA: &str = GET_REQUEST_SCHEMA_V1;
const QUERY_INPUT_SCHEMA: &str = QUERY_REQUEST_SCHEMA_V1;
const RECEIPT_OUTPUT_SCHEMA: &str = COMMIT_RECEIPT_SCHEMA_V1;
const GET_OUTPUT_SCHEMA: &str = GET_RESPONSE_SCHEMA_V1;
const QUERY_OUTPUT_SCHEMA: &str = QUERY_RESPONSE_SCHEMA_V1;
const DEFINE_VECTOR_SPACE_INPUT_SCHEMA: &str = DEFINE_VECTOR_SPACE_REQUEST_SCHEMA_V1;
const PUT_VECTORS_INPUT_SCHEMA: &str = PUT_VECTORS_REQUEST_SCHEMA_V1;
const DELETE_VECTORS_INPUT_SCHEMA: &str = DELETE_VECTORS_REQUEST_SCHEMA_V1;
const EXACT_RETRIEVAL_INPUT_SCHEMA: &str = EXACT_RETRIEVAL_REQUEST_SCHEMA_V1;
const EXACT_RETRIEVAL_OUTPUT_SCHEMA: &str = EXACT_RETRIEVAL_RESPONSE_SCHEMA_V1;
const DEFINE_LEXICAL_INDEX_INPUT_SCHEMA: &str = DEFINE_LEXICAL_INDEX_REQUEST_SCHEMA_V1;
const LEXICAL_RETRIEVAL_INPUT_SCHEMA: &str = LEXICAL_RETRIEVAL_REQUEST_SCHEMA_V1;
const LEXICAL_RETRIEVAL_OUTPUT_SCHEMA: &str = LEXICAL_RETRIEVAL_RESPONSE_SCHEMA_V1;
const HYBRID_RETRIEVAL_INPUT_SCHEMA: &str = HYBRID_RETRIEVAL_REQUEST_SCHEMA_V1;
const HYBRID_RETRIEVAL_OUTPUT_SCHEMA: &str = HYBRID_RETRIEVAL_RESPONSE_SCHEMA_V1;

struct Session {
    client: HyphaeClient,
    initialize_seen: bool,
    initialized: bool,
}

/// Runs one newline-delimited JSON-RPC 2.0 MCP session over stdio.
///
/// # Errors
///
/// Returns an error for local client construction or fatal standard-I/O and
/// response-serialization failures. Malformed peer requests receive JSON-RPC
/// errors and do not terminate the session.
pub(crate) async fn run(base_url: &str, bearer_token: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut builder = HyphaeClient::builder(base_url)?;
    if let Some(token) = bearer_token {
        builder = builder.bearer_token(token)?;
    }
    let mut session = Session {
        client: builder.build()?,
        initialize_seen: false,
        initialized: false,
    };
    let mut input = BufReader::new(io::stdin().lock());
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
            self.handle_notification(method, &params);
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        match method {
            "initialize" => Some(self.initialize(&id, &params)),
            "ping" => Some(rpc_result(&id, &json!({}))),
            _ if !self.initialized => Some(rpc_error(&id, -32002, "Server not initialized")),
            "tools/list" => Some(Self::list_tools(&id, &params)),
            "tools/call" => Some(self.call_tool(id, &params).await),
            _ => Some(rpc_error(&id, -32601, "Method not found")),
        }
    }

    fn handle_notification(&mut self, method: &str, _params: &Value) {
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
                "protocolVersion": MCP_PROTOCOL,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "hyphae",
                    "title": "Hyphae autonomous data engine",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Use the versioned structured tools. Results include verifiable Hyphae proofs. Mutations require host/user authorization."
            }),
        )
    }

    fn list_tools(id: &Value, params: &Value) -> Value {
        if !params.as_object().is_some_and(serde_json::Map::is_empty) {
            return rpc_error(id, -32602, "Pagination is not supported");
        }
        match tool_definitions() {
            Ok(tools) => rpc_result(id, &json!({ "tools": tools })),
            Err(_) => rpc_error(id, -32603, "Internal error"),
        }
    }

    async fn call_tool(&self, id: Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return rpc_error(&id, -32602, "Tool name is required");
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return rpc_error(&id, -32602, "Tool arguments must be an object");
        }
        let result = match name {
            "hyphae_capabilities" => {
                if arguments.as_object().is_some_and(serde_json::Map::is_empty) {
                    self.client
                        .capabilities()
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|response| {
                            serde_json::to_value(response.value).map_err(|error| error.to_string())
                        })
                } else {
                    return rpc_result(&id, &tool_error("capabilities accepts no arguments"));
                }
            }
            "hyphae_put" => {
                self.call::<PutRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.put(&request).await
                })
                .await
            }
            "hyphae_get" => {
                self.call::<GetRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.get(&request).await
                })
                .await
            }
            "hyphae_delete" => {
                self.call::<DeleteRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.delete(&request).await
                })
                .await
            }
            "hyphae_query" => {
                self.call::<QueryRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.query(&request).await
                })
                .await
            }
            "hyphae_define_vector_space" => {
                self.call::<DefineVectorSpaceRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.define_vector_space(&request).await
                })
                .await
            }
            "hyphae_put_vectors" => {
                self.call::<PutVectorsRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.put_vectors(&request).await
                })
                .await
            }
            "hyphae_delete_vectors" => {
                self.call::<DeleteVectorsRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.delete_vectors(&request).await
                })
                .await
            }
            "hyphae_retrieve_exact" => {
                self.call::<ExactRetrievalRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.retrieve_exact(&request).await
                })
                .await
            }
            "hyphae_define_lexical_index" => {
                self.call::<DefineLexicalIndexRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.define_lexical_index(&request).await
                })
                .await
            }
            "hyphae_retrieve_lexical" => {
                self.call::<LexicalRetrievalRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.retrieve_lexical(&request).await
                })
                .await
            }
            "hyphae_retrieve_hybrid" => {
                self.call::<HybridRetrievalRequestV1, _, _, _>(arguments, |request| async move {
                    self.client.retrieve_hybrid(&request).await
                })
                .await
            }
            _ => return rpc_error(&id, -32602, "Unknown tool"),
        };
        match result {
            Ok(value) => rpc_result(&id, &tool_success(&value)),
            Err(error) => rpc_result(&id, &tool_error(&error)),
        }
    }

    async fn call<Request, Response, Function, FutureType>(
        &self,
        arguments: Value,
        function: Function,
    ) -> Result<Value, String>
    where
        Request: DeserializeOwned,
        Response: serde::Serialize,
        Function: FnOnce(Request) -> FutureType,
        FutureType: Future<
            Output = Result<hyphae_client::ApiResponse<Response>, hyphae_client::ClientError>,
        >,
    {
        let request = serde_json::from_value::<Request>(arguments)
            .map_err(|error| format!("invalid tool input: {error}"))?;
        let response = function(request).await.map_err(|error| error.to_string())?;
        serde_json::to_value(response.value).map_err(|error| error.to_string())
    }
}

#[allow(clippy::too_many_lines)]
fn tool_definitions() -> Result<Vec<Value>, serde_json::Error> {
    Ok(vec![
        tool(
            "hyphae_capabilities",
            "Inspect versioned Hyphae capabilities and effective limits.",
            EMPTY_INPUT_SCHEMA,
            CAPABILITIES_OUTPUT_SCHEMA,
            true,
            false,
            true,
        )?,
        tool(
            "hyphae_put",
            "Atomically store a structured record batch. Obtain user authorization before mutation.",
            PUT_INPUT_SCHEMA,
            RECEIPT_OUTPUT_SCHEMA,
            false,
            true,
            false,
        )?,
        tool(
            "hyphae_get",
            "Get proven key presence or absence by hexadecimal binary key.",
            GET_INPUT_SCHEMA,
            GET_OUTPUT_SCHEMA,
            true,
            false,
            true,
        )?,
        tool(
            "hyphae_delete",
            "Atomically delete a key batch. Obtain user authorization before mutation.",
            DELETE_INPUT_SCHEMA,
            RECEIPT_OUTPUT_SCHEMA,
            false,
            true,
            false,
        )?,
        tool(
            "hyphae_query",
            "Execute a deterministic proof-bearing structured query without AI.",
            QUERY_INPUT_SCHEMA,
            QUERY_OUTPUT_SCHEMA,
            true,
            false,
            true,
        )?,
        tool(
            "hyphae_define_vector_space",
            "Define or exactly reuse one immutable durable vector space.",
            DEFINE_VECTOR_SPACE_INPUT_SCHEMA,
            RECEIPT_OUTPUT_SCHEMA,
            false,
            true,
            true,
        )?,
        tool(
            "hyphae_put_vectors",
            "Atomically store a durable signed-Q15 vector batch.",
            PUT_VECTORS_INPUT_SCHEMA,
            RECEIPT_OUTPUT_SCHEMA,
            false,
            true,
            false,
        )?,
        tool(
            "hyphae_delete_vectors",
            "Atomically delete durable vectors by binary key.",
            DELETE_VECTORS_INPUT_SCHEMA,
            RECEIPT_OUTPUT_SCHEMA,
            false,
            true,
            false,
        )?,
        tool(
            "hyphae_retrieve_exact",
            "Execute proof-bearing exact durable vector retrieval.",
            EXACT_RETRIEVAL_INPUT_SCHEMA,
            EXACT_RETRIEVAL_OUTPUT_SCHEMA,
            true,
            false,
            true,
        )?,
        tool(
            "hyphae_define_lexical_index",
            "Define or exactly reuse one immutable provider-free lexical index.",
            DEFINE_LEXICAL_INDEX_INPUT_SCHEMA,
            RECEIPT_OUTPUT_SCHEMA,
            false,
            true,
            true,
        )?,
        tool(
            "hyphae_retrieve_lexical",
            "Execute proof-bearing provider-free lexical retrieval.",
            LEXICAL_RETRIEVAL_INPUT_SCHEMA,
            LEXICAL_RETRIEVAL_OUTPUT_SCHEMA,
            true,
            false,
            true,
        )?,
        tool(
            "hyphae_retrieve_hybrid",
            "Execute proof-bearing deterministic hybrid RRF retrieval.",
            HYBRID_RETRIEVAL_INPUT_SCHEMA,
            HYBRID_RETRIEVAL_OUTPUT_SCHEMA,
            true,
            false,
            true,
        )?,
    ])
}

fn tool(
    name: &str,
    description: &str,
    input_schema: &str,
    output_schema: &str,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
) -> Result<Value, serde_json::Error> {
    Ok(json!({
        "name": name,
        "description": description,
        "inputSchema": serde_json::from_str::<Value>(input_schema)?,
        "outputSchema": serde_json::from_str::<Value>(output_schema)?,
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": true
        },
        "execution": { "taskSupport": "forbidden" }
    }))
}

fn tool_success(value: &Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": compact_json(value) }],
        "structuredContent": value,
        "isError": false
    })
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
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
        if line.len().saturating_add(consumed) > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP message exceeds 4 MiB",
            ));
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

    use super::{MAX_MESSAGE_BYTES, read_bounded_line, tool_definitions};

    #[test]
    fn embedded_tool_schemas_are_valid_json_objects() -> Result<(), serde_json::Error> {
        let tools = tool_definitions()?;
        assert_eq!(tools.len(), 12);
        assert!(tools.iter().all(|tool| tool["inputSchema"].is_object()));
        assert!(tools.iter().all(|tool| tool["outputSchema"].is_object()));
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
