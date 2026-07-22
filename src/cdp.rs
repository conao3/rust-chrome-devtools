use serde_json::Value;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::thread;
use std::time::Duration;
use std::time::Instant;

struct PageTarget {
    url: String,
    websocket_path: String,
}

fn connect_page_client(port: u16, page_url: Option<&str>) -> Result<CdpClient, String> {
    let targets = list_page_targets(port)?;
    let target = page_url
        .and_then(|url| targets.iter().find(|target| target.url == url))
        .or_else(|| targets.first())
        .ok_or_else(|| "no page target available".to_string())?;
    CdpClient::connect(port, &target.websocket_path)
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

pub(crate) fn wait_for_js(
    port: u16,
    page_url: Option<&str>,
    expression: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<Duration, String> {
    let mut client = connect_page_client(port, page_url)?;
    // 式は毎回 !!() で bool に潰す。syntax error / throw は poll せず即エラーにする。
    let wrapped = format!("!!({expression})");
    let started = Instant::now();
    loop {
        let value = evaluate_value(&mut client, &wrapped)?;
        if value.as_bool() == Some(true) {
            return Ok(started.elapsed());
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

pub(crate) fn click_at(port: u16, page_url: Option<&str>, x: f64, y: f64) -> Result<(), String> {
    let mut client = connect_page_client(port, page_url)?;
    dispatch_click(&mut client, x, y)
}

pub(crate) fn click_selector(
    port: u16,
    page_url: Option<&str>,
    selector: &str,
) -> Result<(f64, f64), String> {
    let mut client = connect_page_client(port, page_url)?;
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
    page_url: Option<&str>,
    key: &str,
    modifiers: u64,
) -> Result<(), String> {
    let mut client = connect_page_client(port, page_url)?;
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
    page_url: Option<&str>,
    files: &[String],
) -> Result<(), String> {
    if files.is_empty() {
        return Err("upload_file fallback requires at least one file".to_string());
    }
    let targets = list_page_targets(port)?;
    let target = page_url
        .and_then(|url| targets.iter().find(|target| target.url == url))
        .or_else(|| targets.first())
        .ok_or_else(|| "no page target for upload_file fallback".to_string())?;
    let mut client = CdpClient::connect(port, &target.websocket_path)?;
    let document = client.call(serde_json::json!({
        "method": "DOM.getDocument",
        "params": {
            "depth": -1,
            "pierce": true
        }
    }))?;
    let root_node_id = document
        .get("result")
        .and_then(|result| result.get("root"))
        .and_then(|root| root.get("nodeId"))
        .and_then(|node_id| node_id.as_i64())
        .ok_or_else(|| "DOM.getDocument did not return root nodeId".to_string())?;
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
            let url = item.get("url").and_then(|value| value.as_str())?;
            let websocket = item
                .get("webSocketDebuggerUrl")
                .and_then(|value| value.as_str())?;
            let websocket_path = websocket
                .split_once("://")
                .and_then(|(_, rest)| rest.split_once('/'))
                .map(|(_, path)| format!("/{path}"))?;
            Some(PageTarget {
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
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("failed to read DevTools HTTP response: {error}"))?;
    let Some((head, body)) = response.split_once("\r\n\r\n") else {
        return Err("invalid DevTools HTTP response".to_string());
    };
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return Err(format!(
            "DevTools HTTP returned {}",
            head.lines().next().unwrap_or("unknown status")
        ));
    }
    Ok(body.to_string())
}

struct CdpClient {
    stream: TcpStream,
    next_id: i64,
}

impl CdpClient {
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
