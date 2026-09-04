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

/// セッションが所有するタブの指し方。`target_id` は CDP の target id で、タブの寿命の
/// 間ずっと変わらない。`url` は診断用と、target id 未解決時の 1 回きりの解決に使う。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PageRef<'a> {
    pub(crate) target_id: Option<&'a str>,
    pub(crate) url: Option<&'a str>,
}

impl<'a> PageRef<'a> {
    pub(crate) fn new(target_id: Option<&'a str>, url: Option<&'a str>) -> Self {
        Self { target_id, url }
    }
}

/// セッションのタブだけに接続する。**他のタブへフォールバックしない。**
/// 共有 Chrome では別エージェントのタブが並んでいるので、取り違えると相手のフォームを
/// 操作してしまう。対象が見つからないときは黙って別のタブを掴まずエラーにする。
fn connect_page_client(port: u16, page: PageRef<'_>) -> Result<CdpClient, String> {
    let targets = list_page_targets(port)?;
    let target = match page.target_id {
        Some(target_id) => targets
            .iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| {
                format!("session page target {target_id} is gone; create a new session")
            })?,
        None => {
            let url = page
                .url
                .ok_or_else(|| "session has no page target".to_string())?;
            let mut matches = targets.iter().filter(|target| target.url == url);
            let first = matches
                .next()
                .ok_or_else(|| format!("no page target for url {url}"))?;
            if matches.next().is_some() {
                return Err(format!(
                    "url {url} matches multiple page targets; cannot pick the session tab"
                ));
            }
            first
        }
    };
    CdpClient::connect(port, &target.websocket_path)
}

/// session が所有する tab を閉じる。MCP の `close_page` は toolMutex の後ろに並ぶので、
/// ダイアログや重い tool で MCP が詰まっている間は届かない。DevTools の HTTP endpoint を
/// 直接叩くこの経路なら詰まった状態でも閉じられ、閉じた時点で MCP が回復する。
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

/// `/json` の page target id を列挙する。`new_page` の前後で差分を取り、
/// セッションに割り当てられたタブの target id を確定するのに使う。
pub(crate) fn page_target_ids(port: u16) -> Result<Vec<String>, String> {
    Ok(list_page_targets(port)?
        .into_iter()
        .map(|target| target.id)
        .collect())
}

/// url がちょうど 1 つの target に一致するときだけ target id を返す。
/// 0 件・複数件では `None` を返し、呼び出し側は解決を諦める。
pub(crate) fn resolve_unique_target_id(port: u16, url: &str) -> Result<Option<String>, String> {
    let targets = list_page_targets(port)?;
    let mut matches = targets.iter().filter(|target| target.url == url);
    let Some(first) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Ok(None);
    }
    Ok(Some(first.id.clone()))
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
    page: PageRef<'_>,
    url: &str,
    wait_expression: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<(Duration, String), String> {
    let mut client = connect_page_client(port, page)?;
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
                        "goto timed out after {}ms while waiting for {wait_expression}: {error}",
                        timeout.as_millis()
                    ));
                }
            }
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "goto timed out after {}ms while waiting for {wait_expression}",
                timeout.as_millis()
            ));
        }
        thread::sleep(interval);
    }
}

fn allowed_output_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("screenshot_quiet filePath must be absolute".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "screenshot_quiet filePath has no parent".to_string())?;
    let parent = parent
        .canonicalize()
        .map_err(|error| format!("failed to resolve screenshot parent: {error}"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| "screenshot_quiet filePath has no file name".to_string())?;

    let mut roots = vec![std::env::temp_dir()];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home));
    }
    let allowed = roots
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| parent.starts_with(root));
    if !allowed {
        return Err("screenshot_quiet filePath must be under HOME or the OS temp directory".into());
    }
    Ok(parent.join(file_name))
}

/// アップロード対象のパスを検証する。MCP の roots と同じ範囲 ($HOME と OS の temp)
/// に制限し、実在するファイルだけを通す。
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
    page: PageRef<'_>,
    file_path: &Path,
    format: &str,
    quality: Option<u64>,
) -> Result<PathBuf, String> {
    let output = allowed_output_path(file_path)?;
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
        let attempt = connect_page_client(port, page).and_then(|mut client| {
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

pub(crate) fn wait_for_js(
    port: u16,
    page: PageRef<'_>,
    expression: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<Duration, String> {
    // 式は毎回 !!() で bool に潰す。syntax error / throw は poll せず即エラーにする。
    let wrapped = format!("!!({expression})");
    let started = Instant::now();
    // クリック起点のページ遷移中は CDP 接続が応答しない/壊れることがあるため、
    // poll ごとの read timeout を短くし、接続系エラーは deadline 内で再接続して続行する。
    let poll_read_timeout = Duration::from_secs(3);
    let mut client: Option<CdpClient> = None;
    loop {
        if client.is_none() {
            match connect_page_client(port, page) {
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
                // 遷移などによる接続断: 次のループで張り直す。
                client = None;
            }
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "wait_for_js timed out after {}ms: {expression}",
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

pub(crate) fn click_at(port: u16, page: PageRef<'_>, x: f64, y: f64) -> Result<(), String> {
    let mut client = connect_page_client(port, page)?;
    dispatch_click(&mut client, x, y)
}

pub(crate) fn click_selector(
    port: u16,
    page: PageRef<'_>,
    selector: &str,
) -> Result<(f64, f64), String> {
    let mut client = connect_page_client(port, page)?;
    let selector_json = serde_json::json!(selector).to_string();
    let expression = format!(
        "(() => {{ const el = document.querySelector({selector_json}); if (!el) return null; el.scrollIntoView({{block: 'center', inline: 'center'}}); const r = el.getBoundingClientRect(); return {{x: r.x + r.width / 2, y: r.y + r.height / 2}}; }})()"
    );
    let value = evaluate_value(&mut client, &expression)?;
    if value.is_null() {
        return Err(format!("no element matches selector: {selector}"));
    }
    let x = value
        .get("x")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| "selector center has no x".to_string())?;
    let y = value
        .get("y")
        .and_then(|y| y.as_f64())
        .ok_or_else(|| "selector center has no y".to_string())?;
    dispatch_click(&mut client, x, y)?;
    Ok((x, y))
}

fn key_info(key: &str) -> (String, i64, Option<String>) {
    // (code, windowsVirtualKeyCode, text) の対応。text は文字入力を伴うキーのみ。
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
    page: PageRef<'_>,
    key: &str,
    modifiers: u64,
) -> Result<(), String> {
    let mut client = connect_page_client(port, page)?;
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
    page: PageRef<'_>,
    files: &[String],
) -> Result<(), String> {
    if files.is_empty() {
        return Err("upload_file fallback requires at least one file".to_string());
    }
    let mut client = connect_page_client(port, page)?;
    let root_node_id = document_root_node_id(&mut client)?;
    let node_id = query_file_input(&mut client, root_node_id)?;
    let _ = client.call(serde_json::json!({
        "method": "DOM.setFileInputFiles",
        "params": {
            "nodeId": node_id,
            "files": files
        }
    }))?;
    Ok(())
}

fn document_root_node_id(client: &mut CdpClient) -> Result<i64, String> {
    let document = client.call(serde_json::json!({
        "method": "DOM.getDocument",
        "params": {
            "depth": -1,
            "pierce": true
        }
    }))?;
    document
        .get("result")
        .and_then(|result| result.get("root"))
        .and_then(|root| root.get("nodeId"))
        .and_then(|node_id| node_id.as_i64())
        .ok_or_else(|| "DOM.getDocument did not return root nodeId".to_string())
}

/// セレクタで指定した `input[type=file]` にファイルを渡す。file chooser を使わないので
/// 他 session の chooser を横取りする余地がなく、file input が複数あるページでも
/// どれに添付するかを呼び出し側が決められる。
pub(crate) fn set_file_input(
    port: u16,
    page: PageRef<'_>,
    selector: &str,
    files: &[String],
) -> Result<(), String> {
    if files.is_empty() {
        return Err("set_file_input requires at least one file".to_string());
    }
    let mut client = connect_page_client(port, page)?;
    let selector_json = serde_json::json!(selector).to_string();
    let kind = evaluate_value(
        &mut client,
        &format!(
            "(() => {{ const el = document.querySelector({selector_json}); if (!el) return 'missing'; if (el.tagName !== 'INPUT' || el.type !== 'file') return 'not-file'; return 'ok'; }})()"
        ),
    )?;
    match kind.as_str() {
        Some("ok") => {}
        Some("missing") => return Err(format!("no element matches selector: {selector}")),
        _ => return Err(format!("selector is not an input[type=file]: {selector}")),
    }
    let root_node_id = document_root_node_id(&mut client)?;
    let response = client.call(serde_json::json!({
        "method": "DOM.querySelector",
        "params": {
            "nodeId": root_node_id,
            "selector": selector
        }
    }))?;
    let node_id = response
        .get("result")
        .and_then(|result| result.get("nodeId"))
        .and_then(|node_id| node_id.as_i64())
        .filter(|node_id| *node_id != 0)
        .ok_or_else(|| format!("DOM.querySelector found no node for {selector}"))?;
    let _ = client.call(serde_json::json!({
        "method": "DOM.setFileInputFiles",
        "params": {
            "nodeId": node_id,
            "files": files
        }
    }))?;
    Ok(())
}

/// セレクタの要素に実フォーカスを当て、`Input.insertText` で文字列を入れる。
/// native value setter + `input` イベントでは state が更新されないフレームワーク
/// (React 系の一部 ATS) でも、実キー入力と同じ経路になるので値が反映される。
pub(crate) fn type_into(
    port: u16,
    page: PageRef<'_>,
    selector: &str,
    text: &str,
    clear: bool,
) -> Result<(), String> {
    let mut client = connect_page_client(port, page)?;
    let selector_json = serde_json::json!(selector).to_string();
    let focused = evaluate_value(
        &mut client,
        &format!(
            "(() => {{ const el = document.querySelector({selector_json}); if (!el) return false; el.scrollIntoView({{block: 'center', inline: 'center'}}); el.focus(); return document.activeElement === el; }})()"
        ),
    )?;
    if focused.as_bool() != Some(true) {
        return Err(format!("could not focus selector: {selector}"));
    }
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

fn query_file_input(client: &mut CdpClient, root_node_id: i64) -> Result<i64, String> {
    for selector in [
        "form input[type=file]",
        "input[type=file]",
        "input[type=\"file\"]",
    ] {
        let response = client.call(serde_json::json!({
            "method": "DOM.querySelector",
            "params": {
                "nodeId": root_node_id,
                "selector": selector
            }
        }))?;
        if let Some(node_id) = response
            .get("result")
            .and_then(|result| result.get("nodeId"))
            .and_then(|node_id| node_id.as_i64())
            .filter(|node_id| *node_id != 0)
        {
            return Ok(node_id);
        }
    }
    Err("no input[type=file] for upload_file fallback".to_string())
}

fn list_page_targets(port: u16) -> Result<Vec<PageTarget>, String> {
    let response = http_get(port, "/json")?;
    let targets = serde_json::from_str::<Value>(&response)
        .map_err(|error| format!("failed to parse /json response: {error}"))?;
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
    // Chrome は Connection: close を送っても接続を維持することがあり、EOF 依存の
    // read_to_string は read timeout (EAGAIN) で死ぬ。Content-Length を読んで
    // その分だけ body を読む。
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
            // Content-Length なし (chunked 等は DevTools では想定外): EOF まで読む。
            let mut body = String::new();
            reader
                .read_to_string(&mut body)
                .map_err(|error| format!("failed to read DevTools HTTP body: {error}"))?;
            Ok(body)
        }
    }
}

struct CdpClient {
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

    fn call(&mut self, mut request: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        request["id"] = serde_json::json!(id);
        write_ws_text(&mut self.stream, &request.to_string())?;
        loop {
            let response = read_ws_text(&mut self.stream)?;
            let value = serde_json::from_str::<Value>(&response)
                .map_err(|error| format!("failed to parse CDP response: {error}"))?;
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
