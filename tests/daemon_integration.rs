use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct TestEnv {
    home: PathBuf,
    daemon: Option<Child>,
    session_ttl_secs: Option<u64>,
    reaper_interval_ms: Option<u64>,
    mcp_request_timeout_secs: Option<u64>,
    mcp_probe_timeout_secs: Option<u64>,
    fake_mcp_hang_tool: Option<String>,
    fake_mcp_hang_secs: Option<u64>,
    fake_mcp_persist_pages: bool,
    devtools: Vec<FakeDevTools>,
}

struct FakeDevTools {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    state: Arc<Mutex<FakeState>>,
}

struct FakeTarget {
    id: String,
    url: String,
}

struct FakeState {
    targets: Vec<FakeTarget>,
    markers: HashMap<String, String>,
}

impl FakeDevTools {
    fn navigate(&self, id: &str, url: &str) {
        let mut state = self.state.lock().unwrap();
        let target = state
            .targets
            .iter_mut()
            .find(|target| target.id == id)
            .unwrap();
        target.url = url.to_string();
    }
}

static NEXT_TEST_ENV_ID: AtomicU64 = AtomicU64::new(1);

impl TestEnv {
    fn new(tag: &str) -> Self {
        Self::new_with_options(tag, None, None, None, None, None, None)
    }

    fn new_with_reaper(
        tag: &str,
        session_ttl_secs: Option<u64>,
        reaper_interval_ms: Option<u64>,
    ) -> Self {
        Self::new_with_options(
            tag,
            session_ttl_secs,
            reaper_interval_ms,
            None,
            None,
            None,
            None,
        )
    }

    fn new_with_mcp_timeout(tag: &str, timeout_secs: u64, hang_tool: &str) -> Self {
        Self::new_with_options(
            tag,
            None,
            None,
            Some(timeout_secs),
            Some(1),
            Some(hang_tool.to_string()),
            None,
        )
    }

    fn new_with_transient_mcp_timeout(
        tag: &str,
        timeout_secs: u64,
        probe_timeout_secs: u64,
        hang_tool: &str,
        hang_secs: u64,
    ) -> Self {
        Self::new_with_options(
            tag,
            None,
            None,
            Some(timeout_secs),
            Some(probe_timeout_secs),
            Some(hang_tool.to_string()),
            Some(hang_secs),
        )
    }

    fn new_with_persistent_mcp(tag: &str) -> Self {
        let mut env = Self::new(tag);
        env.fake_mcp_persist_pages = true;
        env
    }

    fn new_with_options(
        tag: &str,
        session_ttl_secs: Option<u64>,
        reaper_interval_ms: Option<u64>,
        mcp_request_timeout_secs: Option<u64>,
        mcp_probe_timeout_secs: Option<u64>,
        fake_mcp_hang_tool: Option<String>,
        fake_mcp_hang_secs: Option<u64>,
    ) -> Self {
        let home = std::env::temp_dir().join(format!(
            "cdt-{tag}-{}-{}",
            std::process::id(),
            NEXT_TEST_ENV_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".config/chrome-devtools")).unwrap();
        fs::create_dir_all(home.join(".cache/chrome-devtools/daemons")).unwrap();
        fs::write(
            home.join(".config/chrome-devtools/config.toml"),
            "[[profiles]]\nname = \"default\"\n",
        )
        .unwrap();

        let devtools = spawn_fake_devtools();
        fs::write(
            home.join(".cache/chrome-devtools/daemons/default.port"),
            devtools.port.to_string(),
        )
        .unwrap();

        Self {
            home,
            daemon: None,
            session_ttl_secs,
            reaper_interval_ms,
            mcp_request_timeout_secs,
            mcp_probe_timeout_secs,
            fake_mcp_hang_tool,
            fake_mcp_hang_secs,
            fake_mcp_persist_pages: false,
            devtools: vec![devtools],
        }
    }

    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chrome-devtools"));
        command
            .env("HOME", &self.home)
            .env("CHROME_DEVTOOLS_MCP_COMMAND", fake_mcp_path())
            .env("CHROME", "/nonexistent-chrome-for-tests");
        if let Some(value) = self.session_ttl_secs {
            command.env("CHROME_DEVTOOLS_SESSION_IDLE_TTL_SECS", value.to_string());
        }
        if let Some(value) = self.reaper_interval_ms {
            command.env(
                "CHROME_DEVTOOLS_SESSION_REAPER_INTERVAL_MS",
                value.to_string(),
            );
        }
        if let Some(value) = self.mcp_request_timeout_secs {
            command.env(
                "CHROME_DEVTOOLS_MCP_REQUEST_TIMEOUT_SECS",
                value.to_string(),
            );
            // heavy tool (take_snapshot 等) の timeout も同値に下げ、timeout 再現テストを成立させる
            command.env(
                "CHROME_DEVTOOLS_MCP_HEAVY_REQUEST_TIMEOUT_SECS",
                value.to_string(),
            );
        }
        if let Some(value) = self.mcp_probe_timeout_secs {
            command.env("CHROME_DEVTOOLS_MCP_PROBE_TIMEOUT_SECS", value.to_string());
        }
        if let Some(value) = self.fake_mcp_hang_tool.as_ref() {
            command.env("FAKE_MCP_HANG_TOOL", value);
        }
        if let Some(value) = self.fake_mcp_hang_secs {
            command.env("FAKE_MCP_HANG_SECS", value.to_string());
        }
        if self.fake_mcp_persist_pages {
            command.env("FAKE_MCP_PERSIST_PAGES", "1");
        }
        command
    }

    fn use_devtools_port(&self, port: u16) {
        fs::write(
            self.home
                .join(".cache/chrome-devtools/daemons/default.port"),
            port.to_string(),
        )
        .unwrap();
    }

    fn replace_devtools(&mut self) -> u16 {
        self.devtools.clear();
        let devtools = spawn_fake_devtools();
        let port = devtools.port;
        fs::write(
            self.home
                .join(".cache/chrome-devtools/daemons/default.port"),
            port.to_string(),
        )
        .unwrap();
        self.devtools.push(devtools);
        port
    }

    fn start_daemon(&mut self) {
        let stderr_path = self.home.join("daemon.stderr");
        let stderr = fs::File::create(&stderr_path).unwrap();
        let child = self
            .cli()
            .args(["daemon", "run", "--profile", "default"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let socket = self.socket_path();
        let started = Instant::now();
        while !socket.exists() {
            if started.elapsed() >= Duration::from_secs(10) {
                let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
                let status = self
                    .daemon
                    .as_mut()
                    .and_then(|child| child.try_wait().ok().flatten())
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "running".to_string());
                panic!(
                    "daemon socket did not appear within 10s: socket={} status={status} stderr={stderr}",
                    socket.display()
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn socket_path(&self) -> PathBuf {
        self.home
            .join(".cache/chrome-devtools/daemons/default.sock")
    }

    fn connect(&self) -> UnixStream {
        UnixStream::connect(self.socket_path()).unwrap()
    }

    fn run_cli(&self, args: &[&str]) -> (bool, String) {
        let output = self.cli().args(args).output().unwrap();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.success(), combined)
    }

    fn wait_status_field_at_least(&self, prefix: &str, expected: u128) -> String {
        let started = Instant::now();
        loop {
            let (ok, output) = self.run_cli(&["daemon", "status", "--profile", "default"]);
            assert!(ok, "status failed: {output}");
            if status_field(&output, prefix)
                .and_then(|value| value.parse::<u128>().ok())
                .map(|value| value >= expected)
                == Some(true)
            {
                return output;
            }
            if started.elapsed() >= Duration::from_secs(5) {
                panic!("{prefix} did not reach {expected}: {output}");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_session_contains(&self, session: &str, expected: &str) -> String {
        let started = Instant::now();
        loop {
            let (ok, output) = self.run_cli(&["session", "list", "--profile", "default"]);
            assert!(ok, "session list failed: {output}");
            if output.contains(session) && output.contains(expected) {
                return output;
            }
            if started.elapsed() >= Duration::from_secs(5) {
                panic!("session {session} did not contain {expected}: {output}");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn create_session(&self) -> String {
        let (ok, output) = self.run_cli(&["session", "create", "--profile", "default"]);
        assert!(ok, "session create failed: {output}");
        output
            .split_whitespace()
            .find_map(|part| part.strip_prefix("session="))
            .expect("session id in output")
            .to_string()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        if let Some(child) = self.daemon.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.home);
    }
}

impl Drop for FakeDevTools {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn fake_mcp_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-mcp.sh")
}

fn spawn_fake_devtools() -> FakeDevTools {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let state = Arc::new(Mutex::new(FakeState {
        targets: vec![FakeTarget {
            id: "FAKETARGET1".to_string(),
            url: "about:blank".to_string(),
        }],
        markers: HashMap::new(),
    }));
    let state_for_thread = Arc::clone(&state);
    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            let mut buffer = [0u8; 1024];
            let bytes = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let request_line = request.lines().next().unwrap_or("").to_string();
            let path = request_line.split_whitespace().nth(1).unwrap_or("");
            if request.starts_with("GET /json ") {
                let items = state_for_thread
                    .lock()
                    .unwrap()
                    .targets
                    .iter()
                    .map(|target| {
                        format!(
                            r#"{{"id":"{id}","type":"page","url":"{url}","webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/page/{id}"}}"#,
                            id = target.id,
                            url = target.url
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let body = format!("[{items}]");
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            } else if request.contains("Upgrade: websocket") {
                let target_id = path
                    .strip_prefix("/devtools/page/")
                    .unwrap_or("")
                    .to_string();
                let _ = stream.write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: fake\r\n\r\n",
                );
                // ダイアログ watcher が tab ごとに常設接続を張るので、1 本の接続で
                // accept ループを塞ぐと後続の /json が読めなくなる。
                let state = Arc::clone(&state_for_thread);
                thread::spawn(move || handle_fake_cdp(stream, state, target_id));
            } else if let Some(query) = path.strip_prefix("/__fake/add?") {
                let (id, url) = fake_query_pair(query, "id", "url");
                state_for_thread
                    .lock()
                    .unwrap()
                    .targets
                    .push(FakeTarget { id, url });
                respond_fake_ok(&mut stream);
            } else if let Some(query) = path.strip_prefix("/__fake/marker?") {
                let (id, value) = fake_query_pair(query, "id", "value");
                state_for_thread.lock().unwrap().markers.insert(id, value);
                respond_fake_ok(&mut stream);
            } else {
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"Browser\":\"fake\"}",
                );
            }
        }
    });
    FakeDevTools {
        port,
        stop,
        handle: Some(handle),
        state,
    }
}

fn fake_query_pair(query: &str, first: &str, second: &str) -> (String, String) {
    let rest = query.strip_prefix(&format!("{first}=")).unwrap();
    let (first_value, rest) = rest.split_once(&format!("&{second}=")).unwrap();
    (first_value.to_string(), rest.to_string())
}

fn respond_fake_ok(stream: &mut TcpStream) {
    let _ =
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
}

fn handle_fake_cdp(mut stream: TcpStream, state: Arc<Mutex<FakeState>>, target_id: String) {
    while let Ok(text) = read_fake_ws_text(&mut stream) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let id = value["id"].clone();
        let response = match value["method"].as_str().unwrap_or("") {
            "DOM.getDocument" => serde_json::json!({
                "id": id,
                "result": {
                    "root": {
                        "nodeId": 1
                    }
                }
            }),
            "DOM.querySelector" => serde_json::json!({
                "id": id,
                "result": {
                    "nodeId": 7
                }
            }),
            "DOM.setFileInputFiles" => serde_json::json!({
                "id": id,
                "result": {}
            }),
            "Runtime.evaluate" if value["params"]["returnByValue"] == serde_json::json!(false) => {
                // 要素を掴む評価。実 Chrome は remote object を返し、呼び出し側は
                // objectId を DOM.setFileInputFiles に渡す。
                serde_json::json!({
                    "id": id,
                    "result": {
                        "result": {
                            "objectId": "fake-object-1"
                        }
                    }
                })
            }
            "Runtime.evaluate" => {
                let expression = value["params"]["expression"].as_str().unwrap_or("");
                let result_value = if expression.contains("getBoundingClientRect") {
                    serde_json::json!({"x": 100.0, "y": 200.0})
                } else if expression.contains("__cdt_pin") {
                    match state.lock().unwrap().markers.remove(&target_id) {
                        Some(marker) => serde_json::json!(marker),
                        None => serde_json::Value::Null,
                    }
                } else if expression.contains("readyState: document.readyState") {
                    let state = state.lock().unwrap();
                    let url = state
                        .targets
                        .iter()
                        .find(|target| target.id == target_id)
                        .map(|target| target.url.clone())
                        .unwrap_or_default();
                    serde_json::json!({"url": url, "title": "Fake Title", "readyState": "loading"})
                } else if expression.contains("JSON.stringify") {
                    serde_json::json!("false")
                } else if expression == "location.href" {
                    let state = state.lock().unwrap();
                    state
                        .targets
                        .iter()
                        .find(|target| target.id == target_id)
                        .map(|target| serde_json::json!(target.url))
                        .unwrap_or(serde_json::Value::Null)
                } else if expression.contains("__ready") {
                    serde_json::json!(true)
                } else {
                    serde_json::json!(false)
                };
                serde_json::json!({
                    "id": id,
                    "result": {
                        "result": {
                            "value": result_value
                        }
                    }
                })
            }
            "Runtime.callFunctionOn" => {
                let declaration = value["params"]["functionDeclaration"]
                    .as_str()
                    .unwrap_or("");
                let result_value = if declaration.contains("getBoundingClientRect") {
                    serde_json::json!({"x": 100.0, "y": 200.0})
                } else {
                    serde_json::json!(true)
                };
                serde_json::json!({
                    "id": id,
                    "result": {
                        "result": {
                            "value": result_value
                        }
                    }
                })
            }
            "Page.navigate" => {
                let url = value["params"]["url"].as_str().unwrap_or("").to_string();
                if let Some(target) = state
                    .lock()
                    .unwrap()
                    .targets
                    .iter_mut()
                    .find(|target| target.id == target_id)
                {
                    target.url = url;
                }
                serde_json::json!({
                    "id": id,
                    "result": {"frameId": "fake-frame"}
                })
            }
            "Page.captureScreenshot" => {
                use base64::Engine;
                serde_json::json!({
                    "id": id,
                    "result": {"data": base64::engine::general_purpose::STANDARD.encode(&target_id)}
                })
            }
            _ => serde_json::json!({
                "id": id,
                "result": {}
            }),
        };
        let _ = write_fake_ws_text(&mut stream, &response.to_string());
    }
}

fn read_fake_ws_text(stream: &mut TcpStream) -> Result<String, String> {
    let mut head = [0_u8; 2];
    stream
        .read_exact(&mut head)
        .map_err(|error| error.to_string())?;
    let opcode = head[0] & 0x0f;
    if opcode == 0x8 {
        return Err("closed".to_string());
    }
    let masked = head[1] & 0x80 != 0;
    let mut len = (head[1] & 0x7f) as u64;
    if len == 126 {
        let mut extended = [0_u8; 2];
        stream
            .read_exact(&mut extended)
            .map_err(|error| error.to_string())?;
        len = u16::from_be_bytes(extended) as u64;
    } else if len == 127 {
        let mut extended = [0_u8; 8];
        stream
            .read_exact(&mut extended)
            .map_err(|error| error.to_string())?;
        len = u64::from_be_bytes(extended);
    }
    let mut mask = [0_u8; 4];
    if masked {
        stream
            .read_exact(&mut mask)
            .map_err(|error| error.to_string())?;
    }
    let mut payload = vec![0_u8; len as usize];
    stream
        .read_exact(&mut payload)
        .map_err(|error| error.to_string())?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    String::from_utf8(payload).map_err(|error| error.to_string())
}

fn write_fake_ws_text(stream: &mut TcpStream, text: &str) -> Result<(), String> {
    let payload = text.as_bytes();
    let mut frame = Vec::new();
    frame.push(0x81);
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame).map_err(|error| error.to_string())
}

fn control_roundtrip(stream: &mut UnixStream, command: &str) -> String {
    stream
        .write_all(format!("__chrome_devtools_daemon__:{command}\n").as_bytes())
        .unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    line.trim_end().to_string()
}

fn bind_stream(stream: &mut UnixStream, session: &str) {
    let bound = control_roundtrip(stream, &format!("bind session={session}"));
    assert_eq!(bound, format!("bound={session}"));
}

fn json_roundtrip(stream: &mut UnixStream, request: serde_json::Value) -> serde_json::Value {
    stream
        .write_all(request.to_string().as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(line.trim_end()).unwrap()
}

fn tool_call(
    stream: &mut UnixStream,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    json_roundtrip(
        stream,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments
            }
        }),
    )
}

fn content_text(response: &serde_json::Value) -> &str {
    response["result"]["content"][0]["text"].as_str().unwrap()
}

fn uid_token(response: &serde_json::Value) -> String {
    uid_tokens(response).remove(0)
}

fn uid_tokens(response: &serde_json::Value) -> Vec<String> {
    let text = content_text(response);
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find("uid=u:") {
        let after = &rest[index + 4..];
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        tokens.push(after[..end].to_string());
        rest = &after[end..];
    }
    assert!(!tokens.is_empty(), "uid token in response: {text}");
    tokens
}

fn selected_page_id(response: &serde_json::Value) -> u64 {
    response["result"]["structuredContent"]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|page| page["selected"] == true)
        .and_then(|page| page["id"].as_u64())
        .unwrap()
}

fn page_ids(response: &serde_json::Value) -> Vec<u64> {
    response["result"]["structuredContent"]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|page| page["id"].as_u64())
        .collect()
}

fn status_field<'a>(output: &'a str, prefix: &str) -> Option<&'a str> {
    output
        .split_whitespace()
        .find_map(|field| field.strip_prefix(prefix))
}

#[test]
fn smoke_session_create_and_batch() {
    let mut env = TestEnv::new("smoke");
    env.start_daemon();
    let session = env.create_session();

    let script = env.home.join("batch.json");
    fs::write(
        &script,
        r#"[{"type":"tool","name":"take_snapshot","args":{}}]"#,
    )
    .unwrap();
    let (ok, output) = env.run_cli(&[
        "mcp",
        "batch",
        "--profile",
        "default",
        "--session",
        &session,
        "--script",
        script.to_str().unwrap(),
    ]);
    assert!(ok, "batch failed: {output}");
    assert!(output.contains("snapshot page="), "batch output: {output}");
}

#[test]
fn batch_failure_emits_json_error_on_stdout() {
    let mut env = TestEnv::new("batcherr");
    env.start_daemon();
    let script = env.home.join("batch.json");
    fs::write(
        &script,
        r#"[{"type":"tool","name":"take_snapshot","args":{}}]"#,
    )
    .unwrap();
    let output = env
        .cli()
        .args([
            "mcp",
            "batch",
            "--profile",
            "default",
            "--session",
            "sess-bogus",
            "--script",
            script.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(parsed[0]["type"], "error");
    assert!(
        parsed[0]["error"]
            .as_str()
            .unwrap()
            .contains("unknown session")
    );
}

fn write_script(env: &TestEnv, body: &str) -> PathBuf {
    let script = env.home.join("script.json");
    fs::write(&script, body).unwrap();
    script
}

fn batch_args<'a>(session: &'a str, script: &'a Path) -> Vec<&'a str> {
    vec![
        "mcp",
        "batch",
        "--profile",
        "default",
        "--session",
        session,
        "--script",
        script.to_str().unwrap(),
    ]
}

#[test]
fn batch_treats_a_navigation_context_loss_as_navigated() {
    let mut env = TestEnv::new("batchnav");
    env.start_daemon();
    let session = env.create_session();
    let script = write_script(
        &env,
        r#"[{"type":"tool","name":"navigating_click","label":"nav"},{"type":"tool","name":"take_snapshot","label":"after"}]"#,
    );
    let mut args = batch_args(&session, &script);
    args.push("--fail-fast");
    let (ok, output) = env.run_cli(&args);
    assert!(ok, "batch failed: {output}");
    let results: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(results[0]["navigated"], serde_json::json!(true));
    assert_eq!(results[0]["error"], serde_json::Value::Null);
    assert_eq!(results[0]["result"]["isError"], serde_json::json!(false));
    assert!(
        results[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("snapshot page="),
        "output: {output}"
    );
    assert!(results[1].get("navigated").is_none());
}

#[test]
fn batch_rejects_a_sleep_tool_step_before_running_anything() {
    let mut env = TestEnv::new("batchsleep");
    env.start_daemon();
    let script = write_script(
        &env,
        r#"[{"type":"tool","name":"take_snapshot"},{"type":"tool","name":"sleep_ms","args":{"ms":1000}}]"#,
    );
    let output = env
        .cli()
        .args(batch_args("sess-bogus", &script))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let error = parsed[0]["error"].as_str().unwrap();
    assert!(
        error.contains("step 2: 'sleep_ms' is a step type, not a tool")
            && error.contains("Step types: tool, sleep_ms"),
        "error: {error}"
    );
}

#[test]
fn batch_reports_the_keys_of_a_step_without_type() {
    let mut env = TestEnv::new("batchkeys");
    env.start_daemon();
    let script = write_script(&env, r#"[{"tool":"take_snapshot","arguments":{}}]"#);
    let (ok, output) = env.run_cli(&batch_args("sess-bogus", &script));
    assert!(!ok);
    assert!(
        output.contains("step 1 missing 'type' (found: arguments, tool)"),
        "output: {output}"
    );
}

#[test]
fn session_in_use_names_the_holder() {
    let mut env = TestEnv::new("inuse");
    env.start_daemon();
    let session = env.create_session();
    let mut holder = env.connect();
    let bound = control_roundtrip(&mut holder, &format!("bind session={session} pid=4242"));
    assert_eq!(bound, format!("bound={session}"));

    let script = write_script(&env, r#"[{"type":"tool","name":"take_snapshot"}]"#);
    let (ok, output) = env.run_cli(&batch_args(&session, &script));
    assert!(!ok);
    assert!(
        output.contains(&format!("session in use: {session} (held by pid 4242 for ")),
        "output: {output}"
    );
    let (_, listed) = env.run_cli(&["session", "list", "--profile", "default"]);
    assert!(listed.contains("owner_pid=4242"), "list: {listed}");
}

#[test]
fn goto_and_wait_failures_report_the_page_state() {
    let mut env = TestEnv::new("waitdiag");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);

    let goto = tool_error_text(&tool_call(
        &mut stream,
        500,
        "goto",
        serde_json::json!({"url": "https://slow.test/", "waitExpression": "window.__never", "timeout": 300, "interval": 50}),
    ));
    assert!(
        goto.contains("goto timed out after 300ms while waiting for window.__never")
            && goto.contains("current url: https://slow.test/")
            && goto.contains("title: \"Fake Title\"")
            && goto.contains("condition evaluated to: false"),
        "message: {goto}"
    );

    let wait = tool_error_text(&tool_call(
        &mut stream,
        501,
        "wait_for_js",
        serde_json::json!({"expression": "window.__never", "timeout": 300, "interval": 50}),
    ));
    assert!(
        wait.contains("wait_for_js timed out after 300ms")
            && wait.contains("current url: https://slow.test/"),
        "message: {wait}"
    );
}

#[test]
fn direct_list_goes_through_the_running_daemon() {
    let mut env = TestEnv::new("directlist");
    env.start_daemon();
    let (ok, output) = env.run_cli(&["mcp", "direct-list", "--profile", "default"]);
    assert!(ok, "direct-list failed: {output}");
    assert!(
        output.contains("holds the profile lock") && output.contains("\"tools\""),
        "output: {output}"
    );
}

#[test]
fn client_disconnect_does_not_kill_daemon() {
    let mut env = TestEnv::new("disconnect");
    env.start_daemon();
    let session = env.create_session();

    {
        let mut stream = env.connect();
        let bound = control_roundtrip(&mut stream, &format!("bind session={session}"));
        assert_eq!(bound, format!("bound={session}"));
        stream
            .write_all(
                br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"click","arguments":{}}}"#,
            )
            .unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
    }

    thread::sleep(Duration::from_secs(1));
    let mut stream = env.connect();
    let status = control_roundtrip(&mut stream, "status");
    assert!(
        status.contains("daemon=ready"),
        "daemon died after client disconnect: {status}"
    );

    let (ok, output) = env.run_cli(&["session", "list", "--profile", "default"]);
    assert!(ok, "session list failed: {output}");
    assert!(
        output.contains(&session),
        "session lost after another client's disconnect: {output}"
    );
}

#[test]
fn client_kill_during_bound_tool_keeps_session() {
    let mut env = TestEnv::new_with_mcp_timeout("killcall", 1, "take_snapshot");
    env.start_daemon();
    let session = env.create_session();

    {
        let mut stream = env.connect();
        let bound = control_roundtrip(&mut stream, &format!("bind session={session}"));
        assert_eq!(bound, format!("bound={session}"));
        stream
            .write_all(
                br#"{"jsonrpc":"2.0","id":171,"method":"tools/call","params":{"name":"take_snapshot","arguments":{}}}"#,
            )
            .unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
    }

    let output = env.wait_session_contains(&session, "owned=false");
    assert!(output.contains(&session), "output: {output}");
    thread::sleep(Duration::from_secs(2));
    let (ok, output) = env.run_cli(&["daemon", "status", "--profile", "default"]);
    assert!(ok, "status failed: {output}");
    assert!(!output.contains("respawns="), "output: {output}");
    let output = env.wait_session_contains(&session, "owned=false");
    assert!(output.contains(&session), "output: {output}");
}

#[test]
fn string_jsonrpc_id_roundtrips_through_daemon() {
    let mut env = TestEnv::new("stringid");
    env.start_daemon();
    let session = env.create_session();

    let mut stream = env.connect();
    let bound = control_roundtrip(&mut stream, &format!("bind session={session}"));
    assert_eq!(bound, format!("bound={session}"));
    stream
        .write_all(br#"{"jsonrpc":"2.0","id":"client-a","method":"tools/list","params":{}}"#)
        .unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(value["id"], "client-a");
    let click = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "click")
        .unwrap();
    assert!(click["inputSchema"]["properties"].get("pageId").is_none());
}

#[test]
fn unbound_tool_call_is_rejected() {
    let mut env = TestEnv::new("unbound");
    env.start_daemon();

    let mut stream = env.connect();
    stream
        .write_all(
            br#"{"jsonrpc":"2.0","id":"client-a","method":"tools/call","params":{"name":"click","arguments":{}}}"#,
        )
        .unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(value["id"], "client-a");
    assert_eq!(
        value["error"]["message"],
        "session bind required for MCP forwarding"
    );
}

#[test]
fn control_commands_respond_while_client_bound() {
    let mut env = TestEnv::new("busy");
    env.start_daemon();
    let session = env.create_session();

    let mut holder = env.connect();
    let bound = control_roundtrip(&mut holder, &format!("bind session={session}"));
    assert_eq!(bound, format!("bound={session}"));

    let mut stream = env.connect();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"__chrome_devtools_daemon__:session_create\n")
        .unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("session_create should answer within 5s while another client is bound");
    assert!(line.starts_with("session="), "unexpected response: {line}");
}

#[test]
fn daemon_starts_mcp_with_page_id_routing_and_hides_page_id_schema() {
    let mut env = TestEnv::new("pageidflag");
    env.start_daemon();
    let args = fs::read_to_string(env.home.join("fake-mcp-args")).unwrap();
    assert!(args.contains("--pageIdRouting"), "args: {args}");

    let (ok, output) = env.run_cli(&["mcp", "list", "--profile", "default"]);
    assert!(ok, "mcp list failed: {output}");
    let value: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
    let tools = value["result"]["tools"].as_array().unwrap();
    let click = tools.iter().find(|tool| tool["name"] == "click").unwrap();
    let select_page = tools
        .iter()
        .find(|tool| tool["name"] == "select_page")
        .unwrap();
    assert!(
        click["inputSchema"]["properties"].get("pageId").is_none(),
        "output: {output}"
    );
    assert!(
        click["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry.as_str() != Some("pageId")),
        "output: {output}"
    );
    assert!(
        select_page["inputSchema"]["properties"]
            .get("pageId")
            .is_some(),
        "output: {output}"
    );
}

#[test]
fn multiple_sessions_can_bind_at_the_same_time() {
    let mut env = TestEnv::new("multibind");
    env.start_daemon();
    let first = env.create_session();
    let second = env.create_session();

    let mut first_stream = env.connect();
    bind_stream(&mut first_stream, &first);
    let mut second_stream = env.connect();
    bind_stream(&mut second_stream, &second);

    let first_response = tool_call(
        &mut first_stream,
        10,
        "evaluate_script",
        serde_json::json!({
            "function": "() => location.href"
        }),
    );
    let second_response = tool_call(
        &mut second_stream,
        11,
        "evaluate_script",
        serde_json::json!({
            "function": "() => location.href"
        }),
    );
    assert!(content_text(&first_response).contains("evaluated page="));
    assert!(content_text(&second_response).contains("evaluated page="));
}

#[test]
fn session_tabs_keep_snapshot_state_independent() {
    let mut env = TestEnv::new("tabstate");
    env.start_daemon();
    let first = env.create_session();
    let second = env.create_session();
    let mut first_stream = env.connect();
    let mut second_stream = env.connect();
    bind_stream(&mut first_stream, &first);
    bind_stream(&mut second_stream, &second);

    let first_page = selected_page_id(&tool_call(
        &mut first_stream,
        20,
        "new_page",
        serde_json::json!({"url": "https://a.test/"}),
    ));
    let second_page = selected_page_id(&tool_call(
        &mut second_stream,
        21,
        "new_page",
        serde_json::json!({"url": "https://b.test/"}),
    ));
    assert_ne!(first_page, second_page);

    let first_snapshot = tool_call(
        &mut first_stream,
        22,
        "take_snapshot",
        serde_json::json!({}),
    );
    let second_snapshot = tool_call(
        &mut second_stream,
        23,
        "take_snapshot",
        serde_json::json!({}),
    );
    assert!(content_text(&first_snapshot).contains(&format!("page={first_page}")));
    assert!(content_text(&second_snapshot).contains(&format!("page={second_page}")));
    let first_token = uid_token(&first_snapshot);
    let second_token = uid_token(&second_snapshot);

    let first_click = tool_call(
        &mut first_stream,
        24,
        "click",
        serde_json::json!({"uid": first_token}),
    );
    let second_click = tool_call(
        &mut second_stream,
        25,
        "click",
        serde_json::json!({"uid": second_token}),
    );
    assert_eq!(
        content_text(&first_click),
        format!("clicked page={first_page}")
    );
    assert_eq!(
        content_text(&second_click),
        format!("clicked page={second_page}")
    );
}

#[test]
fn list_pages_marks_the_calling_session_page() {
    let mut env = TestEnv::new("listpages");
    env.start_daemon();
    let first = env.create_session();
    let second = env.create_session();
    let mut first_stream = env.connect();
    let mut second_stream = env.connect();
    bind_stream(&mut first_stream, &first);
    bind_stream(&mut second_stream, &second);

    let first_page = selected_page_id(&tool_call(
        &mut first_stream,
        30,
        "new_page",
        serde_json::json!({"url": "https://a.test/"}),
    ));
    let second_page = selected_page_id(&tool_call(
        &mut second_stream,
        31,
        "new_page",
        serde_json::json!({"url": "https://b.test/"}),
    ));

    let first_pages = tool_call(&mut first_stream, 32, "list_pages", serde_json::json!({}));
    let second_pages = tool_call(&mut second_stream, 33, "list_pages", serde_json::json!({}));
    assert_eq!(selected_page_id(&first_pages), first_page);
    assert_eq!(selected_page_id(&second_pages), second_page);
}

#[test]
fn select_page_changes_only_the_calling_session_target() {
    let mut env = TestEnv::new("selectpage");
    env.start_daemon();
    let first = env.create_session();
    let second = env.create_session();
    let mut first_stream = env.connect();
    let mut second_stream = env.connect();
    bind_stream(&mut first_stream, &first);
    bind_stream(&mut second_stream, &second);

    tool_call(
        &mut first_stream,
        40,
        "new_page",
        serde_json::json!({"url": "https://a.test/"}),
    );
    let second_page = selected_page_id(&tool_call(
        &mut second_stream,
        41,
        "new_page",
        serde_json::json!({"url": "https://b.test/"}),
    ));
    tool_call(
        &mut first_stream,
        42,
        "select_page",
        serde_json::json!({"pageId": 1}),
    );

    let first_snapshot = tool_call(
        &mut first_stream,
        43,
        "take_snapshot",
        serde_json::json!({}),
    );
    let second_snapshot = tool_call(
        &mut second_stream,
        44,
        "take_snapshot",
        serde_json::json!({}),
    );
    assert!(content_text(&first_snapshot).contains("page=1 "));
    assert!(content_text(&second_snapshot).contains(&format!("page={second_page}")));
}

#[test]
fn session_attach_sets_page_target() {
    let mut env = TestEnv::new("attachpage");
    env.start_daemon();
    let session = env.create_session();

    let mut control = env.connect();
    let attached = control_roundtrip(
        &mut control,
        &format!("session_attach session={session} page=1"),
    );
    assert_eq!(attached, format!("session={session} page=1"));

    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let snapshot = tool_call(&mut stream, 50, "take_snapshot", serde_json::json!({}));
    assert!(content_text(&snapshot).contains("page=1 "));
}

#[test]
fn uid_token_from_another_session_is_rejected_before_mcp_forward() {
    let mut env = TestEnv::new("foreignuid");
    env.start_daemon();
    let first = env.create_session();
    let second = env.create_session();
    let mut first_stream = env.connect();
    let mut second_stream = env.connect();
    bind_stream(&mut first_stream, &first);
    bind_stream(&mut second_stream, &second);

    tool_call(
        &mut first_stream,
        60,
        "new_page",
        serde_json::json!({"url": "https://a.test/"}),
    );
    tool_call(
        &mut second_stream,
        61,
        "new_page",
        serde_json::json!({"url": "https://b.test/"}),
    );
    let token = uid_token(&tool_call(
        &mut first_stream,
        62,
        "take_snapshot",
        serde_json::json!({}),
    ));
    let response = tool_call(
        &mut second_stream,
        63,
        "click",
        serde_json::json!({"uid": token}),
    );
    assert_eq!(
        response["error"]["message"],
        "uid token belongs to another session"
    );
}

#[test]
fn stale_uid_token_is_rejected_before_mcp_forward() {
    let mut env = TestEnv::new("staleuid");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    tool_call(
        &mut stream,
        70,
        "new_page",
        serde_json::json!({"url": "https://a.test/"}),
    );
    let old_token = uid_token(&tool_call(
        &mut stream,
        71,
        "take_snapshot",
        serde_json::json!({}),
    ));
    let new_snapshot = tool_call(&mut stream, 72, "take_snapshot", serde_json::json!({}));
    assert_ne!(old_token, uid_token(&new_snapshot));
    let response = tool_call(
        &mut stream,
        73,
        "click",
        serde_json::json!({"uid": old_token}),
    );
    assert_eq!(response["error"]["message"], "stale uid token");
}

#[test]
fn evaluate_script_and_nested_uid_fields_translate_tokens() {
    let mut env = TestEnv::new("uidfields");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let page = selected_page_id(&tool_call(
        &mut stream,
        80,
        "new_page",
        serde_json::json!({"url": "https://a.test/"}),
    ));
    let tokens = uid_tokens(&tool_call(
        &mut stream,
        81,
        "take_snapshot",
        serde_json::json!({}),
    ));
    let token = tokens[0].clone();
    let eval = tool_call(
        &mut stream,
        82,
        "evaluate_script",
        serde_json::json!({"function": "(el) => el.id", "args": [token.clone()]}),
    );
    assert_eq!(
        content_text(&eval),
        format!("evaluated page={page} arg={page}_button")
    );
    let form = tool_call(
        &mut stream,
        83,
        "fill_form",
        serde_json::json!({"elements": [{"uid": token.clone(), "value": "x"}]}),
    );
    assert_eq!(
        content_text(&form),
        format!("filled form page={page} raw={page}_button")
    );
    let drag = tool_call(
        &mut stream,
        84,
        "drag",
        serde_json::json!({"from_uid": token, "to_uid": tokens[1]}),
    );
    assert_eq!(
        content_text(&drag),
        format!("dragged page={page} from={page}_button to={page}_after")
    );
}

#[test]
fn upload_file_error_falls_back_to_cdp() {
    let mut env = TestEnv::new("uploadfb");
    env.start_daemon();
    let session = env.create_session();
    let file = env.home.join("upload.png");
    fs::write(&file, b"png").unwrap();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let token = uid_token(&tool_call(
        &mut stream,
        85,
        "take_snapshot",
        serde_json::json!({}),
    ));
    let upload = tool_call(
        &mut stream,
        86,
        "upload_file",
        serde_json::json!({"uid": token, "filePath": file.to_str().unwrap()}),
    );
    assert_eq!(content_text(&upload), "uploaded file via CDP fallback");
}

#[test]
fn native_tools_run_via_cdp_without_mcp() {
    let mut env = TestEnv::new("nativetools");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);

    let navigated = tool_call(
        &mut stream,
        198,
        "goto",
        serde_json::json!({
            "url": "https://navigated.test/",
            "waitExpression": "window.__ready",
            "interval": 50
        }),
    );
    assert!(
        content_text(&navigated).contains("navigated to https://navigated.test/"),
        "response: {navigated}"
    );
    let session_state = env.wait_session_contains(&session, "page=2");
    assert!(
        session_state.contains("page_created_by_daemon=true"),
        "fresh native call did not allocate a session page: {session_state}"
    );
    assert!(
        session_state.contains("page_url=https://navigated.test/"),
        "native navigation URL was not recorded: {session_state}"
    );

    let screenshot_path = env.home.join("quiet.png");
    let screenshot = tool_call(
        &mut stream,
        199,
        "screenshot_quiet",
        serde_json::json!({"filePath": screenshot_path.to_str().unwrap()}),
    );
    assert!(
        content_text(&screenshot).contains("saved screenshot"),
        "response: {screenshot}"
    );
    assert_eq!(fs::read(&screenshot_path).unwrap(), b"FAKETARGET2");

    let ready = tool_call(
        &mut stream,
        200,
        "wait_for_js",
        serde_json::json!({"expression": "window.__ready", "interval": 50}),
    );
    assert!(
        content_text(&ready).contains("condition became truthy"),
        "response: {ready}"
    );

    let timeout = tool_call(
        &mut stream,
        201,
        "wait_for_js",
        serde_json::json!({"expression": "window.__never", "timeout": 300, "interval": 50}),
    );
    assert_eq!(timeout["result"]["isError"], serde_json::json!(true));
    assert!(
        content_text(&timeout).contains("timed out"),
        "response: {timeout}"
    );

    let clicked = tool_call(
        &mut stream,
        202,
        "click_selector",
        serde_json::json!({"selector": "#btn"}),
    );
    assert!(
        content_text(&clicked).contains("clicked #btn at (100, 200)"),
        "response: {clicked}"
    );

    let clicked_at = tool_call(
        &mut stream,
        203,
        "click_at",
        serde_json::json!({"x": 10, "y": 20}),
    );
    assert!(
        content_text(&clicked_at).contains("clicked at (10, 20)"),
        "response: {clicked_at}"
    );

    let key = tool_call(
        &mut stream,
        204,
        "dispatch_key",
        serde_json::json!({"key": "Escape"}),
    );
    assert!(
        content_text(&key).contains("dispatched key Escape"),
        "response: {key}"
    );
}

fn screenshot_target(env: &TestEnv, stream: &mut UnixStream, id: u64, name: &str) -> String {
    let path = env.home.join(format!("{name}.png"));
    let response = tool_call(
        stream,
        id,
        "screenshot_quiet",
        serde_json::json!({"filePath": path.to_str().unwrap()}),
    );
    assert!(
        content_text(&response).contains("saved screenshot"),
        "response: {response}"
    );
    String::from_utf8(fs::read(&path).unwrap()).unwrap()
}

#[test]
fn two_sessions_with_the_same_url_keep_their_own_tab() {
    let mut env = TestEnv::new("sameurl");
    env.start_daemon();
    let first = env.create_session();
    let second = env.create_session();
    let mut first_stream = env.connect();
    let mut second_stream = env.connect();
    bind_stream(&mut first_stream, &first);
    bind_stream(&mut second_stream, &second);

    tool_call(
        &mut first_stream,
        300,
        "new_page",
        serde_json::json!({"url": "https://same.test/"}),
    );
    tool_call(
        &mut second_stream,
        301,
        "new_page",
        serde_json::json!({"url": "https://same.test/"}),
    );

    assert_eq!(
        screenshot_target(&env, &mut first_stream, 302, "first"),
        "FAKETARGET2"
    );
    assert_eq!(
        screenshot_target(&env, &mut second_stream, 303, "second"),
        "FAKETARGET3"
    );
    let key = tool_call(
        &mut first_stream,
        304,
        "dispatch_key",
        serde_json::json!({"key": "Escape"}),
    );
    assert!(
        content_text(&key).contains("dispatched key Escape"),
        "response: {key}"
    );
}

#[test]
fn select_page_pins_the_tab_among_tabs_with_the_same_url() {
    let mut env = TestEnv::new("selectsameurl");
    env.start_daemon();
    let owner = env.create_session();
    let other = env.create_session();
    let mut owner_stream = env.connect();
    let mut other_stream = env.connect();
    bind_stream(&mut owner_stream, &owner);
    bind_stream(&mut other_stream, &other);

    tool_call(
        &mut owner_stream,
        310,
        "new_page",
        serde_json::json!({"url": "https://same.test/"}),
    );
    tool_call(
        &mut owner_stream,
        311,
        "new_page",
        serde_json::json!({"url": "https://same.test/"}),
    );

    for (id, page_id, expected) in [(312, 2, "FAKETARGET2"), (314, 3, "FAKETARGET3")] {
        tool_call(
            &mut other_stream,
            id,
            "select_page",
            serde_json::json!({"pageId": page_id}),
        );
        assert_eq!(
            screenshot_target(&env, &mut other_stream, id + 1, "selected"),
            expected
        );
    }
}

#[test]
fn session_keeps_its_tab_after_the_url_changes() {
    let mut env = TestEnv::new("urlchange");
    env.start_daemon();
    let owner = env.create_session();
    let other = env.create_session();
    let mut owner_stream = env.connect();
    let mut other_stream = env.connect();
    bind_stream(&mut owner_stream, &owner);
    bind_stream(&mut other_stream, &other);

    tool_call(
        &mut owner_stream,
        320,
        "new_page",
        serde_json::json!({"url": "https://before.test/"}),
    );
    env.devtools[0].navigate("FAKETARGET2", "https://after.test/#/redirected");

    assert_eq!(
        screenshot_target(&env, &mut owner_stream, 321, "owner"),
        "FAKETARGET2"
    );

    tool_call(
        &mut other_stream,
        322,
        "select_page",
        serde_json::json!({"pageId": 2}),
    );
    assert_eq!(
        screenshot_target(&env, &mut other_stream, 323, "other"),
        "FAKETARGET2"
    );

    let navigated = tool_call(
        &mut owner_stream,
        324,
        "goto",
        serde_json::json!({"url": "https://third.test/", "waitExpression": "window.__ready", "interval": 50}),
    );
    assert!(
        content_text(&navigated).contains("navigated to https://third.test/"),
        "response: {navigated}"
    );
    assert_eq!(
        screenshot_target(&env, &mut owner_stream, 325, "owner-after-goto"),
        "FAKETARGET2"
    );
}

struct RealChrome {
    child: Child,
    port: u16,
    profile_dir: PathBuf,
}

impl RealChrome {
    fn find_binary() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("CHROME_DEVTOOLS_TEST_CHROME") {
            return Some(PathBuf::from(path));
        }
        let path_var = std::env::var_os("PATH")?;
        [
            "google-chrome-stable",
            "google-chrome",
            "chromium",
            "chromium-browser",
        ]
        .iter()
        .flat_map(|name| std::env::split_paths(&path_var).map(move |dir| dir.join(name)))
        .find(|candidate| candidate.is_file())
    }

    fn launch(binary: &Path, tag: &str) -> Self {
        let profile_dir = std::env::temp_dir().join(format!(
            "cdt-chrome-{tag}-{}-{}",
            std::process::id(),
            NEXT_TEST_ENV_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&profile_dir);
        fs::create_dir_all(&profile_dir).unwrap();
        let child = Command::new(binary)
            .args([
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--no-first-run",
                "--site-per-process",
                "--host-resolver-rules=MAP crossorigin.test 127.0.0.1",
                "--remote-debugging-port=0",
            ])
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(profile_dir.join("chrome.stderr")).unwrap())
            .spawn()
            .unwrap();
        let mut chrome = Self {
            child,
            port: 0,
            profile_dir,
        };
        let active_port = chrome.profile_dir.join("DevToolsActivePort");
        let started = Instant::now();
        chrome.port = loop {
            if let Some(port) = fs::read_to_string(&active_port).ok().and_then(|text| {
                text.lines()
                    .next()
                    .and_then(|line| line.parse::<u16>().ok())
            }) {
                break port;
            }
            chrome.assert_alive("publishing DevToolsActivePort");
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "Chrome did not publish DevToolsActivePort: {}",
                chrome.diagnostics()
            );
            thread::sleep(Duration::from_millis(100));
        };
        while !(http_get_body(chrome.port, "/json/version").contains("200 OK")
            && http_get_body(chrome.port, "/json").contains("\"type\": \"page\""))
        {
            chrome.assert_alive("serving DevTools HTTP");
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "Chrome DevTools HTTP did not become ready: {}",
                chrome.diagnostics()
            );
            thread::sleep(Duration::from_millis(100));
        }
        chrome
    }

    fn assert_alive(&mut self, phase: &str) {
        if let Ok(Some(status)) = self.child.try_wait() {
            panic!(
                "Chrome exited with {status} while {phase}: {}",
                self.diagnostics()
            );
        }
    }

    fn diagnostics(&self) -> String {
        let stderr = fs::read_to_string(self.profile_dir.join("chrome.stderr")).unwrap_or_default();
        let version = http_get_body(self.port, "/json/version");
        format!(
            "port={} /json/version={version:?} stderr={stderr}",
            self.port
        )
    }
}

fn http_get_body(port: u16, path: &str) -> String {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return String::new();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return String::new();
    }
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(bytes) = stream.read(&mut chunk) {
        if bytes == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..bytes]);
    }
    String::from_utf8_lossy(&response).to_string()
}

impl Drop for RealChrome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.profile_dir);
    }
}

struct FrameFixtureServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FrameFixtureServer {
    fn start(label: &'static str, cross_origin_url: String, log: Arc<Mutex<Vec<String>>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let log = Arc::clone(&log);
                let cross_origin_url = cross_origin_url.clone();
                thread::spawn(move || serve_frame_fixture(stream, label, &cross_origin_url, &log));
            }
        });
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for FrameFixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

const FRAME_CHILD_HTML: &str = r#"<!doctype html><body>
<input id="name" oninput="fetch('/report?name=' + encodeURIComponent(this.value))">
<input id="hidden" style="display:none">
<input type="file" id="file" onchange="fetch('/report?file=' + encodeURIComponent(this.files[0].name))">
<button id="go" onclick="fetch('/report?click=go')">go</button>
<iframe id="nested" src="/nested.html" width="300" height="80"></iframe>
</body>"#;

const FRAME_NESTED_HTML: &str = r#"<!doctype html><body>
<input id="deep" oninput="fetch('/report?deep=' + encodeURIComponent(this.value))">
</body>"#;

fn serve_frame_fixture(
    mut stream: TcpStream,
    label: &str,
    cross_origin_url: &str,
    log: &Mutex<Vec<String>>,
) {
    let mut buffer = [0u8; 4096];
    let bytes = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let body = if let Some(entry) = path.strip_prefix("/report?") {
        log.lock().unwrap().push(format!("{label}:{entry}"));
        String::new()
    } else if path == "/nested.html" {
        FRAME_NESTED_HTML.to_string()
    } else if path == "/child.html" {
        FRAME_CHILD_HTML.to_string()
    } else {
        format!(
            r#"<!doctype html><body><h1>parent</h1>
<input id="top" oninput="fetch('/report?top=' + encodeURIComponent(this.value))">
<iframe id="same" src="/child.html" style="position:absolute;left:60px;top:300px;width:400px;height:200px;border:3px solid red;padding:5px"></iframe>
<iframe id="cross" src="{cross_origin_url}/child.html" style="position:absolute;left:60px;top:800px;width:400px;height:200px"></iframe>
</body>"#
        )
    };
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
}

fn tool_error_text(response: &serde_json::Value) -> String {
    assert_eq!(
        response["result"]["isError"],
        serde_json::json!(true),
        "expected a tool error: {response}"
    );
    content_text(response).to_string()
}

fn wait_for_log(log: &Mutex<Vec<String>>, expected: &str) {
    let started = Instant::now();
    loop {
        if log.lock().unwrap().iter().any(|entry| entry == expected) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "log never contained {expected}: {:?}",
            log.lock().unwrap()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn retry_tool(
    stream: &mut UnixStream,
    first_id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let started = Instant::now();
    let mut id = first_id;
    loop {
        let response = tool_call(stream, id, name, arguments.clone());
        if response["result"]["isError"] != serde_json::json!(true) {
            return response;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{name} kept failing: {response}"
        );
        id += 1;
        thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn iframe_elements_are_reachable_with_the_frame_separator() {
    let Some(binary) = RealChrome::find_binary() else {
        eprintln!("skipping: no Chrome binary (set CHROME_DEVTOOLS_TEST_CHROME)");
        return;
    };
    let chrome = RealChrome::launch(&binary, "frames");
    let log = Arc::new(Mutex::new(Vec::new()));
    let cross_server = FrameFixtureServer::start("cross", String::new(), Arc::clone(&log));
    let cross_origin_url = format!("http://crossorigin.test:{}", cross_server.port);
    let parent_server = FrameFixtureServer::start("same", cross_origin_url, Arc::clone(&log));

    let mut env = TestEnv::new("frames");
    env.use_devtools_port(chrome.port);
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);

    let parent_url = format!("http://127.0.0.1:{}/", parent_server.port);
    let navigated = tool_call(
        &mut stream,
        400,
        "goto",
        serde_json::json!({"url": parent_url, "waitSelector": "iframe#cross"}),
    );
    assert!(
        content_text(&navigated).contains("navigated to"),
        "response: {navigated}"
    );

    retry_tool(
        &mut stream,
        410,
        "type_into",
        serde_json::json!({"selector": "iframe#same >>> #name", "text": "same-origin"}),
    );
    wait_for_log(&log, "same:name=same-origin");

    retry_tool(
        &mut stream,
        430,
        "type_into",
        serde_json::json!({"selector": "iframe#cross >>> #name", "text": "cross-origin"}),
    );
    wait_for_log(&log, "cross:name=cross-origin");

    retry_tool(
        &mut stream,
        450,
        "type_into",
        serde_json::json!({"selector": "iframe#cross >>> iframe#nested >>> #deep", "text": "nested"}),
    );
    wait_for_log(&log, "cross:deep=nested");

    let upload = env.home.join("upload.txt");
    fs::write(&upload, "attachment").unwrap();
    for (id, selector, label) in [
        (470, "iframe#same >>> #file", "same"),
        (471, "iframe#cross >>> #file", "cross"),
    ] {
        let response = tool_call(
            &mut stream,
            id,
            "set_file_input",
            serde_json::json!({"selector": selector, "filePaths": [upload.to_str().unwrap()]}),
        );
        assert!(
            content_text(&response).contains("attached 1 file(s)"),
            "response: {response}"
        );
        wait_for_log(&log, &format!("{label}:file=upload.txt"));
    }

    for (id, selector, label) in [
        (480, "iframe#same >>> #go", "same"),
        (481, "iframe#cross >>> #go", "cross"),
    ] {
        let response = tool_call(
            &mut stream,
            id,
            "click_selector",
            serde_json::json!({"selector": selector}),
        );
        assert!(
            content_text(&response).contains("clicked"),
            "response: {response}"
        );
        wait_for_log(&log, &format!("{label}:click=go"));
    }

    let missing_frame = tool_error_text(&tool_call(
        &mut stream,
        490,
        "type_into",
        serde_json::json!({"selector": "iframe#nosuch >>> #name", "text": "x"}),
    ));
    assert!(
        missing_frame.contains("no iframe matches selector: iframe#nosuch")
            && missing_frame.contains("top frame")
            && missing_frame.contains("iframes here: iframe#same, iframe#cross"),
        "message: {missing_frame}"
    );

    let missing_element = tool_error_text(&tool_call(
        &mut stream,
        491,
        "click_selector",
        serde_json::json!({"selector": "iframe#same >>> #nosuch"}),
    ));
    assert!(
        missing_element.contains("no element matches selector: #nosuch")
            && missing_element.contains("frame iframe#same")
            && missing_element.contains(&format!(
                "http://127.0.0.1:{}/child.html",
                parent_server.port
            )),
        "message: {missing_element}"
    );

    let hidden = tool_error_text(&tool_call(
        &mut stream,
        492,
        "type_into",
        serde_json::json!({"selector": "iframe#cross >>> #hidden", "text": "x"}),
    ));
    assert!(
        hidden.contains("could not focus selector")
            && hidden.contains("matched <input>, hidden")
            && hidden.contains("frame iframe#cross"),
        "message: {hidden}"
    );

    let not_iframe = tool_error_text(&tool_call(
        &mut stream,
        493,
        "type_into",
        serde_json::json!({"selector": "h1 >>> #name", "text": "x"}),
    ));
    assert!(
        not_iframe.contains("matches <h1>, not an iframe"),
        "message: {not_iframe}"
    );

    let top = tool_call(
        &mut stream,
        494,
        "type_into",
        serde_json::json!({"selector": "#top", "text": "top-level"}),
    );
    assert!(
        content_text(&top).contains("typed 9 char(s)"),
        "response: {top}"
    );
    wait_for_log(&log, "same:top=top-level");
}

#[test]
fn tools_list_includes_native_tools() {
    let mut env = TestEnv::new("nativelist");
    env.start_daemon();
    let mut stream = env.connect();
    let response = json_roundtrip(
        &mut stream,
        serde_json::json!({"jsonrpc": "2.0", "id": 210, "method": "tools/list", "params": {}}),
    );
    let names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for native in [
        "goto",
        "screenshot_quiet",
        "wait_for_js",
        "click_at",
        "click_selector",
        "dispatch_key",
    ] {
        assert!(names.contains(&native), "missing {native} in {names:?}");
    }
}

#[test]
fn include_snapshot_response_rewrites_returned_uids() {
    let mut env = TestEnv::new("includesnapshot");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let page = selected_page_id(&tool_call(
        &mut stream,
        90,
        "new_page",
        serde_json::json!({"url": "https://a.test/"}),
    ));
    let token = uid_token(&tool_call(
        &mut stream,
        91,
        "take_snapshot",
        serde_json::json!({}),
    ));
    let click = tool_call(
        &mut stream,
        92,
        "click",
        serde_json::json!({"uid": token, "includeSnapshot": true}),
    );
    let text = content_text(&click);
    assert!(text.contains(&format!("clicked page={page} raw={page}_button")));
    assert!(text.contains(&format!("snapshot page={page} uid=u:")));
}

#[test]
fn session_close_closes_daemon_created_page() {
    let mut env = TestEnv::new("closeowned");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let page = selected_page_id(&tool_call(
        &mut stream,
        100,
        "new_page",
        serde_json::json!({"url": "https://owned.test/"}),
    ));
    drop(stream);

    let (ok, output) = env.run_cli(&[
        "session",
        "close",
        "--profile",
        "default",
        "--session",
        &session,
    ]);
    assert!(ok, "session close failed: {output}");

    let probe = env.create_session();
    let mut probe_stream = env.connect();
    bind_stream(&mut probe_stream, &probe);
    let pages = tool_call(&mut probe_stream, 101, "list_pages", serde_json::json!({}));
    assert!(!page_ids(&pages).contains(&page), "pages: {pages}");
}

#[test]
fn session_close_keeps_attached_page() {
    let mut env = TestEnv::new("closeattached");
    env.start_daemon();
    let session = env.create_session();
    let mut control = env.connect();
    let attached = control_roundtrip(
        &mut control,
        &format!("session_attach session={session} page=1"),
    );
    assert_eq!(attached, format!("session={session} page=1"));

    let (ok, output) = env.run_cli(&[
        "session",
        "close",
        "--profile",
        "default",
        "--session",
        &session,
    ]);
    assert!(ok, "session close failed: {output}");

    let probe = env.create_session();
    let mut probe_stream = env.connect();
    bind_stream(&mut probe_stream, &probe);
    let pages = tool_call(&mut probe_stream, 110, "list_pages", serde_json::json!({}));
    assert!(page_ids(&pages).contains(&1), "pages: {pages}");
}

#[test]
fn session_expiry_closes_daemon_created_page() {
    let mut env = TestEnv::new_with_reaper("expireowned", Some(1), Some(50));
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let page = selected_page_id(&tool_call(
        &mut stream,
        120,
        "new_page",
        serde_json::json!({"url": "https://expire.test/"}),
    ));
    drop(stream);

    let started = Instant::now();
    loop {
        let (ok, output) = env.run_cli(&["session", "list", "--profile", "default"]);
        assert!(ok, "session list failed: {output}");
        if !output.contains(&session) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "session did not expire: {output}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    let probe = env.create_session();
    let mut probe_stream = env.connect();
    bind_stream(&mut probe_stream, &probe);
    let pages = tool_call(&mut probe_stream, 121, "list_pages", serde_json::json!({}));
    assert!(!page_ids(&pages).contains(&page), "pages: {pages}");
}

#[test]
fn last_tab_is_kept_when_daemon_created_page_is_closed() {
    let mut env = TestEnv::new("lasttab");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    tool_call(&mut stream, 130, "take_snapshot", serde_json::json!({}));
    drop(stream);

    let (ok, output) = env.run_cli(&[
        "session",
        "close",
        "--profile",
        "default",
        "--session",
        &session,
    ]);
    assert!(ok, "session close failed: {output}");

    let probe = env.create_session();
    let mut probe_stream = env.connect();
    bind_stream(&mut probe_stream, &probe);
    let pages = tool_call(&mut probe_stream, 131, "list_pages", serde_json::json!({}));
    assert_eq!(page_ids(&pages), vec![1]);
}

#[test]
fn daemon_status_reports_pages_and_queue() {
    let mut env = TestEnv::new("statuspages");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let page = selected_page_id(&tool_call(
        &mut stream,
        140,
        "new_page",
        serde_json::json!({"url": "https://status.test/"}),
    ));

    let (ok, output) = env.run_cli(&["daemon", "status", "--profile", "default"]);
    assert!(ok, "status failed: {output}");
    assert!(output.contains("active_sessions=1"), "output: {output}");
    assert!(
        output.contains(&format!("pages={page}")),
        "output: {output}"
    );
    assert!(output.contains("queued_mcp_requests=0"), "output: {output}");
    assert!(
        output.contains("max_control_latency_ms="),
        "output: {output}"
    );
    assert!(
        output.contains("max_forward_latency_ms="),
        "output: {output}"
    );
    assert!(
        output.contains("diagnostic_window_secs="),
        "output: {output}"
    );
}

#[test]
fn mcp_request_timeout_keeps_session_when_probe_succeeds() {
    let mut env = TestEnv::new_with_transient_mcp_timeout("mcpsurv", 1, 5, "take_snapshot", 2);
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);

    let response = tool_call(&mut stream, 145, "take_snapshot", serde_json::json!({}));
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MCP request timed out"),
        "response: {response}"
    );

    let (ok, output) = env.run_cli(&["session", "list", "--profile", "default"]);
    assert!(ok, "session list failed: {output}");
    assert!(output.contains(&session), "output: {output}");

    let eval = tool_call(
        &mut stream,
        146,
        "evaluate_script",
        serde_json::json!({"function": "() => location.href"}),
    );
    assert!(content_text(&eval).contains("evaluated page="));

    let output = env.wait_status_field_at_least("max_forward_latency_ms=", 1000);
    assert!(!output.contains("respawns="), "output: {output}");
}

#[test]
fn mcp_request_timeout_respawns_when_probe_fails() {
    let mut env = TestEnv::new_with_mcp_timeout("mcptimeout", 1, "take_snapshot");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);

    let response = tool_call(&mut stream, 150, "take_snapshot", serde_json::json!({}));
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MCP request timed out"),
        "response: {response}"
    );

    let started = Instant::now();
    let output = loop {
        let (ok, output) = env.run_cli(&["session", "list", "--profile", "default"]);
        assert!(ok, "session list failed: {output}");
        if !output.contains(&session) {
            break output;
        }
        if started.elapsed() > Duration::from_secs(5) {
            panic!("session stayed alive after respawn: {output}");
        }
        thread::sleep(Duration::from_millis(100));
    };
    assert!(!output.contains(&session), "output: {output}");

    let next_session = env.create_session();
    let mut next_stream = env.connect();
    bind_stream(&mut next_stream, &next_session);
    let eval = tool_call(
        &mut next_stream,
        151,
        "evaluate_script",
        serde_json::json!({"function": "() => location.href"}),
    );
    assert!(content_text(&eval).contains("evaluated page="));

    let (ok, output) = env.run_cli(&["daemon", "status", "--profile", "default"]);
    assert!(ok, "status failed: {output}");
    assert!(output.contains("respawns=1"), "output: {output}");
}

#[test]
fn devtools_port_change_respawns_mcp_and_drops_sessions() {
    let mut env = TestEnv::new("devresp");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let new_port = env.replace_devtools();

    let response = tool_call(
        &mut stream,
        160,
        "evaluate_script",
        serde_json::json!({"function": "() => location.href"}),
    );
    assert_eq!(
        response["error"]["message"],
        format!("unknown session: {session}")
    );

    let next_session = env.create_session();
    let mut next_stream = env.connect();
    bind_stream(&mut next_stream, &next_session);
    let eval = tool_call(
        &mut next_stream,
        161,
        "evaluate_script",
        serde_json::json!({"function": "() => location.href"}),
    );
    assert!(content_text(&eval).contains("evaluated page="));

    let (ok, output) = env.run_cli(&["daemon", "status", "--profile", "default"]);
    assert!(ok, "status failed: {output}");
    assert!(
        output.contains(&format!("port={new_port}")),
        "output: {output}"
    );
    assert!(output.contains("respawns=1"), "output: {output}");
}

#[test]
fn respawn_closes_daemon_created_pages() {
    let mut env = TestEnv::new_with_persistent_mcp("orphan");
    env.start_daemon();
    let session = env.create_session();
    let mut stream = env.connect();
    bind_stream(&mut stream, &session);
    let created = selected_page_id(&tool_call(
        &mut stream,
        165,
        "new_page",
        serde_json::json!({"url": "https://orphan.test/"}),
    ));
    assert_eq!(created, 2);
    env.replace_devtools();

    let response = tool_call(
        &mut stream,
        166,
        "evaluate_script",
        serde_json::json!({"function": "() => location.href"}),
    );
    assert_eq!(
        response["error"]["message"],
        format!("unknown session: {session}")
    );

    let next_session = env.create_session();
    let mut next_stream = env.connect();
    bind_stream(&mut next_stream, &next_session);
    let pages = tool_call(&mut next_stream, 167, "list_pages", serde_json::json!({}));
    assert_eq!(page_ids(&pages), vec![1]);
}

#[test]
fn stop_refused_while_sessions_active() {
    let mut env = TestEnv::new("stopguard");
    env.start_daemon();
    let session = env.create_session();

    let (ok, output) = env.run_cli(&["daemon", "stop", "--profile", "default"]);
    assert!(!ok, "stop should be refused: {output}");
    assert!(output.contains("active session(s)"), "output: {output}");

    let (ok, output) = env.run_cli(&["profile", "stop", "--profile", "default"]);
    assert!(!ok, "profile stop should be refused: {output}");
    assert!(output.contains("is running"), "output: {output}");

    let (ok, output) = env.run_cli(&[
        "session",
        "close",
        "--profile",
        "default",
        "--session",
        &session,
    ]);
    assert!(ok, "session close failed: {output}");
    let (ok, output) = env.run_cli(&["daemon", "stop", "--profile", "default"]);
    assert!(ok, "stop after close failed: {output}");
}

#[test]
fn status_reports_version_sessions_and_health() {
    let mut env = TestEnv::new("status");
    env.start_daemon();
    env.create_session();

    let (ok, output) = env.run_cli(&["daemon", "status", "--profile", "default"]);
    assert!(ok, "status failed: {output}");
    assert!(
        output.contains(&format!("version={}", env!("CARGO_PKG_VERSION"))),
        "output: {output}"
    );
    assert!(output.contains("sessions=1"), "output: {output}");
    assert!(output.contains("chrome=ready"), "output: {output}");

    let (ok, output) = env.run_cli(&["daemon", "stop", "--profile", "default", "--force"]);
    assert!(ok, "forced stop failed: {output}");
    let started = Instant::now();
    while env.socket_path().exists() && started.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(100));
    }

    let (ok, output) = env.run_cli(&["daemon", "status", "--profile", "default"]);
    assert!(ok, "status after stop failed: {output}");
    assert!(output.contains("daemon=stopped"), "output: {output}");
    assert!(output.contains("exit=ok"), "output: {output}");
}
