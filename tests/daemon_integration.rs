use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct TestEnv {
    home: PathBuf,
    daemon: Option<Child>,
}

impl TestEnv {
    fn new(tag: &str) -> Self {
        let home =
            std::env::temp_dir().join(format!("chrome-devtools-it-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".config/chrome-devtools")).unwrap();
        fs::create_dir_all(home.join(".cache/chrome-devtools/daemons")).unwrap();
        fs::write(
            home.join(".config/chrome-devtools/config.toml"),
            "[[profiles]]\nname = \"default\"\n",
        )
        .unwrap();

        let devtools_port = spawn_fake_devtools();
        fs::write(
            home.join(".cache/chrome-devtools/daemons/default.port"),
            devtools_port.to_string(),
        )
        .unwrap();

        Self { home, daemon: None }
    }

    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chrome-devtools"));
        command
            .env("HOME", &self.home)
            .env("CHROME_DEVTOOLS_MCP_COMMAND", fake_mcp_path())
            .env("CHROME", "/nonexistent-chrome-for-tests");
        command
    }

    fn start_daemon(&mut self) {
        let child = self
            .cli()
            .args(["daemon", "run", "--profile", "default"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let socket = self.socket_path();
        let started = Instant::now();
        while !socket.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "daemon socket did not appear within 10s"
            );
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

fn fake_mcp_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-mcp.sh")
}

fn spawn_fake_devtools() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"Browser\":\"fake\"}",
            );
        }
    });
    port
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

fn selected_page_id(response: &serde_json::Value) -> u64 {
    response["result"]["structuredContent"]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|page| page["selected"] == true)
        .and_then(|page| page["id"].as_u64())
        .unwrap()
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

    let first_click = tool_call(
        &mut first_stream,
        24,
        "click",
        serde_json::json!({"uid": format!("{first_page}_button")}),
    );
    let second_click = tool_call(
        &mut second_stream,
        25,
        "click",
        serde_json::json!({"uid": format!("{second_page}_button")}),
    );
    assert_eq!(
        content_text(&first_click),
        format!("clicked page={first_page} uid={first_page}_button")
    );
    assert_eq!(
        content_text(&second_click),
        format!("clicked page={second_page} uid={second_page}_button")
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
