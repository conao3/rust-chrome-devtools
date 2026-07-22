use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
}

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
            "cdt-{}-{}",
            tag.chars().take(6).collect::<String>(),
            std::process::id()
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
    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            let mut buffer = [0u8; 1024];
            let bytes = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..bytes]);
            if request.starts_with("GET /json ") {
                let body = format!(
                    r#"[{{"type":"page","url":"about:blank","webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/page/1"}}]"#
                );
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            } else if request.contains("Upgrade: websocket") {
                let _ = stream.write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: fake\r\n\r\n",
                );
                handle_fake_cdp(stream);
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
    }
}

fn handle_fake_cdp(mut stream: TcpStream) {
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
            "Runtime.evaluate" => {
                let expression = value["params"]["expression"].as_str().unwrap_or("");
                let result_value = if expression.contains("getBoundingClientRect") {
                    serde_json::json!({"x": 100.0, "y": 200.0})
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
    assert!(parsed[0]["error"]
        .as_str()
        .unwrap()
        .contains("unknown session"));
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
    assert!(args.contains("--experimentalPageIdRouting"), "args: {args}");

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
    for native in ["wait_for_js", "click_at", "click_selector", "dispatch_key"] {
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
