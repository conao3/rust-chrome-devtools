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
    assert!(
        output.contains("\"text\": \"ok\""),
        "batch output: {output}"
    );
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
#[ignore = "single-threaded accept; enable after the router redesign (M3)"]
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
