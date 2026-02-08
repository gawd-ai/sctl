//! MCP (Model Context Protocol) JSON-RPC handler.
//!
//! Implements the [MCP specification](https://spec.modelcontextprotocol.io/)
//! over stdio — reads JSON-RPC 2.0 requests from stdin (one per line) and
//! writes responses to stdout.
//!
//! ## Supported methods
//!
//! | Method              | Description                      |
//! |---------------------|----------------------------------|
//! | `initialize`        | Handshake, returns capabilities  |
//! | `tools/list`        | List available tool definitions  |
//! | `tools/call`        | Execute a tool and return result |
//! | `ping`              | Liveness check                   |
//!
//! Notifications (`notifications/initialized`, `notifications/cancelled`) are
//! acknowledged silently.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::devices::DeviceRegistry;
use crate::playbook_registry::PlaybookRegistry;
use crate::tools;

const SERVER_NAME: &str = "mcp-sctl";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the MCP server on stdio, processing JSON-RPC requests until EOF.
pub async fn run_stdio(registry: DeviceRegistry, pb_registry: PlaybookRegistry) {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("mcp-sctl: stdin read error: {}", e);
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32700,
                        "message": format!("Parse error: {}", e)
                    }
                });
                write_response(&mut stdout, &response).await;
                continue;
            }
        };

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");

        // Notifications (no id) — acknowledge silently
        if id.is_none() {
            match method {
                "notifications/initialized" | "notifications/cancelled" => {}
                _ => {
                    eprintln!("mcp-sctl: unknown notification: {}", method);
                }
            }
            continue;
        }

        let (response, notify_tools_changed) = match method {
            "initialize" => (handle_initialize(&request), false),
            "tools/list" => (handle_tools_list(&registry, &pb_registry).await, false),
            "tools/call" => handle_tools_call(&request, &registry, &pb_registry).await,
            "ping" => (json!({ "jsonrpc": "2.0", "id": id, "result": {} }), false),
            _ => (
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: {}", method)
                    }
                }),
                false,
            ),
        };

        // Inject the request id into the response
        let response = inject_id(response, id);
        write_response(&mut stdout, &response).await;

        // Send tools/list_changed notification after the response
        if notify_tools_changed {
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed"
            });
            write_response(&mut stdout, &notification).await;
        }
    }
}

/// Handle `initialize` — return protocol version, capabilities, and server info.
fn handle_initialize(request: &Value) -> Value {
    let _params = request.get("params");
    json!({
        "jsonrpc": "2.0",
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": { "listChanged": true }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION
            }
        }
    })
}

/// Handle `tools/list` — return all tool definitions (triggers lazy playbook load).
async fn handle_tools_list(registry: &DeviceRegistry, pb_reg: &PlaybookRegistry) -> Value {
    pb_reg.ensure_loaded(registry.clients()).await;
    json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": tools::all_tool_definitions(pb_reg).await
        }
    })
}

/// Handle `tools/call` — dispatch to the appropriate tool handler.
///
/// Returns `(response, tools_changed)`. When `tools_changed` is true, the caller
/// should send a `notifications/tools/list_changed` notification after the response.
async fn handle_tools_call(
    request: &Value,
    registry: &DeviceRegistry,
    pb_reg: &PlaybookRegistry,
) -> (Value, bool) {
    let params = request.get("params").cloned().unwrap_or(json!({}));
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = tools::handle_tool_call(name, &args, registry, pb_reg).await;
    let tools_changed = result.tools_changed;

    let mut response_result = json!({
        "content": result.content
    });
    if result.is_error {
        response_result["isError"] = json!(true);
    }

    (
        json!({
            "jsonrpc": "2.0",
            "result": response_result
        }),
        tools_changed,
    )
}

/// Inject the request `id` into a response object.
fn inject_id(mut response: Value, id: Option<Value>) -> Value {
    if let Some(id) = id {
        response["id"] = id;
    }
    response
}

/// Write a JSON-RPC response to stdout (one line, flushed immediately).
async fn write_response(stdout: &mut tokio::io::Stdout, response: &Value) {
    let mut output = serde_json::to_string(response).unwrap_or_default();
    output.push('\n');
    if let Err(e) = stdout.write_all(output.as_bytes()).await {
        eprintln!("mcp-sctl: stdout write error: {}", e);
    }
    if let Err(e) = stdout.flush().await {
        eprintln!("mcp-sctl: stdout flush error: {}", e);
    }
}
