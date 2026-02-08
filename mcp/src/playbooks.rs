//! Playbook model, parsing, rendering, and tool definition generation.
//!
//! A playbook is a Markdown file with YAML frontmatter that defines a
//! shell script template. Each playbook becomes a dynamic MCP tool.
//!
//! ## Format
//!
//! ```markdown
//! ---
//! name: restart-wifi
//! description: Restart WiFi radio interfaces
//! params:
//!   radio:
//!     type: string
//!     description: Which radio
//!     default: all
//! ---
//! # Restart WiFi
//! ```sh
//! wifi down {{radio}}
//! wifi up {{radio}}
//! ```
//! ```
//!
//! This module is pure data — no I/O.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};

/// Parameter definition from playbook frontmatter.
#[derive(Clone, Debug)]
pub struct ParamDef {
    pub param_type: String,
    pub description: String,
    pub default: Option<Value>,
    pub enum_values: Option<Vec<Value>>,
}

/// A parsed playbook ready for execution.
#[derive(Clone, Debug)]
pub struct Playbook {
    pub name: String,
    pub description: String,
    pub params: HashMap<String, ParamDef>,
    pub script: String,
    pub source_device: String,
    pub source_path: String,
}

impl Playbook {
    /// The MCP tool name for this playbook: `pb_{name}`.
    pub fn tool_name(&self) -> String {
        format!("pb_{}", self.name)
    }
}

/// Validate a playbook name: must be non-empty, ASCII alphanumeric / hyphens / underscores.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Playbook name is empty".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Invalid playbook name '{}': only alphanumeric, hyphens, and underscores allowed",
            name
        ));
    }
    Ok(())
}

// --- YAML deserialization helpers ---

#[derive(Deserialize)]
struct FrontMatter {
    name: String,
    description: String,
    #[serde(default)]
    params: HashMap<String, RawParam>,
}

#[derive(Deserialize)]
struct RawParam {
    #[serde(rename = "type", default = "default_type")]
    param_type: String,
    #[serde(default)]
    description: String,
    default: Option<Value>,
    #[serde(rename = "enum")]
    enum_values: Option<Vec<Value>>,
}

fn default_type() -> String {
    "string".to_string()
}

// --- Parsing ---

/// Parse a playbook from its Markdown source.
///
/// Returns `Err` with a human-readable message on malformed input.
pub fn parse_playbook(markdown: &str, device: &str, path: &str) -> Result<Playbook, String> {
    // Split frontmatter: must start with "---"
    let trimmed = markdown.trim_start();
    if !trimmed.starts_with("---") {
        return Err("Missing YAML frontmatter (must start with ---)".into());
    }

    // Find the closing "---"
    let after_open = &trimmed[3..];
    let close_pos = after_open
        .find("\n---")
        .ok_or("Missing closing --- for frontmatter")?;
    let yaml_str = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..]; // skip "\n---"

    // Parse YAML frontmatter
    let fm: FrontMatter =
        serde_yaml::from_str(yaml_str).map_err(|e| format!("YAML parse error: {e}"))?;

    if fm.name.is_empty() {
        return Err("Playbook name is empty".into());
    }
    if fm.description.is_empty() {
        return Err("Playbook description is empty".into());
    }

    // Validate name: only alphanumeric, hyphens, underscores
    if !fm
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Invalid playbook name '{}': only alphanumeric, hyphens, and underscores allowed",
            fm.name
        ));
    }

    // Extract first ```sh or ```bash code block
    let script = extract_script_block(body)?;

    // Convert params
    let params = fm
        .params
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                ParamDef {
                    param_type: v.param_type,
                    description: v.description,
                    default: v.default,
                    enum_values: v.enum_values,
                },
            )
        })
        .collect();

    Ok(Playbook {
        name: fm.name,
        description: fm.description,
        params,
        script,
        source_device: device.to_string(),
        source_path: path.to_string(),
    })
}

/// Find the first fenced ```sh or ```bash code block and return its contents.
fn extract_script_block(body: &str) -> Result<String, String> {
    let lines = body.lines();
    let mut in_block = false;
    let mut script_lines = Vec::new();

    for line in lines {
        if !in_block {
            let trimmed = line.trim();
            if trimmed.starts_with("```sh") || trimmed.starts_with("```bash") {
                in_block = true;
                continue;
            }
        } else if line.trim().starts_with("```") {
            // End of block
            return Ok(script_lines.join("\n"));
        } else {
            script_lines.push(line);
        }
    }

    if in_block {
        return Err("Unclosed code block".into());
    }

    Err("No ```sh or ```bash code block found".into())
}

// --- Tool definition generation ---

/// Convert a playbook into an MCP tool definition JSON value.
pub fn playbook_to_tool_definition(pb: &Playbook) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    // Add device param
    properties.insert(
        "device".to_string(),
        json!({
            "type": "string",
            "description": "Device name. Omit to use the default device."
        }),
    );

    // Add playbook params
    for (name, def) in &pb.params {
        let mut prop = serde_json::Map::new();
        prop.insert("type".to_string(), json!(def.param_type));
        if !def.description.is_empty() {
            prop.insert("description".to_string(), json!(def.description));
        }
        if let Some(ref default) = def.default {
            prop.insert("default".to_string(), default.clone());
        }
        if let Some(ref enum_vals) = def.enum_values {
            prop.insert("enum".to_string(), json!(enum_vals));
        }
        properties.insert(name.clone(), Value::Object(prop));

        // Required if no default
        if def.default.is_none() {
            required.push(json!(name));
        }
    }

    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }

    json!({
        "name": pb.tool_name(),
        "description": format!("[{}] {}", pb.source_device, pb.description),
        "inputSchema": schema
    })
}

// --- Script rendering ---

/// Render a playbook script by substituting `{{param}}` placeholders.
///
/// Uses values from `args`, falling back to param defaults. Returns an error
/// if a required param (no default) is missing from args.
pub fn render_script(pb: &Playbook, args: &Value) -> Result<String, String> {
    let mut script = pb.script.clone();

    for (name, def) in &pb.params {
        let placeholder = format!("{{{{{}}}}}", name);
        if !script.contains(&placeholder) {
            continue;
        }

        let value = args
            .get(name)
            .filter(|v| !v.is_null())
            .or(def.default.as_ref())
            .ok_or_else(|| format!("Missing required parameter: {name}"))?;

        let rendered = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            other => other.to_string(),
        };

        script = script.replace(&placeholder, &rendered);
    }

    // Check for any remaining unreplaced {{...}} placeholders
    if let Some(start) = script.find("{{") {
        if let Some(end) = script[start + 2..].find("}}") {
            let undeclared = &script[start + 2..start + 2 + end];
            return Err(format!(
                "Script references undeclared parameter: {{{{{}}}}}",
                undeclared
            ));
        }
    }

    Ok(script)
}
