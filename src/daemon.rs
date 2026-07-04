use crate::config::*;
use crate::lock::*;
use crate::router::*;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

pub(crate) const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn daemon_dir() -> Result<PathBuf, String> {
    Ok(cache_dir()?.join("daemons"))
}

pub(crate) fn daemon_socket_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.sock", safe_lock_name(&profile.name))))
}

pub(crate) fn daemon_pid_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.pid", safe_lock_name(&profile.name))))
}

pub(crate) fn daemon_log_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.log", safe_lock_name(&profile.name))))
}

pub(crate) fn daemon_port_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.port", safe_lock_name(&profile.name))))
}

pub(crate) fn daemon_health_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.health", safe_lock_name(&profile.name))))
}

pub(crate) fn record_daemon_exit(profile: &Profile, result: &Result<(), String>) {
    let Ok(path) = daemon_health_path(profile) else {
        return;
    };
    let now = unix_secs(SystemTime::now());
    let line = match result {
        Ok(()) => format!("ts={now} exit=ok reason=stop\n"),
        Err(reason) => format!("ts={now} exit=error reason={}\n", reason.replace('\n', " ")),
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub(crate) fn read_daemon_health(profile: &Profile) -> Option<(String, usize)> {
    let path = daemon_health_path(profile).ok()?;
    let content = fs::read_to_string(path).ok()?;
    let abnormal = content
        .lines()
        .filter(|line| line.contains("exit=error"))
        .count();
    let last = content.lines().last()?.to_string();
    Some((last, abnormal))
}

pub(crate) fn read_runtime_port(profile: &Profile) -> Option<u16> {
    let path = daemon_port_path(profile).ok()?;
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse().ok()
}

pub(crate) fn write_runtime_port(profile: &Profile, port: u16) -> Result<(), String> {
    let path = daemon_port_path(profile)?;
    fs::write(&path, port.to_string())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

pub(crate) fn current_port(profile: &Profile) -> Option<u16> {
    read_runtime_port(profile)
}

pub(crate) fn pick_free_port() -> Result<u16, String> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .map_err(|error| format!("failed to acquire a free port: {error}"))?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("failed to read local addr: {error}"))?;
    Ok(addr.port())
}

pub(crate) fn start_daemon(profile: &Profile, quiet: bool) -> Result<(), String> {
    if is_daemon_ready(profile)? {
        if !quiet {
            println!(
                "profile={} daemon=ready socket={}",
                profile.name,
                daemon_socket_path(profile)?.display()
            );
        }
        return Ok(());
    }

    let dir = daemon_dir()?;
    fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    cleanup_stale_daemon_files(profile)?;

    let current_exe = env::current_exe()
        .map_err(|error| format!("failed to locate current executable: {error}"))?;
    let log_path = daemon_log_path(profile)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| format!("failed to open {}: {error}", log_path.display()))?;
    let log_for_stderr = log
        .try_clone()
        .map_err(|error| format!("failed to clone daemon log handle: {error}"))?;

    let child = Command::new(current_exe)
        .arg("daemon")
        .arg("run")
        .arg("--profile")
        .arg(&profile.name)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_for_stderr))
        .spawn()
        .map_err(|error| format!("failed to start daemon: {error}"))?;

    wait_for_daemon(profile, DAEMON_READY_TIMEOUT).map_err(|error| {
        format!(
            "failed to start daemon process {} for profile {}: {error}; log={}",
            child.id(),
            profile.name,
            log_path.display()
        )
    })?;

    if !quiet {
        println!(
            "profile={} daemon=started pid={} socket={}",
            profile.name,
            child.id(),
            daemon_socket_path(profile)?.display()
        );
    }
    Ok(())
}

pub(crate) fn run_daemon(profile: &Profile) -> Result<(), String> {
    let dir = daemon_dir()?;
    fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    let socket_path = daemon_socket_path(profile)?;
    let pid_path = daemon_pid_path(profile)?;
    let _lock = acquire_profile_lock(profile)?;

    if socket_path.exists() {
        fs::remove_file(&socket_path).map_err(|error| {
            format!(
                "failed to remove stale socket {}: {error}",
                socket_path.display()
            )
        })?;
    }

    ensure_chrome(profile)?;
    let mcp_port = current_port(profile)
        .ok_or_else(|| "runtime port is missing after ensure_chrome".to_string())?;
    let mut command = daemon_mcp_command(profile);
    eprintln!(
        "chrome-devtools {} daemon starting MCP: {:?}",
        env!("CARGO_PKG_VERSION"),
        command
    );
    let mut mcp = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("failed to run chrome-devtools-mcp: {error}"))?;
    let mut mcp_stdin = mcp
        .stdin
        .take()
        .ok_or_else(|| "failed to open chrome-devtools-mcp stdin".to_string())?;
    let mcp_stdout = mcp
        .stdout
        .take()
        .ok_or_else(|| "failed to open chrome-devtools-mcp stdout".to_string())?;
    let mut runtime = DaemonRuntimeGuard::new(mcp, socket_path.clone(), pid_path.clone());
    let mut mcp_reader = BufReader::new(mcp_stdout);
    initialize_daemon_mcp(&mut mcp_stdin, &mut mcp_reader)?;
    let sessions: SharedSessions =
        Arc::new((Mutex::new(SessionRegistry::default()), Condvar::new()));
    let router = RouterHandle::start(mcp_stdin, mcp_reader, Arc::clone(&sessions));

    fs::write(&pid_path, format!("{}\n", std::process::id()))
        .map_err(|error| format!("failed to write {}: {error}", pid_path.display()))?;

    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| format!("failed to bind {}: {error}", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("failed to configure {}: {error}", socket_path.display()))?;

    let sessions_for_reaper = Arc::clone(&sessions);
    thread::spawn(move || loop {
        thread::sleep(SESSION_REAPER_INTERVAL);
        let (lock, _) = &*sessions_for_reaper;
        if let Ok(mut registry) = lock.lock() {
            registry.reap_expired();
        }
    });

    let stopping = Arc::new(AtomicBool::new(false));
    let (stop_tx, stop_rx) = mpsc::channel();
    let mut daemon_result = Ok(());
    while !stopping.load(Ordering::SeqCst) {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        match runtime.child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                daemon_result = Err(format!("chrome-devtools-mcp exited with {status}"));
                break;
            }
            Err(error) => {
                daemon_result = Err(format!("failed to poll chrome-devtools-mcp: {error}"));
                break;
            }
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(error) => {
                eprintln!("warning: failed to accept daemon client: {error}");
                continue;
            }
        };
        let router = router.clone();
        let sessions = Arc::clone(&sessions);
        let stop_tx = stop_tx.clone();
        let stopping = Arc::clone(&stopping);
        thread::spawn(move || {
            match handle_daemon_client(stream, router, &sessions, mcp_port, bind_timeout()) {
                Ok(false) => {}
                Ok(true) => {
                    stopping.store(true, Ordering::SeqCst);
                    let _ = stop_tx.send(());
                }
                Err(DaemonError::Client(message)) => {
                    eprintln!("warning: daemon client connection failed: {message}");
                }
                Err(DaemonError::Fatal(message)) => {
                    eprintln!("error: daemon client fatal error: {message}");
                    stopping.store(true, Ordering::SeqCst);
                    let _ = stop_tx.send(());
                }
            }
        });
    }

    drop(runtime);
    record_daemon_exit(profile, &daemon_result);
    daemon_result
}

pub(crate) struct DaemonRuntimeGuard {
    child: Child,
    socket_path: PathBuf,
    pid_path: PathBuf,
}

impl DaemonRuntimeGuard {
    fn new(child: Child, socket_path: PathBuf, pid_path: PathBuf) -> Self {
        Self {
            child,
            socket_path,
            pid_path,
        }
    }
}

impl Drop for DaemonRuntimeGuard {
    fn drop(&mut self) {
        terminate_child(&mut self.child);
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.pid_path);
    }
}

pub(crate) fn initialize_daemon_mcp(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
) -> Result<(), String> {
    write_json_line(
        mcp_stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"roots":{"listChanged":false}},"clientInfo":{"name":"chrome-devtools-daemon","version":"0.1.0"}}}"#,
    )?;
    read_response(mcp_reader, mcp_stdin, 1)?;
    write_json_line(
        mcp_stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    )
}

pub(crate) fn create_session(profile: &Profile) -> Result<(), String> {
    ensure_daemon(profile)?;
    let response = send_daemon_control(profile, "session_create")?;
    let line = response
        .lines()
        .next()
        .ok_or_else(|| "daemon returned empty response".to_string())?;
    if let Some(message) = line.strip_prefix("error=") {
        return Err(message.to_string());
    }
    println!("{line}");
    Ok(())
}

pub(crate) fn list_sessions(profile: &Profile) -> Result<(), String> {
    if !is_daemon_ready(profile)? {
        return Ok(());
    }
    let response = send_daemon_control(profile, "session_list")?;
    print!("{response}");
    Ok(())
}

pub(crate) fn close_session(profile: &Profile, session_id: &str) -> Result<(), String> {
    if !is_daemon_ready(profile)? {
        return Err(format!("unknown session: {session_id}"));
    }
    let response = send_daemon_control(profile, &format!("session_close session={session_id}"))?;
    let line = response
        .lines()
        .next()
        .ok_or_else(|| "daemon returned empty response".to_string())?;
    if let Some(message) = line.strip_prefix("error=") {
        return Err(message.to_string());
    }
    println!("{line}");
    Ok(())
}

pub(crate) fn call_daemon(profile: &Profile, session_id: &str) -> Result<(), String> {
    ensure_daemon(profile)?;
    let socket_path = daemon_socket_path(profile)?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("failed to connect {}: {error}", socket_path.display()))?;
    let mut read_stream = stream
        .try_clone()
        .map_err(|error| format!("failed to clone daemon stream: {error}"))?;
    let mut daemon_reader = BufReader::new(&mut read_stream);

    bind_session(&mut stream, &mut daemon_reader, session_id, &profile.name)?;

    let stdin_to_daemon = thread::spawn(move || -> Result<(), String> {
        let mut stdin = std::io::stdin().lock();
        std::io::copy(&mut stdin, &mut stream)
            .map_err(|error| format!("failed to send daemon request: {error}"))?;
        stream
            .shutdown(Shutdown::Write)
            .map_err(|error| format!("failed to close daemon request stream: {error}"))?;
        Ok(())
    });

    let mut stdout = std::io::stdout().lock();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = daemon_reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read daemon response: {error}"))?;
        if bytes == 0 {
            break;
        }
        stdout
            .write_all(line.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(|error| format!("failed to write stdout: {error}"))?;
    }

    stdin_to_daemon
        .join()
        .map_err(|_| "stdin forwarding thread panicked".to_string())??;
    Ok(())
}

pub(crate) fn bind_session(
    stream: &mut UnixStream,
    reader: &mut impl BufRead,
    session_id: &str,
    profile_name: &str,
) -> Result<(), String> {
    let timeout = bind_timeout();
    let _ = stream.set_read_timeout(Some(timeout));
    let request = format!("__chrome_devtools_daemon__:bind session={session_id}\n");
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|error| format!("failed to send bind request: {error}"))?;
    let mut response = String::new();
    let bytes = reader.read_line(&mut response).map_err(|error| {
        if is_timeout_error(&error) {
            format!(
                "{DAEMON_BUSY_PREFIX}: bind not acknowledged within {}s; retry shortly (raise CHROME_DEVTOOLS_BIND_TIMEOUT_SECS to wait longer)",
                timeout.as_secs()
            )
        } else {
            format!("failed to read bind response: {error}")
        }
    })?;
    if bytes == 0 {
        return Err("daemon closed connection before bind response".to_string());
    }
    let response = response.trim_end();
    if let Some(message) = response.strip_prefix("error=") {
        if message.starts_with("unknown session") {
            return Err(format!(
                "{message}; sessions are dropped after 30 minutes idle or when the daemon restarts, mint a new one: chrome-devtools session create --profile {profile_name}"
            ));
        }
        if message.starts_with("session in use") {
            return Err(format!(
                "{message}; another invocation is bound to this session, wait for it or mint a separate one: chrome-devtools session create --profile {profile_name}"
            ));
        }
        return Err(message.to_string());
    }
    if !response.starts_with("bound=") {
        return Err(format!("unexpected bind response: {response}"));
    }
    let _ = stream.set_read_timeout(None);
    Ok(())
}

pub(crate) fn list_mcp_tools_via_daemon(profile: &Profile) -> Result<(), String> {
    ensure_daemon(profile)?;
    let request = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"roots":{"listChanged":false}},"clientInfo":{"name":"chrome-devtools","version":"0.1.0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        "\n",
    );
    let response = send_daemon_request(profile, request)?;
    if let Some(line) = response
        .lines()
        .rev()
        .find(|line| extract_jsonrpc_id(line) == Some(2))
    {
        println!("{line}");
    } else {
        print!("{response}");
    }
    Ok(())
}

pub(crate) fn ensure_daemon(profile: &Profile) -> Result<(), String> {
    if daemon_socket_path(profile)?.exists() {
        match send_daemon_control(profile, "status") {
            Ok(response) if response.contains("daemon=ready") => {
                warn_on_version_mismatch(profile, &response);
                return Ok(());
            }
            Err(error) if error.starts_with(DAEMON_BUSY_PREFIX) => return Ok(()),
            _ => {}
        }
    }
    start_daemon(profile, true)
}

pub(crate) fn warn_on_version_mismatch(profile: &Profile, status_response: &str) {
    let daemon_version = parse_status_field(status_response, "version=");
    let cli_version = env!("CARGO_PKG_VERSION");
    match daemon_version {
        Some(version) if version == cli_version => {}
        Some(version) => eprintln!(
            "warning: daemon for profile {} runs chrome-devtools {version} but this CLI is {cli_version}; restart it when idle: chrome-devtools daemon stop --profile {}",
            profile.name, profile.name
        ),
        None => eprintln!(
            "warning: daemon for profile {} was started by an older chrome-devtools than this CLI ({cli_version}); restart it when idle: chrome-devtools daemon stop --profile {}",
            profile.name, profile.name
        ),
    }
}

pub(crate) fn parse_status_field<'a>(response: &'a str, prefix: &str) -> Option<&'a str> {
    response
        .split_whitespace()
        .find_map(|part| part.strip_prefix(prefix))
}

pub(crate) fn wait_for_daemon(profile: &Profile, timeout: Duration) -> Result<(), String> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if is_daemon_ready(profile)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "daemon did not become ready within {} seconds",
        timeout.as_secs()
    ))
}

pub(crate) fn is_daemon_ready(profile: &Profile) -> Result<bool, String> {
    let socket_path = daemon_socket_path(profile)?;
    if !socket_path.exists() {
        return Ok(false);
    }
    match send_daemon_control(profile, "status") {
        Ok(response) => Ok(response.contains("daemon=ready")),
        Err(error) if error.starts_with(DAEMON_BUSY_PREFIX) => Ok(true),
        Err(_) => Ok(false),
    }
}

pub(crate) fn print_daemon_status(profile: &Profile) -> Result<(), String> {
    let socket_path = daemon_socket_path(profile)?;
    let pid_path = daemon_pid_path(profile)?;
    let pid = read_pid_file(&pid_path)
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let status_response = if socket_path.exists() {
        send_daemon_control(profile, "status").ok()
    } else {
        None
    };
    let health = read_daemon_health(profile)
        .map(|(last, abnormal)| format!(" abnormal_exits={abnormal} last_exit=[{last}]"))
        .unwrap_or_default();

    let Some(response) = status_response.filter(|response| response.contains("daemon=ready"))
    else {
        println!(
            "profile={} daemon=stopped pid={} socket={}{health}",
            profile.name,
            pid,
            socket_path.display()
        );
        return Ok(());
    };

    let version = parse_status_field(&response, "version=").unwrap_or("pre-0.3.1");
    let sessions = parse_status_field(&response, "sessions=").unwrap_or("unknown");
    let chrome = match parse_status_field(&response, "mcp_port=")
        .and_then(|port| port.parse::<u16>().ok())
    {
        Some(port) if is_devtools_ready(port) => format!("ready port={port}"),
        Some(port) => format!("unreachable port={port} (restart the daemon to reattach Chrome)"),
        None => "unknown".to_string(),
    };
    println!(
        "profile={} daemon=ready version={version} sessions={sessions} chrome={chrome} pid={pid} socket={}{health}",
        profile.name,
        socket_path.display()
    );
    Ok(())
}

pub(crate) fn stop_daemon(profile: &Profile, force: bool) -> Result<(), String> {
    if is_daemon_ready(profile)? {
        let command = if force { "stop force" } else { "stop" };
        let response = send_daemon_control(profile, command)?;
        if let Some(message) = response.trim_end().strip_prefix("error=") {
            return Err(message.to_string());
        }
        print!("{response}");
        wait_for_daemon_stop(profile, Duration::from_secs(5))?;
        return Ok(());
    }

    let pid_path = daemon_pid_path(profile)?;
    if let Some(pid) = read_pid_file(&pid_path) {
        let status = Command::new("kill")
            .arg(pid.to_string())
            .status()
            .map_err(|error| format!("failed to run kill: {error}"))?;
        if !status.success() {
            return Err(format!("kill exited with {status}"));
        }
    }
    cleanup_stale_daemon_files(profile)
}

pub(crate) fn wait_for_daemon_stop(profile: &Profile, timeout: Duration) -> Result<(), String> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !is_daemon_ready(profile)? {
            cleanup_stale_daemon_files(profile)?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "daemon did not stop within {} seconds",
        timeout.as_secs()
    ))
}

pub(crate) const DAEMON_BUSY_PREFIX: &str = "daemon busy";

pub(crate) fn control_timeout() -> Duration {
    timeout_from_env("CHROME_DEVTOOLS_CONTROL_TIMEOUT_SECS", 10)
}

pub(crate) fn bind_timeout() -> Duration {
    timeout_from_env("CHROME_DEVTOOLS_BIND_TIMEOUT_SECS", 120)
}

pub(crate) fn timeout_from_env(variable: &str, default_secs: u64) -> Duration {
    env::var(variable)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

pub(crate) fn is_timeout_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

pub(crate) fn send_daemon_control(profile: &Profile, command: &str) -> Result<String, String> {
    send_daemon_request(profile, &format!("__chrome_devtools_daemon__:{command}\n"))
}

pub(crate) fn send_daemon_request(profile: &Profile, request: &str) -> Result<String, String> {
    let socket_path = daemon_socket_path(profile)?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("failed to connect {}: {error}", socket_path.display()))?;
    let timeout = control_timeout();
    let _ = stream.set_read_timeout(Some(timeout));
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.shutdown(Shutdown::Write))
        .map_err(|error| format!("failed to send daemon request: {error}"))?;
    let mut response = String::new();
    match stream.read_to_string(&mut response) {
        Ok(_) => Ok(response),
        Err(error) if is_timeout_error(&error) => Err(format!(
            "{DAEMON_BUSY_PREFIX}: no response within {}s; another client is likely using the daemon, retry shortly (raise CHROME_DEVTOOLS_CONTROL_TIMEOUT_SECS to wait longer)",
            timeout.as_secs()
        )),
        Err(error) => Err(format!("failed to read daemon response: {error}")),
    }
}

pub(crate) fn read_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub(crate) fn cleanup_stale_daemon_files(profile: &Profile) -> Result<(), String> {
    let pid_path = daemon_pid_path(profile)?;
    if let Some(pid) = read_pid_file(&pid_path) {
        if process_exists(pid) {
            return Ok(());
        }
    }
    let socket_path = daemon_socket_path(profile)?;
    let _ = fs::remove_file(socket_path);
    let _ = fs::remove_file(pid_path);
    Ok(())
}

pub(crate) fn print_status(profile: &Profile) {
    match current_port(profile) {
        Some(port) if is_devtools_ready(port) => {
            println!(
                "profile={} status=ready port={} user_data_dir={}",
                profile.name, port, profile.user_data_dir
            );
        }
        _ => {
            println!(
                "profile={} status=stopped user_data_dir={}",
                profile.name, profile.user_data_dir
            );
        }
    }
}

pub(crate) fn default_chrome_binary() -> String {
    #[cfg(target_os = "macos")]
    {
        for candidate in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta",
            "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ] {
            if std::path::Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
    }
    "google-chrome-stable".to_string()
}

pub(crate) fn ensure_chrome(profile: &Profile) -> Result<(), String> {
    if let Some(port) = read_runtime_port(profile) {
        if is_devtools_ready(port) {
            return Ok(());
        }
    }

    if let Some(port) = find_running_chrome_port(profile) {
        if is_devtools_ready(port) {
            write_runtime_port(profile, port)?;
            return Ok(());
        }
    }

    let port = pick_free_port()?;
    let chrome = env::var("CHROME").unwrap_or_else(|_| default_chrome_binary());
    let user_data_dir = expand_home(&profile.user_data_dir)?;

    Command::new(chrome)
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start Chrome: {error}"))?;

    wait_for_devtools(port, Duration::from_secs(30))?;
    write_runtime_port(profile, port)
}

pub(crate) fn exec_mcp(profile: &Profile) -> Result<(), String> {
    let status = mcp_command(profile)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("failed to run chrome-devtools-mcp: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("chrome-devtools-mcp exited with {status}"))
    }
}

pub(crate) fn list_mcp_tools(profile: &Profile) -> Result<(), String> {
    let mut child = mcp_command(profile)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("failed to run chrome-devtools-mcp: {error}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open chrome-devtools-mcp stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open chrome-devtools-mcp stdout".to_string())?;
    let mut reader = BufReader::new(stdout);

    write_json_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"roots":{"listChanged":false}},"clientInfo":{"name":"chrome-devtools","version":"0.1.0"}}}"#,
    )?;
    read_response(&mut reader, &mut stdin, 1)?;

    write_json_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    )?;
    write_json_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    )?;

    let tools = read_response(&mut reader, &mut stdin, 2)?;
    println!("{tools}");

    terminate_child(&mut child);
    Ok(())
}

pub(crate) const DEFAULT_MCP_VERSION: &str = "1.5.0";

pub(crate) fn daemon_mcp_command(profile: &Profile) -> Command {
    let mut command = mcp_command(profile);
    command
        .arg("--experimentalPageIdRouting")
        .arg("--experimentalStructuredContent");
    command
}

pub(crate) fn mcp_command(profile: &Profile) -> Command {
    let mut command = if let Ok(program) = env::var("CHROME_DEVTOOLS_MCP_COMMAND") {
        Command::new(program)
    } else {
        let version = env::var("CHROME_DEVTOOLS_MCP_VERSION")
            .unwrap_or_else(|_| DEFAULT_MCP_VERSION.to_string());
        let mut command = Command::new("npx");
        command
            .arg("-y")
            .arg(format!("chrome-devtools-mcp@{version}"));
        command
    };
    let max_old_space_mb =
        env::var("CHROME_DEVTOOLS_MCP_MAX_OLD_SPACE_MB").unwrap_or_else(|_| "1024".to_string());
    let node_options = format!("--max-old-space-size={max_old_space_mb}");
    let merged = match env::var("NODE_OPTIONS") {
        Ok(existing) if !existing.is_empty() => format!("{existing} {node_options}"),
        _ => node_options,
    };
    command.env("NODE_OPTIONS", merged);
    let port = current_port(profile)
        .expect("ensure_chrome must run before mcp_command so the runtime port is recorded");
    command
        .arg("--browser-url")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg("--no-usage-statistics")
        .arg("--no-performance-crux");
    command
}

pub(crate) fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn find_running_chrome_port(profile: &Profile) -> Option<u16> {
    let user_data_dir = expand_home(&profile.user_data_dir).ok()?;
    let needle = format!("--user-data-dir={}", user_data_dir.display());

    for cmdline in list_browser_cmdlines() {
        if cmdline.contains("--type=") {
            continue;
        }
        if !cmdline.contains(&needle) {
            continue;
        }
        if let Some(port) = extract_remote_debugging_port(&cmdline) {
            return Some(port);
        }
    }
    None
}

#[cfg(target_os = "linux")]
pub(crate) fn list_browser_cmdlines() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline: String = bytes
            .iter()
            .map(|&b| if b == 0 { ' ' } else { b as char })
            .collect();
        out.push(cmdline);
    }
    out
}

#[cfg(target_os = "macos")]
pub(crate) fn list_browser_cmdlines() -> Vec<String> {
    let Ok(output) = Command::new("ps").args(["-ax", "-o", "command="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn list_browser_cmdlines() -> Vec<String> {
    Vec::new()
}

pub(crate) fn extract_remote_debugging_port(cmdline: &str) -> Option<u16> {
    let prefix = "--remote-debugging-port=";
    let start = cmdline.find(prefix)? + prefix.len();
    let rest = &cmdline[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

pub(crate) fn stop_profile(profile: &Profile, force: bool) -> Result<(), String> {
    if !force && is_daemon_ready(profile)? {
        return Err(format!(
            "daemon for profile {} is running and other agents may be using it; stopping Chrome would break their sessions. run 'chrome-devtools daemon stop --profile {}' first, or pass --force",
            profile.name, profile.name
        ));
    }
    let user_data_dir = expand_home(&profile.user_data_dir)?;
    let pattern = format!("--user-data-dir={}", user_data_dir.display());

    let status = Command::new("pkill")
        .arg("-f")
        .arg("--")
        .arg(&pattern)
        .status()
        .map_err(|error| format!("failed to run pkill: {error}"))?;

    if status.success() || status.code() == Some(1) {
        Ok(())
    } else {
        Err(format!("pkill exited with {status}"))
    }
}

pub(crate) fn wait_for_devtools(port: u16, timeout: Duration) -> Result<(), String> {
    let started = Instant::now();

    while started.elapsed() < timeout {
        if is_devtools_ready(port) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }

    Err(format!(
        "Chrome DevTools did not become ready on port {port} within {} seconds",
        timeout.as_secs()
    ))
}

pub(crate) fn is_devtools_ready(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(250)) else {
        return false;
    };

    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let request = b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if stream.write_all(request).is_err() {
        return false;
    }

    let mut response = [0; 4096];
    let Ok(bytes) = stream.read(&mut response) else {
        return false;
    };

    String::from_utf8_lossy(&response[..bytes]).contains("200 OK")
}
