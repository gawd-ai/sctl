//! MCP tool definitions and handlers.
//!
//! Each tool is defined as a JSON schema (returned by [`builtin_tool_definitions`])
//! and handled by an async function dispatched from [`handle_tool_call`].
//!
//! ## Tool categories
//!
//! **Device tools** use the HTTP REST API via [`SctlClient`](crate::client::SctlClient):
//! - `device_list`, `device_health`, `device_info`
//! - `device_exec`, `device_exec_batch`
//! - `device_file_read`, `device_file_write`
//!
//! **Session tools** use the WebSocket API via [`DeviceWsConnection`](crate::websocket::DeviceWsConnection):
//! - `session_start`, `session_exec`, `session_send`
//! - `session_read`, `session_signal`, `session_kill`
//!
//! **Playbook management tools** (always present):
//! - `playbook_list`, `playbook_get`, `playbook_put`
//!
//! **Dynamic playbook tools** (`pb_*`): one per playbook discovered on devices.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::devices::DeviceRegistry;
use crate::playbook_registry::PlaybookRegistry;
use crate::playbooks;

/// Returns all tool definitions: builtins + playbook management + dynamic pb_* tools.
pub async fn all_tool_definitions(pb_reg: &PlaybookRegistry) -> Vec<Value> {
    let mut tools = builtin_tool_definitions();
    tools.extend(playbook_management_tool_definitions());
    for pb in pb_reg.all_playbooks().await {
        tools.push(playbooks::playbook_to_tool_definition(&pb));
    }
    tools
}

/// Returns the built-in (non-playbook) tool definitions.
fn builtin_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "device_list",
            "description": "List configured sctl devices and their connection status.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "device_health",
            "description": "Check if a sctl device is alive. Returns uptime and version. Does not require authentication.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "device_info",
            "description": "Get system information from a sctl device: hostname, IPs, CPU, memory, disk, network interfaces.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "device_exec",
            "description": "Execute a shell command on a sctl device and return stdout, stderr, and exit code.\n\nIMPORTANT: If you have already attached to or been given a session in this conversation, prefer using session_exec or session_exec_wait in that session instead. Sessions are visible to the user in the terminal UI (sctlin), so working in a session lets the user watch your progress in real time. Only use device_exec when no session has been established in the conversation or when you explicitly need an independent execution context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Command timeout in milliseconds. Default is 30000 (30s)."
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Working directory for the command (absolute path)."
                    },
                    "env": {
                        "type": "object",
                        "description": "Environment variables to set for the command.",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "device_exec_batch",
            "description": "Execute multiple shell commands sequentially on a sctl device. Returns results for each command.\n\nIMPORTANT: If you have already attached to or been given a session in this conversation, prefer using session_exec in that session instead. Sessions are visible to the user in the terminal UI (sctlin), so working in a session lets the user watch your progress in real time. Only use device_exec_batch when no session has been established in the conversation or when you explicitly need independent execution.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commands": {
                        "type": "array",
                        "description": "Array of commands to execute. Each item can be a string (simple command) or an object with 'command', 'timeout_ms', 'working_dir', 'env' fields.",
                        "items": {
                            "oneOf": [
                                { "type": "string" },
                                {
                                    "type": "object",
                                    "properties": {
                                        "command": { "type": "string" },
                                        "timeout_ms": { "type": "integer" },
                                        "working_dir": { "type": "string" },
                                        "env": {
                                            "type": "object",
                                            "additionalProperties": { "type": "string" }
                                        }
                                    },
                                    "required": ["command"]
                                }
                            ]
                        }
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Default working directory for all commands."
                    },
                    "env": {
                        "type": "object",
                        "description": "Default environment variables for all commands.",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["commands"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "device_file_read",
            "description": "Read a file or list a directory on a sctl device. For directories, set list=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the file or directory."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    },
                    "list": {
                        "type": "boolean",
                        "description": "If true, list directory entries instead of reading file content. Default false."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "device_file_write",
            "description": "Write content to a file on a sctl device. The write is atomic (temp file + rename).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path for the file to write."
                    },
                    "content": {
                        "type": "string",
                        "description": "File content. Plain text by default, or base64 if encoding='base64'."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    },
                    "encoding": {
                        "type": "string",
                        "description": "Content encoding: omit for UTF-8 text, or 'base64' for binary.",
                        "enum": ["base64"]
                    },
                    "mode": {
                        "type": "string",
                        "description": "File permissions as octal string, e.g. '0644'."
                    },
                    "create_dirs": {
                        "type": "boolean",
                        "description": "Create parent directories if they don't exist. Default false."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_start",
            "description": "Start a persistent interactive shell session on a device. Returns a session_id for subsequent calls. Sessions are NEVER killed automatically unless you set idle_timeout. Use idle_timeout to control cleanup of sessions you may not return to.\n\nSession lifecycle:\n- idle_timeout=0 (default): session lives forever until explicitly killed via session_kill\n- idle_timeout=N: session is gracefully terminated after N seconds of inactivity while detached (no client connected)\n- For long-running work, use 0 or a high value (3600). For quick one-off commands, use a lower value (300-600).\n- Activity resets whenever you send input or re-attach.\n\nSet pty=true for full terminal emulation (TUI programs like nano, vi, htop).\n\nPTY workflow:\n1. session_exec to run commands — works in both shell prompts and TUI programs (auto-appends Enter)\n2. session_send for raw keystrokes without Enter (arrow keys, Ctrl combos, Escape sequences)\n3. session_read to see output (contains ANSI escape codes in PTY mode)\n\nSee session_send description for full list of control characters and escape sequences (arrow keys, function keys, navigation keys, etc.).\n\nPTY workflow for TUI programs (Claude Code, fzf, dialog, etc.):\n- Use session_send to type text (no Enter appended)\n- Use session_send with \\n to press Enter (auto-translated to \\r)\n- Do NOT use session_exec for TUI input fields — it sends text+Enter as one write, which TUIs may interpret as embedded newline rather than submit",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Initial working directory (absolute path)."
                    },
                    "shell": {
                        "type": "string",
                        "description": "Shell binary, e.g. '/bin/bash'. Defaults to device's configured shell."
                    },
                    "env": {
                        "type": "object",
                        "description": "Environment variables to set.",
                        "additionalProperties": { "type": "string" }
                    },
                    "persistent": {
                        "type": "boolean",
                        "description": "If true, session survives WebSocket disconnects (default true)."
                    },
                    "pty": {
                        "type": "boolean",
                        "description": "Use PTY for full terminal emulation (default false). Enables TUI programs, isatty() detection, and ANSI color output."
                    },
                    "rows": {
                        "type": "integer",
                        "description": "Terminal rows (default 24, only used when pty=true)."
                    },
                    "cols": {
                        "type": "integer",
                        "description": "Terminal columns (default 80, only used when pty=true)."
                    },
                    "idle_timeout": {
                        "type": "integer",
                        "description": "Seconds of inactivity (while detached) before the server gracefully kills this session. 0 = never auto-kill (default). Use 300-600 for throwaway sessions, 0 or 3600+ for long-lived work."
                    },
                    "name": {
                        "type": "string",
                        "description": "Human-readable name for the session. Optional. Helps identify sessions in multi-client environments."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_exec",
            "description": "Execute a command in an existing interactive session and press Enter. The server automatically uses the correct line ending (\\r for PTY, \\n for pipe sessions), so this works in both shell prompts and TUI programs. Use session_read to get output. For raw keystrokes without Enter (arrow keys, Ctrl combos, Escape), use session_send instead.\n\nIMPORTANT for TUI programs (like Claude Code, fzf, dialog): session_exec sends text+Enter as a single write. Some TUI input fields treat this as \"insert text with embedded newline\" rather than \"type text then submit\". For TUI programs, prefer session_send with text first, then a separate session_send with \\n (translated to Enter) to submit. session_exec is best for shell prompts where command+Enter is the standard pattern.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "command": {
                        "type": "string",
                        "description": "Command to execute."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id", "command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_send",
            "description": "Send raw data to a session's stdin. No newline is appended — use session_exec instead if you want to run a command. In PTY mode, \\n is automatically translated to \\r (Enter key). Common control chars: Ctrl+C=\\u0003, Ctrl+D=\\u0004, Ctrl+Z=\\u001a, Ctrl+O=\\u000f, Ctrl+X=\\u0018, Ctrl+\\\\=\\u001c, Escape=\\u001b, Tab=\\t. Arrow keys (ANSI escape sequences): Up=\\u001b[A, Down=\\u001b[B, Right=\\u001b[C, Left=\\u001b[D. Other navigation: Home=\\u001b[H, End=\\u001b[F, PageUp=\\u001b[5~, PageDown=\\u001b[6~, Insert=\\u001b[2~, Delete=\\u001b[3~. Function keys: F1=\\u001bOP, F2=\\u001bOQ, F3=\\u001bOR, F4=\\u001bOS, F5=\\u001b[15~, F6=\\u001b[17~. Enter key (\\n) is auto-translated to \\r for PTY sessions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "data": {
                        "type": "string",
                        "description": "Raw data to send to stdin."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id", "data"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_read",
            "description": "Read buffered output from a session. Returns entries since the given sequence number. In PTY mode, output contains ANSI escape codes for cursor movement, colors, etc. After sending input, allow 0.5-2s before reading to let the program process and render.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "since": {
                        "type": "integer",
                        "description": "Sequence number. Returns entries with seq > since. Use 0 to read from the beginning, or last_seq from a previous read."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Max milliseconds to wait for new output. Default 5000."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_signal",
            "description": "Send a POSIX signal to a session's process group. Common signals: 2=SIGINT (Ctrl-C), 15=SIGTERM, 9=SIGKILL.\n\nNote: In non-PTY sessions, signals are delivered to the entire process group including the shell, which will typically terminate the session. Use PTY sessions for interactive signal handling where the kernel provides job control protection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "signal": {
                        "type": "integer",
                        "description": "Signal number (e.g. 2 for SIGINT, 15 for SIGTERM, 9 for SIGKILL)."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id", "signal"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_kill",
            "description": "Kill a session and its process group. The session is permanently destroyed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_resize",
            "description": "Resize the terminal for a PTY session. Only works for sessions started with pty=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "rows": {
                        "type": "integer",
                        "description": "New terminal rows."
                    },
                    "cols": {
                        "type": "integer",
                        "description": "New terminal columns."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id", "rows", "cols"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_list",
            "description": "List all sessions on a sctl device. Shows session IDs, status, PTY mode, idle time, and whether each session is currently attached. Returns all sessions on the server, including those created by other clients.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to list sessions across all connected devices."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_exec_wait",
            "description": "Execute a command in a session and wait for it to complete. Returns the full output and exit code in a single call — no need for separate session_exec + session_read. Uses a marker-based completion detection approach. Best for non-interactive commands with finite output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "command": {
                        "type": "string",
                        "description": "Command to execute."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Max milliseconds to wait for completion. Default 30000 (30s)."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id", "command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_attach",
            "description": "Re-attach to an existing persistent session. Use this after MCP restart to reconnect to sessions that are still alive on the daemon. Combined with session_list to discover session IDs. Returns buffered output since the given sequence number.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID to attach to."
                    },
                    "since": {
                        "type": "integer",
                        "description": "Sequence number. Returns entries with seq > since. Use 0 to replay from the beginning."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_rename",
            "description": "Rename a session with a human-readable name. Other connected clients will see the name update in real-time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID to rename."
                    },
                    "name": {
                        "type": "string",
                        "description": "New human-readable name for the session."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id", "name"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_allow_ai",
            "description": "Set whether AI is permitted to control a session. This is the human-side toggle — the user grants or revokes AI access. AI agents should NOT call this on themselves; it is meant for the human operator (typically via the terminal UI). If allowed=false while the AI is working, the server automatically clears the AI working state and notifies all clients.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "allowed": {
                        "type": "boolean",
                        "description": "true = permit AI to work in this session, false = revoke AI access (also force-stops any active AI work)."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id", "allowed"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "session_ai_status",
            "description": "Report AI working status for a session. Call this to tell the terminal UI what you are doing.\n\nWorkflow:\n1. Before starting work: call with working=true, activity, and message.\n2. While working: call again to update activity or message as your task changes.\n3. When done: call with working=false to clear the status.\n\nEnforcement: working=true will fail if the user has not allowed AI for this session (via session_allow_ai). Always handle this error gracefully.\n\nVisual effect in the terminal UI:\n- activity='read' → blue border, 'AI Reading' badge (use for inspection, reading files, no side effects)\n- activity='write' → green border, 'AI Executing' badge (use when running commands, writing files, making changes)\n- message is shown in the status bar (e.g. 'Running tests', 'Reading config')\n\nWhile working=true, keyboard input from the human is blocked on that session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID from session_start."
                    },
                    "working": {
                        "type": "boolean",
                        "description": "true = AI is actively working in this session, false = AI is done (clears activity and message)."
                    },
                    "activity": {
                        "type": "string",
                        "description": "What kind of work the AI is doing. 'read' = inspecting/reading (no side effects), 'write' = executing commands or making changes (has side effects). Determines the visual indicator color in the terminal UI. Only used when working=true.",
                        "enum": ["read", "write"]
                    },
                    "message": {
                        "type": "string",
                        "description": "Short human-readable status message displayed in the terminal status bar (e.g. 'Running tests', 'Installing dependencies'). Keep under 50 chars. Only used when working=true."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["session_id", "working"],
                "additionalProperties": false
            }
        }),
    ]
}

/// Playbook management tool definitions (always present).
fn playbook_management_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "playbook_list",
            "description": "List playbooks from one or all devices. Always fetches fresh from device (also refreshes the dynamic tool cache).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to list playbooks from all devices."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "playbook_get",
            "description": "Get the full Markdown content of a playbook file from a device.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Playbook name (without .md extension)."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "playbook_put",
            "description": "Create, update, or delete a playbook on a device. Non-empty content = create/update. Empty content = delete.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Playbook name (without .md extension)."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full Markdown content. Empty string = delete the playbook."
                    },
                    "device": {
                        "type": "string",
                        "description": "Device name. Omit to use the default device."
                    }
                },
                "required": ["name", "content"],
                "additionalProperties": false
            }
        }),
    ]
}

/// Handle a tool call and return MCP content.
pub async fn handle_tool_call(
    name: &str,
    args: &Value,
    registry: &DeviceRegistry,
    pb_reg: &PlaybookRegistry,
) -> ToolResult {
    match name {
        "device_list" => handle_device_list(registry),
        "device_health" => handle_device_health(args, registry).await,
        "device_info" => handle_device_info(args, registry).await,
        "device_exec" => handle_device_exec(args, registry).await,
        "device_exec_batch" => handle_device_exec_batch(args, registry).await,
        "device_file_read" => handle_device_file_read(args, registry).await,
        "device_file_write" => handle_device_file_write(args, registry).await,
        "session_start" => handle_session_start(args, registry).await,
        "session_exec" => handle_session_exec(args, registry).await,
        "session_send" => handle_session_send(args, registry).await,
        "session_read" => handle_session_read(args, registry).await,
        "session_signal" => handle_session_signal(args, registry).await,
        "session_kill" => handle_session_kill(args, registry).await,
        "session_resize" => handle_session_resize(args, registry).await,
        "session_list" => handle_session_list(args, registry).await,
        "session_exec_wait" => handle_session_exec_wait(args, registry).await,
        "session_attach" => handle_session_attach(args, registry).await,
        "session_rename" => handle_session_rename(args, registry).await,
        "session_allow_ai" => handle_session_allow_ai(args, registry).await,
        "session_ai_status" => handle_session_ai_status(args, registry).await,
        "playbook_list" => handle_playbook_list(args, registry, pb_reg).await,
        "playbook_get" => handle_playbook_get(args, registry, pb_reg).await,
        "playbook_put" => handle_playbook_put(args, registry, pb_reg).await,
        _ if name.starts_with("pb_") => handle_playbook_exec(name, args, registry, pb_reg).await,
        _ => ToolResult::error(format!("Unknown tool: {}", name)),
    }
}

/// Result of an MCP tool call, ready to be serialized into a JSON-RPC response.
pub struct ToolResult {
    /// MCP content blocks (typically a single `{"type":"text","text":"..."}` entry).
    pub content: Vec<Value>,
    /// Whether the tool call failed (maps to `isError` in the MCP response).
    pub is_error: bool,
    /// If true, the caller should send a `notifications/tools/list_changed`.
    pub tools_changed: bool,
}

impl ToolResult {
    fn success(value: Value) -> Self {
        let text = serde_json::to_string_pretty(&value).unwrap_or_default();
        Self {
            content: vec![json!({ "type": "text", "text": text })],
            is_error: false,
            tools_changed: false,
        }
    }

    fn error(message: String) -> Self {
        Self {
            content: vec![json!({ "type": "text", "text": message })],
            is_error: true,
            tools_changed: false,
        }
    }
}

fn get_device_param(args: &Value) -> Option<&str> {
    args.get("device").and_then(Value::as_str)
}

fn handle_device_list(registry: &DeviceRegistry) -> ToolResult {
    let devices: Vec<Value> = registry
        .list()
        .into_iter()
        .map(|d| json!({ "name": d.name, "url": d.url }))
        .collect();

    ToolResult::success(json!({
        "devices": devices,
        "default_device": registry.default_device()
    }))
}

async fn handle_device_health(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let client = match registry.resolve(get_device_param(args)) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(e),
    };
    match client.health().await {
        Ok(v) => ToolResult::success(v),
        Err(e) => ToolResult::error(e.to_string()),
    }
}

async fn handle_device_info(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let client = match registry.resolve(get_device_param(args)) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(e),
    };
    match client.info().await {
        Ok(v) => ToolResult::success(v),
        Err(e) => ToolResult::error(e.to_string()),
    }
}

async fn handle_device_exec(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let client = match registry.resolve(get_device_param(args)) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(e),
    };

    let command = match args.get("command").and_then(Value::as_str) {
        Some(c) => c,
        None => return ToolResult::error("Missing required parameter: command".into()),
    };

    let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64);
    let working_dir = args.get("working_dir").and_then(Value::as_str);
    let env: Option<HashMap<String, String>> = args
        .get("env")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    match client
        .exec(command, timeout_ms, working_dir, env.as_ref())
        .await
    {
        Ok(v) => ToolResult::success(v),
        Err(e) => ToolResult::error(e.to_string()),
    }
}

async fn handle_device_exec_batch(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let client = match registry.resolve(get_device_param(args)) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(e),
    };

    let commands = match args.get("commands").and_then(Value::as_array) {
        Some(c) => c,
        None => return ToolResult::error("Missing required parameter: commands (array)".into()),
    };

    // Normalize: strings become { "command": "..." } objects
    let normalized: Vec<Value> = commands
        .iter()
        .map(|c| {
            if let Some(s) = c.as_str() {
                json!({ "command": s })
            } else {
                c.clone()
            }
        })
        .collect();

    let working_dir = args.get("working_dir").and_then(Value::as_str);
    let env: Option<HashMap<String, String>> = args
        .get("env")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    match client
        .exec_batch(&normalized, working_dir, env.as_ref())
        .await
    {
        Ok(v) => ToolResult::success(v),
        Err(e) => ToolResult::error(e.to_string()),
    }
}

async fn handle_device_file_read(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let client = match registry.resolve(get_device_param(args)) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(e),
    };

    let path = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return ToolResult::error("Missing required parameter: path".into()),
    };

    let list = args.get("list").and_then(Value::as_bool).unwrap_or(false);

    match client.file_read(path, list).await {
        Ok(v) => ToolResult::success(v),
        Err(e) => ToolResult::error(e.to_string()),
    }
}

async fn handle_device_file_write(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let client = match registry.resolve(get_device_param(args)) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(e),
    };

    let path = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return ToolResult::error("Missing required parameter: path".into()),
    };

    let content = match args.get("content").and_then(Value::as_str) {
        Some(c) => c,
        None => return ToolResult::error("Missing required parameter: content".into()),
    };

    let encoding = args.get("encoding").and_then(Value::as_str);
    let mode = args.get("mode").and_then(Value::as_str);
    let create_dirs = args.get("create_dirs").and_then(Value::as_bool);

    match client
        .file_write(path, content, encoding, mode, create_dirs)
        .await
    {
        Ok(v) => ToolResult::success(v),
        Err(e) => ToolResult::error(e.to_string()),
    }
}

// --- Session tools ---

async fn get_ws_connection(
    args: &Value,
    registry: &DeviceRegistry,
) -> Result<std::sync::Arc<crate::websocket::DeviceWsConnection>, ToolResult> {
    // If no explicit device, try to auto-route by session_id
    let device = match get_device_param(args) {
        Some(d) => Some(d.to_string()),
        None => {
            if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
                registry.resolve_session_device(sid).await
            } else {
                None
            }
        }
    };
    let (name, client) =
        match registry.resolve_with_name(device.as_deref()) {
            Ok(v) => v,
            Err(e) => return Err(ToolResult::error(e)),
        };
    registry
        .ws_pool
        .get_or_connect(name, client)
        .await
        .map_err(|e| ToolResult::error(format!("WebSocket connection failed: {e}")))
}

async fn handle_session_start(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let working_dir = args.get("working_dir").and_then(Value::as_str);
    let shell = args.get("shell").and_then(Value::as_str);
    let env: Option<HashMap<String, String>> = args
        .get("env")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let persistent = args
        .get("persistent")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let use_pty = args.get("pty").and_then(Value::as_bool).unwrap_or(false);
    let rows = args.get("rows").and_then(Value::as_u64);
    let cols = args.get("cols").and_then(Value::as_u64);
    let idle_timeout = args.get("idle_timeout").and_then(Value::as_u64);
    let name = args.get("name").and_then(Value::as_str);

    match ws
        .start_session(
            working_dir,
            shell,
            env.as_ref(),
            persistent,
            use_pty,
            rows,
            cols,
            idle_timeout,
            name,
            true,
        )
        .await
    {
        Ok(v) => {
            // Check if it's an error response
            if v["type"].as_str() == Some("error") {
                ToolResult::error(
                    v["message"]
                        .as_str()
                        .unwrap_or("Session start failed")
                        .to_string(),
                )
            } else {
                // Register session→device mapping for auto-routing
                if let Some(sid) = v["session_id"].as_str() {
                    let device_name =
                        get_device_param(args).unwrap_or(registry.default_device());
                    registry.register_session(sid, device_name).await;
                }
                let mut result = json!({
                    "session_id": v["session_id"],
                    "pid": v["pid"],
                    "persistent": v["persistent"],
                    "pty": v["pty"],
                    "user_allows_ai": v["user_allows_ai"],
                });
                if let Some(n) = v["name"].as_str() {
                    result["name"] = json!(n);
                }
                ToolResult::success(result)
            }
        }
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_exec(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let command = match args.get("command").and_then(Value::as_str) {
        Some(c) => c,
        None => return ToolResult::error("Missing required parameter: command".into()),
    };

    // Auto-set AI working status
    ws.auto_set_ai_working(session_id, "write").await;

    match ws
        .send(json!({
            "type": "session.exec",
            "session_id": session_id,
            "command": command,
        }))
        .await
    {
        Ok(()) => ToolResult::success(json!({ "ok": true })),
        Err(e) => ToolResult::error(e),
    }
}

/// Interpret literal escape sequences that MCP clients may send as text.
///
/// When an AI agent writes `\u0003` in a tool argument, many MCP transports
/// deliver the 6-character literal string `\u0003` rather than the single
/// Unicode code-point U+0003.  This function converts those literal sequences
/// back to their intended bytes.
///
/// Handled patterns:
/// - `\uXXXX` — 4-hex-digit Unicode escape (e.g. `\u0003` → Ctrl-C)
/// - `\t` → tab, `\n` → newline, `\r` → carriage return, `\\` → backslash
///
/// Already-decoded bytes (e.g. raw 0x03) pass through unchanged because they
/// don't start with a literal backslash character.
fn unescape_control_chars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        // We saw a literal backslash — peek at the next char
        match chars.next() {
            Some('u') => {
                // Collect exactly 4 hex digits
                let mut hex = String::with_capacity(4);
                for _ in 0..4 {
                    match chars.next() {
                        Some(h) if h.is_ascii_hexdigit() => hex.push(h),
                        Some(other) => {
                            // Not valid hex — emit what we consumed literally
                            out.push('\\');
                            out.push('u');
                            out.push_str(&hex);
                            out.push(other);
                            hex.clear();
                            break;
                        }
                        None => {
                            out.push('\\');
                            out.push('u');
                            out.push_str(&hex);
                            hex.clear();
                            break;
                        }
                    }
                }
                if hex.len() == 4 {
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    } else {
                        // Invalid code-point — emit literally
                        out.push('\\');
                        out.push('u');
                        out.push_str(&hex);
                    }
                }
            }
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                // Unknown escape — keep literal
                out.push('\\');
                out.push(other);
            }
            None => {
                // Trailing backslash
                out.push('\\');
            }
        }
    }
    out
}

async fn handle_session_send(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let raw_data = match args.get("data").and_then(Value::as_str) {
        Some(d) => d,
        None => return ToolResult::error("Missing required parameter: data".into()),
    };

    // Auto-set AI working status
    ws.auto_set_ai_working(session_id, "write").await;

    // First: interpret literal escape sequences (e.g. `\u0003` → Ctrl-C).
    // MCP clients often send these as literal text rather than decoded bytes.
    let unescaped = unescape_control_chars(raw_data);

    // PTY sessions expect \r for Enter (like real terminal emulators)
    let data = if ws.is_pty_session(session_id).await {
        unescaped.replace('\n', "\r")
    } else {
        unescaped
    };

    match ws
        .send(json!({
            "type": "session.stdin",
            "session_id": session_id,
            "data": data,
        }))
        .await
    {
        Ok(()) => ToolResult::success(json!({ "ok": true })),
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_read(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let since = args.get("since").and_then(Value::as_u64).unwrap_or(0);
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(5000);

    // Auto-set AI working status (read activity)
    ws.auto_set_ai_working(session_id, "read").await;

    match ws.read_output(session_id, since, timeout_ms).await {
        Ok(result) => {
            let entries: Vec<Value> = result
                .entries
                .iter()
                .map(|e| {
                    json!({
                        "seq": e.seq,
                        "stream": e.stream,
                        "data": e.data,
                        "timestamp_ms": e.timestamp_ms,
                    })
                })
                .collect();

            let last_seq = result.entries.last().map_or(since, |e| e.seq);

            let status = match result.status {
                crate::websocket::SessionStatus::Running => "running",
                crate::websocket::SessionStatus::Exited => "exited",
            };

            ToolResult::success(json!({
                "entries": entries,
                "last_seq": last_seq,
                "status": status,
                "exit_code": result.exit_code,
                "dropped_entries": result.dropped_count,
            }))
        }
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_signal(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let signal = match args.get("signal").and_then(Value::as_i64) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: signal".into()),
    };

    match ws
        .send(json!({
            "type": "session.signal",
            "session_id": session_id,
            "signal": signal,
        }))
        .await
    {
        Ok(()) => ToolResult::success(json!({ "ok": true })),
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_kill(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };

    match ws
        .send(json!({
            "type": "session.kill",
            "session_id": session_id,
        }))
        .await
    {
        Ok(()) => ToolResult::success(json!({ "ok": true })),
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_resize(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let rows = match args.get("rows").and_then(Value::as_u64) {
        Some(r) => r,
        None => return ToolResult::error("Missing required parameter: rows".into()),
    };
    let cols = match args.get("cols").and_then(Value::as_u64) {
        Some(c) => c,
        None => return ToolResult::error("Missing required parameter: cols".into()),
    };

    match ws
        .send(json!({
            "type": "session.resize",
            "session_id": session_id,
            "rows": rows,
            "cols": cols,
        }))
        .await
    {
        Ok(()) => ToolResult::success(json!({ "ok": true, "rows": rows, "cols": cols })),
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_list(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let device_filter = get_device_param(args);

    let mut all_sessions: Vec<(String, Value)> = Vec::new();

    if let Some(device) = device_filter {
        let ws = match get_ws_connection(args, registry).await {
            Ok(ws) => ws,
            Err(e) => return e,
        };
        match ws.list_sessions_remote().await {
            Ok(resp) => {
                if let Some(sessions) = resp["sessions"].as_array() {
                    for s in sessions {
                        all_sessions.push((device.to_string(), s.clone()));
                    }
                }
            }
            Err(e) => return ToolResult::error(e),
        }
    } else {
        // Iterate all configured devices (not just already-connected ones)
        for (device_name, client) in registry.clients() {
            let conn = match registry.ws_pool.get_or_connect(device_name, client).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("mcp-sctl: Failed to connect to {device_name}: {e}");
                    continue;
                }
            };
            match conn.list_sessions_remote().await {
                Ok(resp) => {
                    if let Some(sessions) = resp["sessions"].as_array() {
                        for s in sessions {
                            all_sessions.push((device_name.clone(), s.clone()));
                        }
                    }
                }
                Err(e) => {
                    eprintln!("mcp-sctl: Failed to list sessions on {device_name}: {e}");
                }
            }
        }
    }

    // Register session→device mappings for auto-routing
    for (device, s) in &all_sessions {
        if let Some(sid) = s["session_id"].as_str() {
            registry.register_session(sid, device).await;
        }
    }

    let sessions: Vec<Value> = all_sessions
        .into_iter()
        .map(|(device, mut s)| {
            s["device"] = json!(device);
            s
        })
        .collect();

    let count = sessions.len();
    ToolResult::success(json!({
        "sessions": sessions,
        "count": count,
    }))
}

async fn handle_session_exec_wait(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let command = match args.get("command").and_then(Value::as_str) {
        Some(c) => c,
        None => return ToolResult::error("Missing required parameter: command".into()),
    };
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30000);

    // Auto-set AI working status
    ws.auto_set_ai_working(session_id, "write").await;

    match ws.exec_wait(session_id, command, timeout_ms).await {
        Ok(result) => ToolResult::success(json!({
            "output": result.output,
            "exit_code": result.exit_code,
            "timed_out": result.timed_out,
        })),
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_attach(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let since = args.get("since").and_then(Value::as_u64).unwrap_or(0);

    match ws.attach_session(session_id, since).await {
        Ok(result) => {
            let entries: Vec<Value> = result
                .entries
                .iter()
                .map(|e| {
                    json!({
                        "seq": e.seq,
                        "stream": e.stream,
                        "data": e.data,
                        "timestamp_ms": e.timestamp_ms,
                    })
                })
                .collect();

            let last_seq = result.entries.last().map_or(since, |e| e.seq);

            let status = match result.status {
                crate::websocket::SessionStatus::Running => "running",
                crate::websocket::SessionStatus::Exited => "exited",
            };

            ToolResult::success(json!({
                "session_id": session_id,
                "entries": entries,
                "last_seq": last_seq,
                "status": status,
                "exit_code": result.exit_code,
            }))
        }
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_rename(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let name = match args.get("name").and_then(Value::as_str) {
        Some(n) => n,
        None => return ToolResult::error("Missing required parameter: name".into()),
    };

    match ws
        .send(json!({
            "type": "session.rename",
            "session_id": session_id,
            "name": name,
        }))
        .await
    {
        Ok(()) => ToolResult::success(json!({
            "ok": true,
            "session_id": session_id,
            "name": name,
        })),
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_allow_ai(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let allowed = match args.get("allowed").and_then(Value::as_bool) {
        Some(a) => a,
        None => return ToolResult::error("Missing required parameter: allowed (bool)".into()),
    };

    match ws
        .send(json!({
            "type": "session.allow_ai",
            "session_id": session_id,
            "allowed": allowed,
        }))
        .await
    {
        Ok(()) => ToolResult::success(json!({
            "ok": true,
            "session_id": session_id,
            "allowed": allowed,
        })),
        Err(e) => ToolResult::error(e),
    }
}

async fn handle_session_ai_status(args: &Value, registry: &DeviceRegistry) -> ToolResult {
    let ws = match get_ws_connection(args, registry).await {
        Ok(ws) => ws,
        Err(e) => return e,
    };

    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::error("Missing required parameter: session_id".into()),
    };
    let working = match args.get("working").and_then(Value::as_bool) {
        Some(w) => w,
        None => return ToolResult::error("Missing required parameter: working (bool)".into()),
    };
    let activity = args.get("activity").and_then(Value::as_str);
    let message = args.get("message").and_then(Value::as_str);

    match ws
        .set_ai_status(session_id, working, activity, message)
        .await
    {
        Ok(v) => {
            // Sync local AI working tracking
            if working {
                ws.mark_ai_working(session_id).await;
            } else {
                ws.clear_ai_working(session_id).await;
            }
            let mut result = json!({
                "ok": true,
                "session_id": v["session_id"].as_str().unwrap_or(session_id),
                "working": v["working"].as_bool().unwrap_or(working),
            });
            if let Some(a) = v["activity"].as_str() {
                result["activity"] = json!(a);
            }
            if let Some(m) = v["message"].as_str() {
                result["message"] = json!(m);
            }
            ToolResult::success(result)
        }
        Err(e) => ToolResult::error(e),
    }
}

// --- Playbook tools ---

/// Get the playbooks directory for a device from the registry config.
fn get_playbooks_dir(device: &str, pb_reg: &PlaybookRegistry) -> String {
    pb_reg.dir_for_device(device).to_string()
}

async fn handle_playbook_list(
    args: &Value,
    registry: &DeviceRegistry,
    pb_reg: &PlaybookRegistry,
) -> ToolResult {
    let device = get_device_param(args);

    let playbooks = if let Some(dev) = device {
        let client = match registry.resolve(Some(dev)) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e),
        };
        pb_reg.refresh_device(dev, client).await
    } else {
        pb_reg.refresh_all(registry.clients()).await
    };

    let items: Vec<Value> = playbooks
        .iter()
        .map(|pb| {
            json!({
                "name": pb.name,
                "tool_name": pb.tool_name(),
                "description": pb.description,
                "device": pb.source_device,
                "path": pb.source_path,
                "params": pb.params.keys().collect::<Vec<_>>(),
            })
        })
        .collect();

    ToolResult::success(json!({
        "playbooks": items,
        "count": items.len(),
    }))
}

async fn handle_playbook_get(
    args: &Value,
    registry: &DeviceRegistry,
    pb_reg: &PlaybookRegistry,
) -> ToolResult {
    let name = match args.get("name").and_then(Value::as_str) {
        Some(n) => n,
        None => return ToolResult::error("Missing required parameter: name".into()),
    };
    if let Err(e) = playbooks::validate_name(name) {
        return ToolResult::error(e);
    }

    let (dev_name, client) = match registry.resolve_with_name(get_device_param(args)) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e),
    };

    let dir = get_playbooks_dir(dev_name, pb_reg);
    let path = format!("{}/{}.md", dir, name);

    match client.file_read(&path, false).await {
        Ok(v) => {
            let content = v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or_default();
            ToolResult::success(json!({
                "name": name,
                "device": dev_name,
                "path": path,
                "content": content,
            }))
        }
        Err(e) => ToolResult::error(format!("Cannot read playbook '{}': {}", name, e)),
    }
}

async fn handle_playbook_put(
    args: &Value,
    registry: &DeviceRegistry,
    pb_reg: &PlaybookRegistry,
) -> ToolResult {
    let name = match args.get("name").and_then(Value::as_str) {
        Some(n) => n,
        None => return ToolResult::error("Missing required parameter: name".into()),
    };
    if let Err(e) = playbooks::validate_name(name) {
        return ToolResult::error(e);
    }

    let content = match args.get("content").and_then(Value::as_str) {
        Some(c) => c,
        None => return ToolResult::error("Missing required parameter: content".into()),
    };

    let (dev_name, client) = match registry.resolve_with_name(get_device_param(args)) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(e),
    };

    let dir = get_playbooks_dir(dev_name, pb_reg);
    let path = format!("{}/{}.md", dir, name);

    if content.is_empty() {
        // Delete
        let rm_cmd = format!("rm -f '{}'", path);
        match client.exec(&rm_cmd, None, None, None).await {
            Ok(_) => {
                pb_reg.invalidate_device(dev_name).await;
                let mut result = ToolResult::success(json!({
                    "action": "deleted",
                    "name": name,
                    "device": dev_name,
                    "path": path,
                }));
                result.tools_changed = true;
                result
            }
            Err(e) => ToolResult::error(format!("Cannot delete playbook '{}': {}", name, e)),
        }
    } else {
        // Validate before writing
        if let Err(e) = playbooks::parse_playbook(content, dev_name, &path) {
            return ToolResult::error(format!("Invalid playbook: {}", e));
        }

        match client
            .file_write(&path, content, None, None, Some(true))
            .await
        {
            Ok(_) => {
                pb_reg.invalidate_device(dev_name).await;
                let mut result = ToolResult::success(json!({
                    "action": "saved",
                    "name": name,
                    "device": dev_name,
                    "path": path,
                }));
                result.tools_changed = true;
                result
            }
            Err(e) => ToolResult::error(format!("Cannot write playbook '{}': {}", name, e)),
        }
    }
}

async fn handle_playbook_exec(
    tool_name: &str,
    args: &Value,
    registry: &DeviceRegistry,
    pb_reg: &PlaybookRegistry,
) -> ToolResult {
    let pb = match pb_reg.find_by_tool_name(tool_name).await {
        Some(pb) => pb,
        None => {
            return ToolResult::error(format!(
                "Playbook tool '{}' not found. Try calling playbook_list to refresh.",
                tool_name
            ))
        }
    };

    // Determine target device: explicit arg > playbook's source device
    let device = get_device_param(args).unwrap_or(&pb.source_device);
    let client = match registry.resolve(Some(device)) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(e),
    };

    let script = match playbooks::render_script(&pb, args) {
        Ok(s) => s,
        Err(e) => return ToolResult::error(e),
    };

    match client.exec(&script, None, None, None).await {
        Ok(v) => ToolResult::success(json!({
            "playbook": pb.name,
            "device": device,
            "result": v,
            "script": script,
        })),
        Err(e) => ToolResult::error(format!(
            "Playbook '{}' execution failed: {}\n\nRendered script:\n{}",
            pb.name, e, script
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_ctrl_c() {
        assert_eq!(unescape_control_chars(r"\u0003"), "\x03");
    }

    #[test]
    fn unescape_ctrl_d() {
        assert_eq!(unescape_control_chars(r"\u0004"), "\x04");
    }

    #[test]
    fn unescape_tab() {
        assert_eq!(unescape_control_chars(r"\t"), "\t");
    }

    #[test]
    fn unescape_newline() {
        assert_eq!(unescape_control_chars(r"\n"), "\n");
    }

    #[test]
    fn unescape_carriage_return() {
        assert_eq!(unescape_control_chars(r"\r"), "\r");
    }

    #[test]
    fn unescape_backslash() {
        assert_eq!(unescape_control_chars(r"\\"), "\\");
    }

    #[test]
    fn unescape_mixed() {
        assert_eq!(
            unescape_control_chars(r"hello\u0003world\t!"),
            "hello\x03world\t!"
        );
    }

    #[test]
    fn unescape_plain_text_passthrough() {
        assert_eq!(unescape_control_chars("hello world"), "hello world");
    }

    #[test]
    fn unescape_already_decoded_byte() {
        // Raw byte 0x03 is not a backslash, so it passes through unchanged
        assert_eq!(unescape_control_chars("\x03"), "\x03");
    }

    #[test]
    fn unescape_invalid_hex_passthrough() {
        // \uZZZZ is not valid hex — passes through literally
        assert_eq!(unescape_control_chars(r"\uZZZZ"), r"\uZZZZ");
    }

    #[test]
    fn unescape_truncated_unicode() {
        // \u00 — only 2 hex digits, then end of string
        assert_eq!(unescape_control_chars(r"\u00"), r"\u00");
    }

    #[test]
    fn unescape_escape_key() {
        assert_eq!(unescape_control_chars(r"\u001b"), "\x1b");
    }

    #[test]
    fn unescape_unknown_escape_literal() {
        // \x is not a recognized escape — kept literally
        assert_eq!(unescape_control_chars(r"\x"), r"\x");
    }

    #[test]
    fn unescape_trailing_backslash() {
        assert_eq!(unescape_control_chars("test\\"), "test\\");
    }
}
