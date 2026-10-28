use crate::frame;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::Value;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use std::time::Instant;

struct PageTarget {
    id: String,
    url: String,
    websocket_path: String,
}

/// Connects only to the session's tab. **Never falls back to another tab.** A shared Chrome has
/// other agents' tabs side by side, and picking the wrong one would operate on someone else's form.
/// When the target is missing, return an error instead of silently grabbing another tab.
fn connect_page_client(port: u16, target_id: &str) -> Result<CdpClient, String> {
    let targets = list_page_targets(port)?;
    let target = targets
        .iter()
        .find(|target| target.id == target_id)
        .ok_or_else(|| format!("session page target {target_id} is gone; create a new session"))?;
    CdpClient::connect(port, &target.websocket_path)
}

/// Decides how to dismiss a JavaScript dialog based on its type.
///
/// `beforeunload` is let through because the agent triggered it intending to navigate. `alert` has
/// no option other than closing. `confirm` / `prompt` can change page state, so they are dismissed
/// without accepting.
pub(crate) fn dialog_accepts(dialog_type: &str) -> bool {
    matches!(dialog_type, "beforeunload" | "alert")
}

/// Persistent connection attached to the session's tab that keeps dismissing JavaScript dialogs.
///
/// When an alert / confirm / prompt / beforeunload opens, puppeteer's tool execution never returns,
/// MCP's toolMutex stays held, and every subsequent request waits. Chrome's PageHandler tracks
/// pending dialogs per client, so a client that connects after the dialog opened gets `No dialog is
/// showing` when it sends `Page.handleJavaScriptDialog`. A connection that ran `Page.enable` before
/// the dialog opened has to be kept alive.
pub(crate) struct DialogWatcher {
    target_id: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DialogWatcher {
    /// Connects to the tab and starts the event subscription thread. If connecting or `Page.enable`
    /// fails, the caller should still carry on, so the error is only returned and the daemon keeps
    /// running.
    pub(crate) fn spawn(port: u16, target_id: &str, session_id: &str) -> Result<Self, String> {
        let targets = list_page_targets(port)?;
        let target = targets
            .iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| format!("page target {target_id} is gone"))?;
        let mut client = CdpClient::connect(port, &target.websocket_path)?;
        client.call(serde_json::json!({ "method": "Page.enable" }))?;
        client.set_read_timeout(Duration::from_millis(500))?;

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        let session = session_id.to_string();
        let watched = target_id.to_string();
        thread::Builder::new()
            .name(format!("dialog-watch-{session_id}"))
            .spawn(move || watch_dialogs(client, stop_thread, &session, &watched))
            .map_err(|error| format!("failed to start the dialog watcher: {error}"))?;
        Ok(Self {
            target_id: target_id.to_string(),
            stop,
        })
    }

    pub(crate) fn target_id(&self) -> &str {
        &self.target_id
    }
}

impl Drop for DialogWatcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl std::fmt::Debug for DialogWatcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DialogWatcher")
            .field("target_id", &self.target_id)
            .finish()
    }
}

fn watch_dialogs(
    mut client: CdpClient,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session_id: &str,
    target_id: &str,
) {
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let frame = match read_ws_text_deadline(&mut client.stream) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(error) => {
                eprintln!(
                    "dialog_watch_stopped session={session_id} target={target_id} reason={error}"
                );
                return;
            }
        };
        let Ok(value) = parse_cdp_value(&frame, "CDP event") else {
            continue;
        };
        if value.get("method").and_then(|method| method.as_str())
            != Some("Page.javascriptDialogOpening")
        {
            continue;
        }
        let params = value.get("params");
        let dialog_type = params
            .and_then(|params| params.get("type"))
            .and_then(|dialog_type| dialog_type.as_str())
            .unwrap_or("unknown");
        let accept = dialog_accepts(dialog_type);
        let outcome = client.call(serde_json::json!({
            "method": "Page.handleJavaScriptDialog",
            "params": { "accept": accept }
        }));
        match outcome {
            Ok(_) => eprintln!(
                "dialog_handled session={session_id} target={target_id} type={dialog_type} accept={accept}"
            ),
            Err(error) => eprintln!(
                "dialog_handle_failed session={session_id} target={target_id} type={dialog_type} error={error}"
            ),
        }
    }
}

/// Closes the tab owned by the session. MCP's `close_page` queues behind the toolMutex, so it never
/// arrives while MCP is stuck on a dialog or a heavy tool. Hitting the DevTools HTTP endpoint
/// directly can close the tab even in that state, and MCP recovers once it is closed.
pub(crate) fn close_target(port: u16, target_id: &str) -> Result<(), String> {
    let targets = list_page_targets(port)?;
    if !targets.iter().any(|target| target.id == target_id) {
        return Err(format!("page target {target_id} is already gone"));
    }
    let response = http_get(port, &format!("/json/close/{target_id}"))?;
    if response.contains("Target is closing") || response.trim().is_empty() {
        return Ok(());
    }
    Err(format!(
        "unexpected /json/close response: {}",
        response.trim()
    ))
}

/// Lists the page target ids from `/json`. Used to diff before and after `new_page` to pin down the
/// target id of the tab assigned to the session.
pub(crate) fn page_target_ids(port: u16) -> Result<Vec<String>, String> {
    Ok(list_page_targets(port)?
        .into_iter()
        .map(|target| target.id)
        .collect())
}

pub(crate) fn candidate_target_ids(port: u16, url: Option<&str>) -> Result<Vec<String>, String> {
    let targets = list_page_targets(port)?;
    let matching = targets
        .iter()
        .filter(|target| Some(target.url.as_str()) == url)
        .map(|target| target.id.clone())
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Ok(targets.into_iter().map(|target| target.id).collect());
    }
    Ok(matching)
}

pub(crate) fn take_pin_marker(port: u16, target_id: &str) -> Result<Option<String>, String> {
    let mut client = connect_page_client(port, target_id)?;
    client.set_read_timeout(Duration::from_secs(3))?;
    let value = evaluate_value(
        &mut client,
        "(() => { const v = window.__cdt_pin; delete window.__cdt_pin; return v === undefined ? null : String(v); })()",
    )?;
    Ok(value.as_str().map(str::to_string))
}

/// Reads a CDP response into a `Value`. `serde_json` stops at 128 levels of nesting by default, so
/// responses from pages with deep DOMs such as Gmail fail with `recursion limit exceeded`. The peer
/// is the local Chrome, so read without a depth limit.
pub(crate) fn parse_cdp_value(text: &str, what: &str) -> Result<Value, String> {
    use serde::Deserialize;

    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    Value::deserialize(&mut deserializer)
        .map_err(|error| format!("failed to parse {what}: {error}"))
}

fn evaluate_value(client: &mut CdpClient, expression: &str) -> Result<Value, String> {
    let response = client.call(serde_json::json!({
        "method": "Runtime.evaluate",
        "params": {
            "expression": expression,
            "returnByValue": true
        }
    }))?;
    let result = response
        .get("result")
        .ok_or_else(|| "Runtime.evaluate returned no result".to_string())?;
    if let Some(details) = result.get("exceptionDetails") {
        let text = details
            .get("exception")
            .and_then(|exception| exception.get("description"))
            .and_then(|description| description.as_str())
            .or_else(|| details.get("text").and_then(|text| text.as_str()))
            .unwrap_or("unknown evaluation error");
        return Err(format!("expression threw: {text}"));
    }
    Ok(result
        .get("result")
        .and_then(|inner| inner.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// Gets the CDP objectId of the element returned by a JS expression. This avoids pulling in the
/// whole DOM tree via `DOM.getDocument`, so responses stay small even on pages with many nodes.
fn evaluate_object_id(client: &mut CdpClient, expression: &str) -> Result<Option<String>, String> {
    let response = client.call(serde_json::json!({
        "method": "Runtime.evaluate",
        "params": {
            "expression": expression,
            "returnByValue": false
        }
    }))?;
    let result = response
        .get("result")
        .ok_or_else(|| "Runtime.evaluate returned no result".to_string())?;
    if let Some(details) = result.get("exceptionDetails") {
        let text = details
            .get("exception")
            .and_then(|exception| exception.get("description"))
            .and_then(|description| description.as_str())
            .or_else(|| details.get("text").and_then(|text| text.as_str()))
            .unwrap_or("unknown evaluation error");
        return Err(format!("expression threw: {text}"));
    }
    Ok(result
        .get("result")
        .and_then(|inner| inner.get("objectId"))
        .and_then(|object_id| object_id.as_str())
        .map(|object_id| object_id.to_string()))
}

/// Releases a remote object once it is no longer needed. Failure does not change whether the caller
/// succeeds.
fn release_object(client: &mut CdpClient, object_id: &str) {
    let _ = client.call(serde_json::json!({
        "method": "Runtime.releaseObject",
        "params": { "objectId": object_id }
    }));
}

fn wait_diagnostics(client: &mut CdpClient, expression: &str) -> String {
    let page = evaluate_value(
        client,
        "({ url: location.href, title: document.title, readyState: document.readyState })",
    )
    .ok()
    .filter(|value| value.is_object());
    let Some(page) = page else {
        return String::new();
    };
    let field = |name: &str| {
        page.get(name)
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string()
    };
    let condition = evaluate_value(
        client,
        &format!(
            "(() => {{ try {{ const v = ({expression}); return v === undefined ? 'undefined' : JSON.stringify(v); }} catch (e) {{ return 'threw: ' + e; }} }})()"
        ),
    )
    .ok()
    .and_then(|value| value.as_str().map(str::to_string))
    .unwrap_or_else(|| "unavailable".to_string());
    format!(
        " (current url: {}, title: {:?}, readyState: {}; condition evaluated to: {condition})",
        field("url"),
        field("title"),
        field("readyState")
    )
}

fn is_transient_navigation_error(error: &str) -> bool {
    [
        "Cannot find context with specified id",
        "Execution context was destroyed",
        "Inspected target navigated or closed",
    ]
    .iter()
    .any(|message| error.contains(message))
}

pub(crate) fn goto(
    port: u16,
    target_id: &str,
    url: &str,
    wait_expression: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<(Duration, String), String> {
    let mut client = connect_page_client(port, target_id)?;
    client.set_read_timeout(Duration::from_secs(3))?;
    let response = client.call(serde_json::json!({
        "method": "Page.navigate",
        "params": {"url": url}
    }))?;
    if let Some(error_text) = response
        .get("result")
        .and_then(|result| result.get("errorText"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
    {
        return Err(format!("Page.navigate failed: {error_text}"));
    }

    let wrapped = format!("!!({wait_expression})");
    let started = Instant::now();
    loop {
        match evaluate_value(&mut client, &wrapped) {
            Ok(value) if value.as_bool() == Some(true) => {
                match evaluate_value(&mut client, "location.href") {
                    Ok(value) => {
                        let final_url = value.as_str().unwrap_or(url).to_string();
                        return Ok((started.elapsed(), final_url));
                    }
                    Err(error) if is_transient_navigation_error(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(_) => {}
            Err(error) if is_transient_navigation_error(&error) => {}
            Err(error) if error.starts_with("expression threw:") => return Err(error),
            Err(error) => {
                if started.elapsed() >= timeout {
                    return Err(format!(
                        "goto timed out after {}ms while waiting for {wait_expression}: {error}{}",
                        timeout.as_millis(),
                        wait_diagnostics(&mut client, wait_expression)
                    ));
                }
            }
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "goto timed out after {}ms while waiting for {wait_expression}{}",
                timeout.as_millis(),
                wait_diagnostics(&mut client, wait_expression)
            ));
        }
        thread::sleep(interval);
    }
}

fn allowed_output_path(path: &Path, tool: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{tool} filePath must be absolute"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{tool} filePath has no parent"))?;
    let parent = parent
        .canonicalize()
        .map_err(|error| format!("failed to resolve {tool} output parent: {error}"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("{tool} filePath has no file name"))?;

    let mut roots = vec![std::env::temp_dir()];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home));
    }
    let allowed = roots
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| parent.starts_with(root));
    if !allowed {
        return Err(format!(
            "{tool} filePath must be under HOME or the OS temp directory"
        ));
    }
    Ok(parent.join(file_name))
}

/// Validates a path to upload. Restricts it to the same scope as the MCP roots ($HOME and the OS
/// temp directory) and only lets existing files through.
pub(crate) fn allowed_input_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("file path must be absolute: {}", path.display()));
    }
    let resolved = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?;
    if !resolved.is_file() {
        return Err(format!("not a file: {}", resolved.display()));
    }
    let mut roots = vec![std::env::temp_dir()];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home));
    }
    let allowed = roots
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| resolved.starts_with(root));
    if !allowed {
        return Err(format!(
            "file path must be under HOME or the OS temp directory: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

pub(crate) fn screenshot_quiet(
    port: u16,
    target_id: &str,
    file_path: &Path,
    format: &str,
    quality: Option<u64>,
) -> Result<PathBuf, String> {
    let output = allowed_output_path(file_path, "screenshot_quiet")?;
    let mut params = serde_json::json!({
        "format": format,
        "fromSurface": true,
        "captureBeyondViewport": false
    });
    if matches!(format, "jpeg" | "webp") {
        if let Some(quality) = quality {
            params["quality"] = serde_json::json!(quality.clamp(0, 100));
        }
    }
    // A newly-created background target can briefly accept CDP connections but
    // delay its first paint. Retry transport failures without activating the tab.
    let started = Instant::now();
    let response = loop {
        let attempt = connect_page_client(port, target_id).and_then(|mut client| {
            client.set_read_timeout(Duration::from_secs(5))?;
            client.call(serde_json::json!({
                "method": "Page.captureScreenshot",
                "params": params.clone()
            }))
        });
        match attempt {
            Ok(response) => break response,
            Err(error)
                if started.elapsed() < Duration::from_secs(30)
                    && (error.contains("WebSocket") || error.contains("DevTools HTTP")) =>
            {
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => return Err(error),
        }
    };
    let encoded = response
        .get("result")
        .and_then(|result| result.get("data"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| "Page.captureScreenshot returned no data".to_string())?;
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid screenshot base64: {error}"))?;
    fs::write(&output, bytes)
        .map_err(|error| format!("failed to write {}: {error}", output.display()))?;
    Ok(output)
}

/// Saves the page as a PDF. Used to keep records (invoices, receipts, emails) as they are. Does not
/// bring the tab to the front and bypasses the MCP queue.
pub(crate) fn print_pdf_quiet(
    port: u16,
    target_id: &str,
    file_path: &Path,
    landscape: bool,
    print_background: bool,
    scale: Option<f64>,
) -> Result<PathBuf, String> {
    let output = allowed_output_path(file_path, "print_pdf_quiet")?;
    let mut params = serde_json::json!({
        "landscape": landscape,
        "printBackground": print_background,
        "transferMode": "ReturnAsBase64"
    });
    if let Some(scale) = scale {
        params["scale"] = serde_json::json!(scale.clamp(0.1, 2.0));
    }
    let mut client = connect_page_client(port, target_id)?;
    client.set_read_timeout(Duration::from_secs(60))?;
    let response = client.call(serde_json::json!({
        "method": "Page.printToPDF",
        "params": params
    }))?;
    let encoded = response
        .get("result")
        .and_then(|result| result.get("data"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| "Page.printToPDF returned no data".to_string())?;
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid PDF base64: {error}"))?;
    fs::write(&output, bytes)
        .map_err(|error| format!("failed to write {}: {error}", output.display()))?;
    Ok(output)
}

pub(crate) fn wait_for_js(
    port: u16,
    target_id: &str,
    expression: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<Duration, String> {
    // Always coerce the expression to a bool with !!(). A syntax error or throw fails immediately
    // without polling.
    let wrapped = format!("!!({expression})");
    let started = Instant::now();
    // The CDP connection may stop responding or break while a click triggers navigation, so keep
    // the per-poll read timeout short and reconnect on connection errors as long as the deadline
    // allows.
    let poll_read_timeout = Duration::from_secs(3);
    let mut client: Option<CdpClient> = None;
    loop {
        if client.is_none() {
            match connect_page_client(port, target_id) {
                Ok(new_client) => {
                    let _ = new_client.set_read_timeout(poll_read_timeout);
                    client = Some(new_client);
                }
                Err(error) => {
                    if started.elapsed() >= timeout {
                        return Err(format!(
                            "wait_for_js timed out after {}ms (page not reachable: {error}): {expression}",
                            timeout.as_millis()
                        ));
                    }
                    thread::sleep(interval);
                    continue;
                }
            }
        }
        match evaluate_value(client.as_mut().unwrap(), &wrapped) {
            Ok(value) => {
                if value.as_bool() == Some(true) {
                    return Ok(started.elapsed());
                }
            }
            Err(error) if error.starts_with("expression threw:") => return Err(error),
            Err(_) => {
                // Connection dropped, e.g. by navigation: reconnect on the next iteration.
                client = None;
            }
        }
        if started.elapsed() >= timeout {
            let diagnostics = client
                .as_mut()
                .map(|client| wait_diagnostics(client, expression))
                .unwrap_or_default();
            return Err(format!(
                "wait_for_js timed out after {}ms: {expression}{diagnostics}",
                timeout.as_millis()
            ));
        }
        thread::sleep(interval);
    }
}

fn dispatch_click(client: &mut CdpClient, x: f64, y: f64) -> Result<(), String> {
    for event_type in ["mousePressed", "mouseReleased"] {
        client.call(serde_json::json!({
            "method": "Input.dispatchMouseEvent",
            "params": {
                "type": event_type,
                "x": x,
                "y": y,
                "button": "left",
                "clickCount": 1
            }
        }))?;
    }
    Ok(())
}

pub(crate) fn click_at(port: u16, target_id: &str, x: f64, y: f64) -> Result<(), String> {
    let mut client = connect_page_client(port, target_id)?;
    dispatch_click(&mut client, x, y)
}

pub(crate) fn click_selector(
    port: u16,
    target_id: &str,
    selector: &str,
) -> Result<(f64, f64), String> {
    let mut client = connect_page_client(port, target_id)?;
    let located = frame::locate(&mut client, selector)?;
    let point = frame::click_point(&mut client, &located);
    located.release(&mut client);
    let (x, y) = point?;
    dispatch_click(&mut client, x, y)?;
    Ok((x, y))
}

fn key_info(key: &str) -> (String, i64, Option<String>) {
    // Maps to (code, windowsVirtualKeyCode, text). text is set only for keys that produce character
    // input.
    match key {
        "Enter" => ("Enter".to_string(), 13, Some("\r".to_string())),
        "Escape" => ("Escape".to_string(), 27, None),
        "Tab" => ("Tab".to_string(), 9, None),
        "Backspace" => ("Backspace".to_string(), 8, None),
        "Delete" => ("Delete".to_string(), 46, None),
        "ArrowUp" => ("ArrowUp".to_string(), 38, None),
        "ArrowDown" => ("ArrowDown".to_string(), 40, None),
        "ArrowLeft" => ("ArrowLeft".to_string(), 37, None),
        "ArrowRight" => ("ArrowRight".to_string(), 39, None),
        "Home" => ("Home".to_string(), 36, None),
        "End" => ("End".to_string(), 35, None),
        "PageUp" => ("PageUp".to_string(), 33, None),
        "PageDown" => ("PageDown".to_string(), 34, None),
        " " | "Space" => ("Space".to_string(), 32, Some(" ".to_string())),
        other if other.chars().count() == 1 => {
            let ch = other.chars().next().unwrap();
            let code = if ch.is_ascii_alphabetic() {
                format!("Key{}", ch.to_ascii_uppercase())
            } else if ch.is_ascii_digit() {
                format!("Digit{ch}")
            } else {
                String::new()
            };
            (
                code,
                ch.to_ascii_uppercase() as i64,
                Some(other.to_string()),
            )
        }
        other => (other.to_string(), 0, None),
    }
}

pub(crate) fn dispatch_key(
    port: u16,
    target_id: &str,
    key: &str,
    modifiers: u64,
) -> Result<(), String> {
    let mut client = connect_page_client(port, target_id)?;
    let (code, virtual_key_code, text) = key_info(key);
    let mut down = serde_json::json!({
        "method": "Input.dispatchKeyEvent",
        "params": {
            "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": key,
            "code": code,
            "windowsVirtualKeyCode": virtual_key_code,
            "nativeVirtualKeyCode": virtual_key_code,
            "modifiers": modifiers
        }
    });
    if let Some(text) = &text {
        down["params"]["text"] = serde_json::json!(text);
        down["params"]["unmodifiedText"] = serde_json::json!(text);
    }
    client.call(down)?;
    client.call(serde_json::json!({
        "method": "Input.dispatchKeyEvent",
        "params": {
            "type": "keyUp",
            "key": key,
            "code": code,
            "windowsVirtualKeyCode": virtual_key_code,
            "nativeVirtualKeyCode": virtual_key_code,
            "modifiers": modifiers
        }
    }))?;
    Ok(())
}

pub(crate) fn set_file_input_files(
    port: u16,
    target_id: &str,
    files: &[String],
) -> Result<(), String> {
    if files.is_empty() {
        return Err("upload_file fallback requires at least one file".to_string());
    }
    let mut client = connect_page_client(port, target_id)?;
    let object_id = first_file_input_object_id(&mut client)?;
    let outcome = client.call(serde_json::json!({
        "method": "DOM.setFileInputFiles",
        "params": {
            "objectId": object_id,
            "files": files
        }
    }));
    release_object(&mut client, &object_id);
    outcome?;
    Ok(())
}

/// Hands files to the `input[type=file]` matched by the selector. Not using the file chooser leaves
/// no way to steal another session's chooser, and on pages with several file inputs the caller
/// decides which one gets the attachment. `iframe >>> input` reaches inside iframes.
pub(crate) fn set_file_input(
    port: u16,
    target_id: &str,
    selector: &str,
    files: &[String],
) -> Result<(), String> {
    if files.is_empty() {
        return Err("set_file_input requires at least one file".to_string());
    }
    let mut client = connect_page_client(port, target_id)?;
    let located = frame::locate(&mut client, selector)?;
    let outcome = if frame::is_file_input(&mut client, &located)? {
        frame::set_files(&mut client, &located, files)
    } else {
        Err(frame::not_file_input(&mut client, &located, selector))
    };
    located.release(&mut client);
    outcome
}

/// Gives the selector's element real focus and inserts the string with `Input.insertText`. This
/// follows the same path as real key input, so the value sticks even in frameworks (some
/// React-based ATSs) whose state is not updated by a native value setter plus an `input` event.
pub(crate) fn type_into(
    port: u16,
    target_id: &str,
    selector: &str,
    text: &str,
    clear: bool,
) -> Result<(), String> {
    let mut client = connect_page_client(port, target_id)?;
    let located = frame::locate(&mut client, selector)?;
    let focused = frame::focus(&mut client, &located);
    if !matches!(focused, Ok(true)) {
        let message = match focused {
            Ok(_) => {
                frame::failure_context(&mut client, &located, selector, "could not focus selector")
            }
            Err(error) => error,
        };
        located.release(&mut client);
        return Err(message);
    }
    located.release(&mut client);
    if clear {
        client.call(serde_json::json!({
            "method": "Input.dispatchKeyEvent",
            "params": {
                "type": "keyDown",
                "key": "a",
                "code": "KeyA",
                "windowsVirtualKeyCode": 65,
                "nativeVirtualKeyCode": 65,
                "modifiers": 2
            }
        }))?;
        client.call(serde_json::json!({
            "method": "Input.dispatchKeyEvent",
            "params": {
                "type": "keyUp",
                "key": "a",
                "code": "KeyA",
                "windowsVirtualKeyCode": 65,
                "nativeVirtualKeyCode": 65,
                "modifiers": 2
            }
        }))?;
        for event_type in ["keyDown", "keyUp"] {
            client.call(serde_json::json!({
                "method": "Input.dispatchKeyEvent",
                "params": {
                    "type": event_type,
                    "key": "Delete",
                    "code": "Delete",
                    "windowsVirtualKeyCode": 46,
                    "nativeVirtualKeyCode": 46
                }
            }))?;
        }
    }
    client.call(serde_json::json!({
        "method": "Input.insertText",
        "params": {"text": text}
    }))?;
    Ok(())
}

/// Finds the file input used by the `upload_file` fallback. Inputs inside a form are checked first
/// so that sites with a hidden input at the top of the page do not miss the real attachment field.
fn first_file_input_object_id(client: &mut CdpClient) -> Result<String, String> {
    for selector in ["form input[type=file]", "input[type=file]"] {
        let selector_json = serde_json::json!(selector).to_string();
        if let Some(object_id) =
            evaluate_object_id(client, &format!("document.querySelector({selector_json})"))?
        {
            return Ok(object_id);
        }
    }
    Err("no input[type=file] for upload_file fallback".to_string())
}

fn list_page_targets(port: u16) -> Result<Vec<PageTarget>, String> {
    let response = http_get(port, "/json")?;
    let targets = parse_cdp_value(&response, "/json response")?;
    let Some(items) = targets.as_array() else {
        return Err("/json response is not an array".to_string());
    };
    Ok(items
        .iter()
        .filter(|item| item.get("type").and_then(|value| value.as_str()) == Some("page"))
        .filter_map(|item| {
            let id = item.get("id").and_then(|value| value.as_str())?;
            let url = item.get("url").and_then(|value| value.as_str())?;
            let websocket = item
                .get("webSocketDebuggerUrl")
                .and_then(|value| value.as_str())?;
            let websocket_path = websocket
                .split_once("://")
                .and_then(|(_, rest)| rest.split_once('/'))
                .map(|(_, path)| format!("/{path}"))?;
            Some(PageTarget {
                id: id.to_string(),
                url: url.to_string(),
                websocket_path,
            })
        })
        .collect())
}

fn http_get(port: u16, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|error| format!("failed to connect DevTools HTTP: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("failed to configure DevTools HTTP timeout: {error}"))?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .map_err(|error| format!("failed to request DevTools HTTP: {error}"))?;
    // Chrome may keep the connection open even after sending Connection: close, so an EOF-based
    // read_to_string dies on the read timeout (EAGAIN). Read Content-Length and read exactly that
    // much body.
    let mut reader = BufReader::new(&mut stream);
    let mut status = String::new();
    reader
        .read_line(&mut status)
        .map_err(|error| format!("failed to read DevTools HTTP status: {error}"))?;
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        return Err(format!("DevTools HTTP returned {}", status.trim_end()));
    }
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read DevTools HTTP headers: {error}"))?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(str::trim)
            .and_then(|value| value.parse::<usize>().ok())
        {
            content_length = Some(value);
        }
    }
    match content_length {
        Some(length) => {
            let mut body = vec![0_u8; length];
            reader
                .read_exact(&mut body)
                .map_err(|error| format!("failed to read DevTools HTTP body: {error}"))?;
            String::from_utf8(body)
                .map_err(|error| format!("DevTools HTTP body is not UTF-8: {error}"))
        }
        None => {
            // No Content-Length (chunked etc. is not expected from DevTools): read until EOF.
            let mut body = String::new();
            reader
                .read_to_string(&mut body)
                .map_err(|error| format!("failed to read DevTools HTTP body: {error}"))?;
            Ok(body)
        }
    }
}

pub(crate) struct CdpClient {
    stream: TcpStream,
    next_id: i64,
}

impl CdpClient {
    fn set_read_timeout(&self, timeout: Duration) -> Result<(), String> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("failed to configure DevTools WebSocket timeout: {error}"))
    }

    fn connect(port: u16, path: &str) -> Result<Self, String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .map_err(|error| format!("failed to connect DevTools WebSocket: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| format!("failed to configure DevTools WebSocket timeout: {error}"))?;
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
                    ["dGhlIH", "NhbXBs", "ZSBub25jZQ=="].concat()
                )
                .as_bytes(),
            )
            .map_err(|error| format!("failed to write DevTools WebSocket handshake: {error}"))?;
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|error| format!("failed to clone DevTools WebSocket: {error}"))?,
        );
        let mut status = String::new();
        reader
            .read_line(&mut status)
            .map_err(|error| format!("failed to read DevTools WebSocket handshake: {error}"))?;
        if !status.starts_with("HTTP/1.1 101") && !status.starts_with("HTTP/1.0 101") {
            return Err(format!(
                "DevTools WebSocket handshake returned {}",
                status.trim_end()
            ));
        }
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|error| format!("failed to read DevTools WebSocket headers: {error}"))?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        Ok(Self { stream, next_id: 1 })
    }

    pub(crate) fn call_in(
        &mut self,
        session_id: Option<&str>,
        mut request: Value,
    ) -> Result<Value, String> {
        if let Some(session_id) = session_id {
            request["sessionId"] = serde_json::json!(session_id);
        }
        self.call(request)
    }

    fn call(&mut self, mut request: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        request["id"] = serde_json::json!(id);
        write_ws_text(&mut self.stream, &request.to_string())?;
        loop {
            let response = read_ws_text(&mut self.stream)?;
            let value = parse_cdp_value(&response, "CDP response")?;
            if value.get("id").and_then(|value| value.as_i64()) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(format!("CDP call failed: {error}"));
                }
                return Ok(value);
            }
        }
    }
}

fn write_ws_text(stream: &mut TcpStream, text: &str) -> Result<(), String> {
    let payload = text.as_bytes();
    let mut frame = Vec::new();
    frame.push(0x81);
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    let mask = [0x12, 0x34, 0x56, 0x78];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream
        .write_all(&frame)
        .map_err(|error| format!("failed to write CDP WebSocket frame: {error}"))
}

fn read_ws_text(stream: &mut TcpStream) -> Result<String, String> {
    let mut head = [0_u8; 2];
    stream
        .read_exact(&mut head)
        .map_err(|error| format!("failed to read CDP WebSocket frame: {error}"))?;
    read_ws_text_body(stream, head)
}

/// Returns `Ok(None)` when the deadline passes while waiting to read. This is the entry point that
/// lets the event subscription loop go back and check its stop flag; it never cuts a frame in half.
fn read_ws_text_deadline(stream: &mut TcpStream) -> Result<Option<String>, String> {
    let mut head = [0_u8; 2];
    match stream.read_exact(&mut head) {
        Ok(()) => read_ws_text_body(stream, head).map(Some),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(format!("failed to read CDP WebSocket frame: {error}")),
    }
}

fn read_ws_text_body(stream: &mut TcpStream, head: [u8; 2]) -> Result<String, String> {
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut len = (head[1] & 0x7f) as u64;
    if len == 126 {
        let mut extended = [0_u8; 2];
        stream
            .read_exact(&mut extended)
            .map_err(|error| format!("failed to read CDP WebSocket frame length: {error}"))?;
        len = u16::from_be_bytes(extended) as u64;
    } else if len == 127 {
        let mut extended = [0_u8; 8];
        stream
            .read_exact(&mut extended)
            .map_err(|error| format!("failed to read CDP WebSocket frame length: {error}"))?;
        len = u64::from_be_bytes(extended);
    }
    let mut mask = [0_u8; 4];
    if masked {
        stream
            .read_exact(&mut mask)
            .map_err(|error| format!("failed to read CDP WebSocket frame mask: {error}"))?;
    }
    let mut payload = vec![0_u8; len as usize];
    stream
        .read_exact(&mut payload)
        .map_err(|error| format!("failed to read CDP WebSocket frame payload: {error}"))?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    match opcode {
        0x1 => String::from_utf8(payload)
            .map_err(|error| format!("CDP WebSocket text is not UTF-8: {error}")),
        0x8 => Err("CDP WebSocket closed".to_string()),
        other => Err(format!("unexpected CDP WebSocket opcode {other}")),
    }
}
