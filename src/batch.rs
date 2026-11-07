use crate::config::*;
use crate::daemon::*;
use crate::error::{Result, parameter, target};
use crate::router::*;
use serde_json::Value;
use std::io::BufReader;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

pub(crate) struct BatchOptions {
    pub(crate) profile: Profile,
    pub(crate) session_id: String,
    pub(crate) steps: Vec<Value>,
    pub(crate) fail_fast: bool,
}

pub(crate) struct BatchRun {
    pub(crate) results: Vec<Value>,
    pub(crate) stopped_on_error: Option<String>,
}

pub(crate) const STEP_SHAPE_HINT: &str = r#"one of the step types "tool" or "sleep_ms": {"Type":"tool","Name":"<mcp-tool>","Args":{...}} or {"Type":"sleep_ms","Ms":<u64>} (shorthand: Type=tool,Name=<mcp-tool>,Args={...} or Type=sleep_ms,Ms=<u64>)"#;

const SLEEP_LIKE_TOOL_NAMES: [&str; 3] = ["sleep_ms", "sleep", "wait"];

/// Each value is a JSON array of steps, one JSON step object, or one step in shorthand syntax.
pub(crate) fn parse_steps(values: &[String]) -> Result<Vec<Value>> {
    let mut steps = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            let parsed: Value = serde_json::from_str(trimmed)
                .map_err(|error| parameter(format!("--steps is not valid JSON: {error}")))?;
            match parsed {
                Value::Array(items) => steps.extend(items),
                object => steps.push(object),
            }
        } else {
            steps.push(parse_shorthand_step(trimmed)?);
        }
    }
    if steps.is_empty() {
        return Err(parameter("--steps needs at least one step"));
    }
    validate_steps(&steps)?;
    Ok(steps)
}

fn split_top_level(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for ch in input.chars() {
        if in_string {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                current.push(ch);
            }
            '{' | '[' => {
                depth += 1;
                current.push(ch);
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    parts.push(current);
    parts
}

pub(crate) fn parse_shorthand_step(input: &str) -> Result<Value> {
    let mut step = serde_json::Map::new();
    for part in split_top_level(input) {
        let Some((key, raw)) = part.split_once('=') else {
            return Err(parameter(format!(
                "--steps shorthand expects Key=Value pairs separated by commas, got: {part}"
            )));
        };
        let raw = raw.trim();
        let value = if raw.starts_with('{') || raw.starts_with('[') {
            serde_json::from_str(raw)
                .map_err(|error| parameter(format!("--steps: {key} is not valid JSON: {error}")))?
        } else if key == "Ms" {
            Value::from(raw.parse::<u64>().map_err(|_| {
                parameter(format!(
                    "--steps: Ms must be a non-negative integer, got {raw}"
                ))
            })?)
        } else {
            Value::String(raw.to_string())
        };
        step.insert(key.trim().to_string(), value);
    }
    Ok(Value::Object(step))
}

pub(crate) fn execute_batch(options: &BatchOptions) -> Result<BatchRun> {
    let profile = &options.profile;
    ensure_daemon(profile)?;
    let socket_path = daemon_socket_path(profile)?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("failed to connect {}: {error}", socket_path.display()))?;
    let mut read_stream = stream
        .try_clone()
        .map_err(|error| format!("failed to clone daemon stream: {error}"))?;
    let mut reader = BufReader::new(&mut read_stream);

    bind_session(&mut stream, &mut reader, &options.session_id, &profile.name).map_err(target)?;

    write_json_line(
        &mut stream,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"roots":{"listChanged":false}},"clientInfo":{"name":"chrome-devtools-batch","version":"0.1.0"}}}"#,
    )?;
    write_json_line(
        &mut stream,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    )?;
    read_response(&mut reader, &mut stream, 1)?;

    let mut results: Vec<Value> = Vec::new();
    let mut next_id: u64 = 2;
    let mut stopped_on_error: Option<String> = None;

    for step in &options.steps {
        let step_type = step.get("Type").and_then(Value::as_str).unwrap_or_default();
        let label = step.get("Label").cloned().unwrap_or(Value::Null);
        let stop_on_error = match step.get("OnError").and_then(Value::as_str) {
            Some(value) => value == "stop",
            None => options.fail_fast,
        };
        match step_type {
            "sleep_ms" => {
                let ms = step.get("Ms").and_then(Value::as_u64).unwrap_or_default();
                thread::sleep(Duration::from_millis(ms));
                results.push(serde_json::json!({
                    "Type": "sleep_ms",
                    "Label": label,
                    "Ms": ms,
                }));
            }
            _ => {
                let name = step.get("Name").and_then(Value::as_str).unwrap_or_default();
                let raw_arguments = step
                    .get("Args")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let arguments = resolve_refs(raw_arguments, &results).map_err(parameter)?;
                let id = next_id;
                next_id += 1;
                let request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/call",
                    "params": {
                        "name": name,
                        "arguments": arguments,
                    }
                });
                write_json_line(&mut stream, &request.to_string())?;
                let response_line = read_response(&mut reader, &mut stream, id)?;
                let response_value: Value =
                    serde_json::from_str(&response_line).unwrap_or(Value::Null);
                let result_value = response_value.get("result").cloned();
                let error_value = response_value.get("error").cloned();
                let is_error_result = matches!(
                    result_value.as_ref().and_then(|value| value.get("isError")),
                    Some(Value::Bool(true))
                );
                let navigated = (is_error_result || error_value.is_some())
                    && is_navigation_context_loss(result_value.as_ref(), error_value.as_ref());
                let has_error = !navigated
                    && (error_value.as_ref().is_some_and(|value| !value.is_null())
                        || is_error_result);
                let mut entry = serde_json::json!({
                    "Type": "tool",
                    "Name": name,
                    "Label": label,
                    "Result": result_value,
                    "Error": error_value,
                });
                if navigated {
                    entry["Navigated"] = serde_json::json!(true);
                    entry["Error"] = Value::Null;
                    if let Some(result) = entry["Result"].as_object_mut() {
                        result.insert("isError".to_string(), serde_json::json!(false));
                    }
                }
                results.push(entry);
                if has_error && stop_on_error {
                    stopped_on_error = Some(
                        label
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| name.to_string()),
                    );
                    break;
                }
            }
        }
    }

    let _ = stream.shutdown(Shutdown::Write);

    Ok(BatchRun {
        results,
        stopped_on_error,
    })
}

const STEP_KEYS: [&str; 6] = ["Type", "Name", "Args", "Label", "OnError", "Ms"];

/// The step key a mistyped key stands for: the same name in another case, or the MCP spelling
/// `arguments`.
fn renamed_step_key(key: &str) -> Option<&'static str> {
    if key.eq_ignore_ascii_case("arguments") {
        return Some("Args");
    }
    STEP_KEYS
        .into_iter()
        .find(|name| name.eq_ignore_ascii_case(key))
}

pub(crate) fn validate_steps(steps: &[Value]) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let position = index + 1;
        let Some(object) = step.as_object() else {
            return Err(parameter(format!(
                "step {position} is not an object: expected {STEP_SHAPE_HINT}"
            )));
        };
        let unknown_keys = object
            .keys()
            .filter(|key| !STEP_KEYS.contains(&key.as_str()))
            .collect::<Vec<_>>();
        if let Some(unknown) = unknown_keys.first() {
            let renames = unknown_keys
                .iter()
                .filter_map(|key| renamed_step_key(key).map(|name| format!("{key} -> {name}")))
                .collect::<Vec<_>>();
            let renames = if renames.is_empty() {
                String::new()
            } else {
                format!("; rename the keys: {}", renames.join(", "))
            };
            return Err(parameter(format!(
                "step {position} has unknown key '{unknown}' (keys: {}): expected {STEP_SHAPE_HINT}{renames}",
                STEP_KEYS.join(", ")
            )));
        }
        let Some(step_type) = step.get("Type").and_then(Value::as_str) else {
            let keys = object.keys().cloned().collect::<Vec<_>>().join(", ");
            return Err(parameter(format!(
                "step {position} missing 'Type' (found: {keys}): expected {STEP_SHAPE_HINT}"
            )));
        };
        if let Some(on_error) = step.get("OnError") {
            if !matches!(on_error.as_str(), Some("continue" | "stop")) {
                return Err(parameter(format!(
                    "step {position}: OnError must be continue or stop"
                )));
            }
        }
        match step_type {
            "sleep_ms" => {
                if step.get("Ms").and_then(Value::as_u64).is_none() {
                    return Err(parameter(format!(
                        "step {position}: sleep_ms requires 'Ms' (non-negative integer)"
                    )));
                }
            }
            "tool" => {
                let Some(name) = step.get("Name").and_then(Value::as_str) else {
                    return Err(parameter(format!(
                        "step {position}: tool step requires 'Name' (the MCP tool name, e.g. take_snapshot)"
                    )));
                };
                if SLEEP_LIKE_TOOL_NAMES.contains(&name) {
                    return Err(parameter(format!(
                        "step {position}: '{name}' is a step type, not a tool. Use {{\"Type\":\"sleep_ms\",\"Ms\":<u64>}} to pause, or the daemon-native wait_for_js / goto to wait for a page condition. Step types: tool, sleep_ms"
                    )));
                }
            }
            other => {
                return Err(parameter(format!(
                    "step {position}: unknown step type: {other}: expected {STEP_SHAPE_HINT}"
                )));
            }
        }
    }
    Ok(())
}

fn is_navigation_context_loss(
    result: Option<&serde_json::Value>,
    error: Option<&serde_json::Value>,
) -> bool {
    const MESSAGE: &str = "Execution context was destroyed";
    let in_result = result
        .and_then(|result| result.get("content"))
        .and_then(|content| content.as_array())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("text")
                    .and_then(|text| text.as_str())
                    .is_some_and(|text| text.contains(MESSAGE))
            })
        });
    let in_error = error
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
        .is_some_and(|message| message.contains(MESSAGE));
    in_result || in_error
}

/// `path` segments are dot-separated; numeric segments index into arrays.
pub(crate) fn resolve_refs(
    value: serde_json::Value,
    results: &[serde_json::Value],
) -> std::result::Result<serde_json::Value, String> {
    match value {
        serde_json::Value::Object(map) if map.len() == 1 && map.contains_key("$ref") => {
            let path = map
                .get("$ref")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "$ref must be a string".to_string())?;
            Ok(resolve_path(path, results))
        }
        serde_json::Value::Object(map) => {
            let mut resolved = serde_json::Map::with_capacity(map.len());
            for (key, sub) in map {
                resolved.insert(key, resolve_refs(sub, results)?);
            }
            Ok(serde_json::Value::Object(resolved))
        }
        serde_json::Value::Array(items) => {
            let mut resolved = Vec::with_capacity(items.len());
            for item in items {
                resolved.push(resolve_refs(item, results)?);
            }
            Ok(serde_json::Value::Array(resolved))
        }
        other => Ok(other),
    }
}

pub(crate) fn resolve_path(path: &str, results: &[serde_json::Value]) -> serde_json::Value {
    let mut parts = path.split('.');
    let Some(label) = parts.next() else {
        return serde_json::Value::Null;
    };
    let Some(entry) = results
        .iter()
        .find(|entry| entry.get("Label").and_then(|value| value.as_str()) == Some(label))
    else {
        return serde_json::Value::Null;
    };
    let mut current = entry.clone();
    for part in parts {
        current = if let Ok(index) = part.parse::<usize>() {
            current
                .get(index)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        } else {
            current
                .get(part)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };
    }
    current
}
