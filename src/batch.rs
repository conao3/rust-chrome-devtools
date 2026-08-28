use crate::config::*;
use crate::daemon::*;
use crate::router::*;
use std::fs;
use std::io::BufReader;
use std::io::Read;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

pub(crate) struct BatchOptions {
    pub(crate) profile_name: String,
    pub(crate) session_id: String,
    pub(crate) script_path: String,
    pub(crate) output_path: Option<String>,
    pub(crate) fail_fast: bool,
}

pub(crate) const STEP_SHAPE_HINT: &str =
    r#"{"type":"tool","name":"<mcp-tool>","args":{...}} or {"type":"sleep_ms","ms":<u64>}"#;

pub(crate) enum BatchOutcome {
    Completed,
    StoppedOnError(String),
}

pub(crate) fn run_batch(config: &Config, args: &[String]) -> Result<(), String> {
    let options = parse_batch_args(args)?;
    match execute_batch(config, &options) {
        Ok(BatchOutcome::Completed) => Ok(()),
        Ok(BatchOutcome::StoppedOnError(label)) => {
            Err(format!("scenario stopped on error at step: {label}"))
        }
        Err(error) => {
            let results = serde_json::json!([{ "type": "error", "error": error }]);
            let output =
                serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".to_string());
            let _ = write_batch_output(&options.output_path, &output);
            Err(error)
        }
    }
}

pub(crate) fn write_batch_output(output_path: &Option<String>, output: &str) -> Result<(), String> {
    if let Some(path) = output_path {
        fs::write(path, output).map_err(|error| format!("failed to write {path}: {error}"))
    } else {
        println!("{output}");
        Ok(())
    }
}

pub(crate) fn execute_batch(
    config: &Config,
    options: &BatchOptions,
) -> Result<BatchOutcome, String> {
    let profile = find_profile(config, &options.profile_name)?;
    let script_content = read_script_source(&options.script_path)?;
    let steps_value: serde_json::Value = serde_json::from_str(&script_content)
        .map_err(|error| format!("failed to parse {}: {error}", options.script_path))?;
    let steps = steps_value
        .as_array()
        .ok_or_else(|| "script must be a JSON array of steps".to_string())?;

    ensure_daemon(&profile)?;
    let socket_path = daemon_socket_path(&profile)?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("failed to connect {}: {error}", socket_path.display()))?;
    let mut read_stream = stream
        .try_clone()
        .map_err(|error| format!("failed to clone daemon stream: {error}"))?;
    let mut reader = BufReader::new(&mut read_stream);

    bind_session(&mut stream, &mut reader, &options.session_id, &profile.name)?;

    write_json_line(
        &mut stream,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"roots":{"listChanged":false}},"clientInfo":{"name":"chrome-devtools-batch","version":"0.1.0"}}}"#,
    )?;
    write_json_line(
        &mut stream,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    )?;
    read_response(&mut reader, &mut stream, 1)?;

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut next_id: u64 = 2;
    let mut stopped_on_error: Option<String> = None;

    for step in steps {
        let step_type = step
            .get("type")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("step missing 'type': expected {STEP_SHAPE_HINT}"))?;
        let label = step.get("label").cloned();
        let on_error = step
            .get("on_error")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .unwrap_or_else(|| {
                if options.fail_fast {
                    "stop".to_string()
                } else {
                    "continue".to_string()
                }
            });
        match step_type {
            "sleep_ms" => {
                let ms = step
                    .get("ms")
                    .and_then(|value| value.as_u64())
                    .ok_or_else(|| "sleep_ms requires 'ms' (non-negative integer)".to_string())?;
                thread::sleep(Duration::from_millis(ms));
                results.push(serde_json::json!({
                    "type": "sleep_ms",
                    "label": label,
                    "ms": ms,
                }));
            }
            "tool" => {
                let name = step
                    .get("name")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        "tool step requires 'name' (the MCP tool name, e.g. take_snapshot)"
                            .to_string()
                    })?;
                let raw_arguments = step
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let arguments = resolve_refs(raw_arguments, &results)?;
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
                let response_value: serde_json::Value =
                    serde_json::from_str(&response_line).unwrap_or(serde_json::Value::Null);
                let result_value = response_value.get("result").cloned();
                let error_value = response_value.get("error").cloned();
                let is_error_result = matches!(
                    result_value.as_ref().and_then(|value| value.get("isError")),
                    Some(serde_json::Value::Bool(true))
                );
                let has_error =
                    error_value.as_ref().is_some_and(|value| !value.is_null()) || is_error_result;
                results.push(serde_json::json!({
                    "type": "tool",
                    "name": name,
                    "label": label,
                    "result": result_value,
                    "error": error_value,
                }));
                if has_error && on_error == "stop" {
                    stopped_on_error = Some(
                        label
                            .as_ref()
                            .and_then(|value| value.as_str())
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| name.to_string()),
                    );
                    break;
                }
            }
            other => {
                return Err(format!(
                    "unknown step type: {other}: expected {STEP_SHAPE_HINT}"
                ));
            }
        }
    }

    let _ = stream.shutdown(Shutdown::Write);

    let output = serde_json::to_string_pretty(&serde_json::Value::Array(results))
        .map_err(|error| format!("failed to serialize results: {error}"))?;
    write_batch_output(&options.output_path, &output)?;

    match stopped_on_error {
        Some(label) => Ok(BatchOutcome::StoppedOnError(label)),
        None => Ok(BatchOutcome::Completed),
    }
}

pub(crate) fn read_script_source(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut content = String::new();
        std::io::stdin()
            .read_to_string(&mut content)
            .map_err(|error| format!("failed to read script from stdin: {error}"))?;
        Ok(content)
    } else {
        fs::read_to_string(path).map_err(|error| format!("failed to read {path}: {error}"))
    }
}

pub(crate) fn parse_batch_args(args: &[String]) -> Result<BatchOptions, String> {
    let mut profile_name = None;
    let mut session_id = None;
    let mut script_path = None;
    let mut output_path = None;
    let mut fail_fast = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--profile" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--profile requires a value".to_string());
                };
                profile_name = Some(value.clone());
                index += 2;
            }
            "--session" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--session requires a value".to_string());
                };
                session_id = Some(value.clone());
                index += 2;
            }
            "--script" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--script requires a value".to_string());
                };
                script_path = Some(value.clone());
                index += 2;
            }
            "--output" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--output requires a value".to_string());
                };
                output_path = Some(value.clone());
                index += 2;
            }
            "--fail-fast" => {
                fail_fast = true;
                index += 1;
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }
    let profile_name = profile_name.ok_or_else(|| "--profile is required".to_string())?;
    let session_id = session_id.ok_or_else(|| "--session is required".to_string())?;
    let script_path = script_path.ok_or_else(|| "--script is required".to_string())?;
    Ok(BatchOptions {
        profile_name,
        session_id,
        script_path,
        output_path,
        fail_fast,
    })
}

/// `path` segments are dot-separated; numeric segments index into arrays.
pub(crate) fn resolve_refs(
    value: serde_json::Value,
    results: &[serde_json::Value],
) -> Result<serde_json::Value, String> {
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
        .find(|entry| entry.get("label").and_then(|value| value.as_str()) == Some(label))
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
