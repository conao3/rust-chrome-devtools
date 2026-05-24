use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
struct Config {
    profiles: Vec<Profile>,
}

#[derive(Clone, Debug)]
struct Profile {
    name: String,
    user_data_dir: String,
}

#[derive(Default)]
struct ProfileBuilder {
    name: Option<String>,
    user_data_dir: Option<String>,
}

const CONFIG_RELATIVE_PATH: &str = ".config/chrome-devtools/config.toml";
const CACHE_RELATIVE_PATH: &str = ".cache/chrome-devtools";
const DEFAULT_PROFILE_NAME: &str = "default";
const DEFAULT_PROFILE_USER_DATA_DIR: &str = "~/.config/chrome-devtools/profiles/default";
const PROFILE_USER_DATA_DIR_PREFIX: &str = "~/.config/chrome-devtools/profiles";
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const SESSION_REAPER_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
struct SessionState {
    id: String,
    created_at: SystemTime,
    last_used_at: SystemTime,
    owned: bool,
}

#[derive(Default)]
struct SessionRegistry {
    sessions: HashMap<String, SessionState>,
}

impl SessionRegistry {
    fn create(&mut self) -> SessionState {
        let now = SystemTime::now();
        let id = generate_session_id();
        let state = SessionState {
            id: id.clone(),
            created_at: now,
            last_used_at: now,
            owned: false,
        };
        self.sessions.insert(id.clone(), state.clone());
        state
    }

    fn list(&self) -> Vec<SessionState> {
        let mut sessions = self.sessions.values().cloned().collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.created_at);
        sessions
    }

    fn close(&mut self, id: &str) -> Result<(), String> {
        if self.sessions.remove(id).is_some() {
            Ok(())
        } else {
            Err(format!("unknown session: {id}"))
        }
    }

    fn bind(&mut self, id: &str) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| format!("unknown session: {id}"))?;
        if session.owned {
            return Err(format!("session in use: {id}"));
        }
        session.owned = true;
        session.last_used_at = SystemTime::now();
        Ok(())
    }

    fn unbind(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.owned = false;
            session.last_used_at = SystemTime::now();
        }
    }

    fn touch(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.last_used_at = SystemTime::now();
        }
    }

    fn reap_expired(&mut self) {
        let now = SystemTime::now();
        self.sessions.retain(|_, session| {
            if session.owned {
                return true;
            }
            match now.duration_since(session.last_used_at) {
                Ok(elapsed) => elapsed < SESSION_IDLE_TTL,
                Err(_) => true,
            }
        });
    }
}

fn generate_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mix = nanos
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(pid.wrapping_mul(0xBF58476D1CE4E5B9))
        .wrapping_add(counter.wrapping_mul(0x94D049BB133111EB));
    format!("sess-{mix:016x}")
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(object) = args.next() else {
        print_usage();
        return Err("missing object".to_string());
    };

    if matches!(object.as_str(), "help" | "--help" | "-h") {
        print_usage();
        return Ok(());
    }
    if matches!(object.as_str(), "version" | "--version" | "-V") {
        print_version();
        return Ok(());
    }

    let config = load_or_create_config()?;
    let Some(action) = args.next() else {
        print_usage();
        return Err(format!("missing action for object: {object}"));
    };

    let rest = args.collect::<Vec<_>>();

    match (object.as_str(), action.as_str()) {
        ("mcp", "help" | "--help" | "-h") => {
            print_mcp_help();
            Ok(())
        }
        ("profile", "help" | "--help" | "-h") => {
            print_profile_help();
            Ok(())
        }
        ("daemon", "help" | "--help" | "-h") => {
            print_daemon_help();
            Ok(())
        }
        ("session", "help" | "--help" | "-h") => {
            print_session_help();
            Ok(())
        }
        ("session", "create") => {
            if wants_help(&rest) {
                print_session_create_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            create_session(&profile)
        }
        ("session", "list") => {
            if wants_help(&rest) {
                print_session_list_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            list_sessions(&profile)
        }
        ("session", "close") => {
            if wants_help(&rest) {
                print_session_close_help();
                return Ok(());
            }
            let (profile, session_id) = require_profile_and_session(&config, &rest)?;
            close_session(&profile, &session_id)
        }
        ("mcp", "call") => {
            if wants_help(&rest) {
                print_mcp_call_help();
                return Ok(());
            }
            let (profile, session_id) = require_profile_and_session(&config, &rest)?;
            call_daemon(&profile, &session_id)
        }
        ("mcp", "batch") => {
            if wants_help(&rest) {
                print_mcp_batch_help();
                return Ok(());
            }
            run_batch(&config, &rest)
        }
        ("mcp", "list") => {
            if wants_help(&rest) {
                print_mcp_list_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            list_mcp_tools_via_daemon(&profile)
        }
        ("mcp", "direct-call") => {
            if wants_help(&rest) {
                print_mcp_direct_call_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            let _lock = acquire_profile_lock(&profile)?;
            ensure_chrome(&profile)?;
            exec_mcp(&profile)
        }
        ("mcp", "direct-list") => {
            if wants_help(&rest) {
                print_mcp_direct_list_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            let _lock = acquire_profile_lock(&profile)?;
            ensure_chrome(&profile)?;
            list_mcp_tools(&profile)
        }
        ("profile", "status") => {
            if wants_help(&rest) {
                print_profile_status_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            print_status(&profile);
            Ok(())
        }
        ("profile", "stop") => {
            if wants_help(&rest) {
                print_profile_stop_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            stop_profile(&profile)
        }
        ("profile", "list") => {
            if wants_help(&rest) {
                print_profile_list_help();
                return Ok(());
            }
            reject_extra_args(&rest)?;
            list_profiles(&config);
            Ok(())
        }
        ("daemon", "start") => {
            if wants_help(&rest) {
                print_daemon_start_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            start_daemon(&profile, false)
        }
        ("daemon", "run") => {
            if wants_help(&rest) {
                print_daemon_run_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            run_daemon(&profile)
        }
        ("daemon", "status") => {
            if wants_help(&rest) {
                print_daemon_status_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            print_daemon_status(&profile)
        }
        ("daemon", "stop") => {
            if wants_help(&rest) {
                print_daemon_stop_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            stop_daemon(&profile)
        }
        _ => Err(format!("unknown command: {object} {action}")),
    }
}

fn wants_help(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
}

fn load_or_create_config() -> Result<Config, String> {
    let path = config_path()?;
    if !path.exists() {
        create_default_config(&path)?;
    }

    let content = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    parse_config(&content).map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn create_default_config(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!("config path has no parent: {}", path.display()));
    };

    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    fs::create_dir_all(expand_home(DEFAULT_PROFILE_USER_DATA_DIR)?)
        .map_err(|error| format!("failed to create default profile user data dir: {error}"))?;
    fs::write(path, default_config_content())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn default_config_content() -> String {
    format!("[[profiles]]\nname = \"{DEFAULT_PROFILE_NAME}\"\n")
}

fn parse_config(content: &str) -> Result<Config, String> {
    let mut profiles = Vec::new();
    let mut current: Option<ProfileBuilder> = None;

    for (line_number, raw_line) in content.lines().enumerate() {
        let line_number = line_number + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line == "[[profiles]]" {
            push_profile(&mut profiles, current.take(), line_number)?;
            current = Some(ProfileBuilder::default());
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {line_number}: expected key = value"));
        };
        let Some(profile) = current.as_mut() else {
            return Err(format!(
                "line {line_number}: profile fields must be inside [[profiles]]"
            ));
        };

        let key = key.trim();
        let value = value.trim();
        match key {
            "name" => profile.name = Some(parse_toml_string(value, line_number)?),
            "port" => {
                eprintln!(
                    "warning: line {line_number}: 'port' is deprecated and ignored; the daemon now picks a free port automatically"
                );
            }
            "user_data_dir" => profile.user_data_dir = Some(parse_toml_string(value, line_number)?),
            unknown => {
                return Err(format!(
                    "line {line_number}: unknown profile key: {unknown}"
                ))
            }
        }
    }

    push_profile(&mut profiles, current.take(), content.lines().count() + 1)?;

    if profiles.is_empty() {
        return Err("config must define at least one [[profiles]] entry".to_string());
    }

    Ok(Config { profiles })
}

fn push_profile(
    profiles: &mut Vec<Profile>,
    builder: Option<ProfileBuilder>,
    line_number: usize,
) -> Result<(), String> {
    let Some(builder) = builder else {
        return Ok(());
    };

    let name = builder
        .name
        .ok_or_else(|| format!("line {line_number}: profile is missing name"))?;
    let user_data_dir = builder
        .user_data_dir
        .unwrap_or_else(|| default_user_data_dir_for_profile(&name));

    if profiles.iter().any(|profile| profile.name == name) {
        return Err(format!("duplicate profile name: {name}"));
    }

    profiles.push(Profile {
        name,
        user_data_dir,
    });
    Ok(())
}

fn default_user_data_dir_for_profile(name: &str) -> String {
    format!("{PROFILE_USER_DATA_DIR_PREFIX}/{name}")
}

fn parse_toml_string(value: &str, line_number: usize) -> Result<String, String> {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return Err(format!("line {line_number}: expected quoted string"));
    };

    let mut parsed = String::new();
    let mut chars = inner.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            parsed.push(character);
            continue;
        }

        let Some(escaped) = chars.next() else {
            return Err(format!("line {line_number}: dangling escape in string"));
        };
        match escaped {
            '\\' => parsed.push('\\'),
            '"' => parsed.push('"'),
            'n' => parsed.push('\n'),
            'r' => parsed.push('\r'),
            't' => parsed.push('\t'),
            other => return Err(format!("line {line_number}: unsupported escape: \\{other}")),
        }
    }

    Ok(parsed)
}

fn config_path() -> Result<PathBuf, String> {
    let Some(home) = env::var_os("HOME") else {
        return Err("HOME is not set".to_string());
    };
    Ok(PathBuf::from(home).join(CONFIG_RELATIVE_PATH))
}

fn cache_dir() -> Result<PathBuf, String> {
    let Some(home) = env::var_os("HOME") else {
        return Err("HOME is not set".to_string());
    };
    Ok(PathBuf::from(home).join(CACHE_RELATIVE_PATH))
}

struct ProfileLock {
    path: PathBuf,
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_profile_lock(profile: &Profile) -> Result<ProfileLock, String> {
    let lock_dir = cache_dir()?.join("locks");
    fs::create_dir_all(&lock_dir)
        .map_err(|error| format!("failed to create {}: {error}", lock_dir.display()))?;

    let path = lock_dir.join(format!("{}.lock", safe_lock_name(&profile.name)));
    let timeout = lock_timeout();
    let started = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "pid={}", std::process::id())
                    .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
                writeln!(file, "profile={}", profile.name)
                    .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
                return Ok(ProfileLock { path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if remove_stale_lock(&path)? {
                    continue;
                }
                if started.elapsed() >= timeout {
                    return Err(format!(
                        "profile {} is locked by another chrome-devtools MCP session: {}",
                        profile.name,
                        path.display()
                    ));
                }
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => {
                return Err(format!("failed to create {}: {error}", path.display()));
            }
        }
    }
}

fn lock_timeout() -> Duration {
    env::var("CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(300))
}

fn remove_stale_lock(path: &Path) -> Result<bool, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("failed to read lock {}: {error}", path.display()))?;
    let Some(pid) = parse_lock_pid(&content) else {
        return Ok(false);
    };

    if process_exists(pid) {
        return Ok(false);
    }

    fs::remove_file(path)
        .map_err(|error| format!("failed to remove stale lock {}: {error}", path.display()))?;
    Ok(true)
}

fn parse_lock_pid(content: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        line.strip_prefix("pid=")
            .and_then(|value| value.trim().parse::<u32>().ok())
    })
}

fn process_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

fn safe_lock_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn daemon_dir() -> Result<PathBuf, String> {
    Ok(cache_dir()?.join("daemons"))
}

fn daemon_socket_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.sock", safe_lock_name(&profile.name))))
}

fn daemon_pid_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.pid", safe_lock_name(&profile.name))))
}

fn daemon_log_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.log", safe_lock_name(&profile.name))))
}

fn daemon_port_path(profile: &Profile) -> Result<PathBuf, String> {
    Ok(daemon_dir()?.join(format!("{}.port", safe_lock_name(&profile.name))))
}

fn read_runtime_port(profile: &Profile) -> Option<u16> {
    let path = daemon_port_path(profile).ok()?;
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse().ok()
}

fn write_runtime_port(profile: &Profile, port: u16) -> Result<(), String> {
    let path = daemon_port_path(profile)?;
    fs::write(&path, port.to_string())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn current_port(profile: &Profile) -> Option<u16> {
    read_runtime_port(profile)
}

fn pick_free_port() -> Result<u16, String> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .map_err(|error| format!("failed to acquire a free port: {error}"))?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("failed to read local addr: {error}"))?;
    Ok(addr.port())
}

fn start_daemon(profile: &Profile, quiet: bool) -> Result<(), String> {
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

fn run_daemon(profile: &Profile) -> Result<(), String> {
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
    let mut mcp = mcp_command(profile)
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
    let mut mcp_reader = BufReader::new(mcp_stdout);
    initialize_daemon_mcp(&mut mcp_stdin, &mut mcp_reader)?;

    fs::write(&pid_path, format!("{}\n", std::process::id()))
        .map_err(|error| format!("failed to write {}: {error}", pid_path.display()))?;

    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| format!("failed to bind {}: {error}", socket_path.display()))?;

    let sessions: Arc<Mutex<SessionRegistry>> = Arc::new(Mutex::new(SessionRegistry::default()));
    let sessions_for_reaper = Arc::clone(&sessions);
    thread::spawn(move || loop {
        thread::sleep(SESSION_REAPER_INTERVAL);
        if let Ok(mut registry) = sessions_for_reaper.lock() {
            registry.reap_expired();
        }
    });

    for stream in listener.incoming() {
        let mut stream =
            stream.map_err(|error| format!("failed to accept daemon client: {error}"))?;
        let should_stop =
            handle_daemon_client(&mut stream, &mut mcp_stdin, &mut mcp_reader, &sessions)?;
        if should_stop {
            break;
        }
        if let Some(status) = mcp
            .try_wait()
            .map_err(|error| format!("failed to poll chrome-devtools-mcp: {error}"))?
        {
            return Err(format!("chrome-devtools-mcp exited with {status}"));
        }
    }

    terminate_child(&mut mcp);
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&pid_path);
    Ok(())
}

fn initialize_daemon_mcp(
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

struct BoundSessionGuard<'a> {
    sessions: &'a Arc<Mutex<SessionRegistry>>,
    id: Option<String>,
}

impl<'a> BoundSessionGuard<'a> {
    fn new(sessions: &'a Arc<Mutex<SessionRegistry>>) -> Self {
        Self { sessions, id: None }
    }
}

impl Drop for BoundSessionGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            if let Ok(mut registry) = self.sessions.lock() {
                registry.unbind(&id);
            }
        }
    }
}

fn handle_daemon_client(
    stream: &mut UnixStream,
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    sessions: &Arc<Mutex<SessionRegistry>>,
) -> Result<bool, String> {
    let mut client_reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|error| format!("failed to clone daemon client stream: {error}"))?,
    );
    let mut line = String::new();
    let mut bound = BoundSessionGuard::new(sessions);

    loop {
        line.clear();
        let bytes = client_reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read daemon client request: {error}"))?;
        if bytes == 0 {
            return Ok(false);
        }

        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }

        if let Some(command) = line.strip_prefix("__chrome_devtools_daemon__:") {
            match handle_control_command(stream, sessions, &mut bound, command)? {
                ControlOutcome::Continue => continue,
                ControlOutcome::CloseConnection => return Ok(false),
                ControlOutcome::StopDaemon => return Ok(true),
            }
        }

        if json_has_method(line, "initialize") {
            if let Some(id) = extract_jsonrpc_id(line) {
                let response = daemon_initialize_response(id);
                stream
                    .write_all(response.as_bytes())
                    .and_then(|_| stream.write_all(b"\n"))
                    .and_then(|_| stream.flush())
                    .map_err(|error| {
                        format!("failed to write daemon initialize response: {error}")
                    })?;
            }
            continue;
        }
        if json_has_method(line, "notifications/initialized") {
            continue;
        }

        let forwarded = sanitize_outgoing_request(line);
        let Some(pending_id) = extract_jsonrpc_id(&forwarded) else {
            write_json_line(mcp_stdin, &forwarded)?;
            continue;
        };

        write_json_line(mcp_stdin, &forwarded)?;

        loop {
            let mut response_line = String::new();
            let bytes = mcp_reader
                .read_line(&mut response_line)
                .map_err(|error| format!("failed to read MCP response: {error}"))?;
            if bytes == 0 {
                return Err("chrome-devtools-mcp closed stdout before responding".to_string());
            }
            let response_line = response_line.trim_end().to_string();
            if json_has_method(&response_line, "roots/list") {
                if let Some(id) = extract_jsonrpc_id(&response_line) {
                    let response =
                        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"roots":[]}}}}"#);
                    write_json_line(mcp_stdin, &response)?;
                }
                continue;
            }

            stream
                .write_all(response_line.as_bytes())
                .and_then(|_| stream.write_all(b"\n"))
                .and_then(|_| stream.flush())
                .map_err(|error| format!("failed to write daemon client response: {error}"))?;

            if extract_jsonrpc_id(&response_line) == Some(pending_id) {
                if let Some(id) = bound.id.as_ref() {
                    if let Ok(mut registry) = sessions.lock() {
                        registry.touch(id);
                    }
                }
                break;
            }
        }
    }
}

enum ControlOutcome {
    Continue,
    CloseConnection,
    StopDaemon,
}

fn handle_control_command(
    stream: &mut UnixStream,
    sessions: &Arc<Mutex<SessionRegistry>>,
    bound: &mut BoundSessionGuard,
    command: &str,
) -> Result<ControlOutcome, String> {
    let (head, rest) = match command.split_once(' ') {
        Some((head, rest)) => (head, rest.trim()),
        None => (command, ""),
    };
    match head {
        "status" => {
            write_control_line(stream, "daemon=ready")?;
            Ok(ControlOutcome::CloseConnection)
        }
        "stop" => {
            write_control_line(stream, "daemon=stopping")?;
            Ok(ControlOutcome::StopDaemon)
        }
        "session_create" => {
            let state = sessions
                .lock()
                .map_err(|_| "session registry poisoned".to_string())?
                .create();
            write_control_line(stream, &format_session_line(&state))?;
            Ok(ControlOutcome::CloseConnection)
        }
        "session_list" => {
            let snapshot = sessions
                .lock()
                .map_err(|_| "session registry poisoned".to_string())?
                .list();
            for state in &snapshot {
                write_control_line(stream, &format_session_line(state))?;
            }
            Ok(ControlOutcome::CloseConnection)
        }
        "session_close" => {
            let id = parse_session_arg(rest)?;
            let result = sessions
                .lock()
                .map_err(|_| "session registry poisoned".to_string())?
                .close(&id);
            match result {
                Ok(()) => write_control_line(stream, &format!("closed={id}"))?,
                Err(message) => write_control_line(stream, &format!("error={message}"))?,
            }
            Ok(ControlOutcome::CloseConnection)
        }
        "bind" => {
            let id = parse_session_arg(rest)?;
            let result = sessions
                .lock()
                .map_err(|_| "session registry poisoned".to_string())?
                .bind(&id);
            match result {
                Ok(()) => {
                    bound.id = Some(id.clone());
                    write_control_line(stream, &format!("bound={id}"))?;
                    Ok(ControlOutcome::Continue)
                }
                Err(message) => {
                    write_control_line(stream, &format!("error={message}"))?;
                    Ok(ControlOutcome::CloseConnection)
                }
            }
        }
        other => {
            write_control_line(stream, &format!("error=unknown command: {other}"))?;
            Ok(ControlOutcome::CloseConnection)
        }
    }
}

fn write_control_line(stream: &mut UnixStream, body: &str) -> Result<(), String> {
    stream
        .write_all(body.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|error| format!("failed to write daemon response: {error}"))
}

fn format_session_line(state: &SessionState) -> String {
    format!(
        "session={} created={} last_used={} owned={}",
        state.id,
        unix_secs(state.created_at),
        unix_secs(state.last_used_at),
        state.owned
    )
}

fn parse_session_arg(args: &str) -> Result<String, String> {
    for part in args.split_whitespace() {
        if let Some(value) = part.strip_prefix("session=") {
            if value.is_empty() {
                return Err("session id must not be empty".to_string());
            }
            return Ok(value.to_string());
        }
    }
    Err("missing session=<id> argument".to_string())
}

fn create_session(profile: &Profile) -> Result<(), String> {
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

fn list_sessions(profile: &Profile) -> Result<(), String> {
    if !is_daemon_ready(profile)? {
        return Ok(());
    }
    let response = send_daemon_control(profile, "session_list")?;
    print!("{response}");
    Ok(())
}

fn close_session(profile: &Profile, session_id: &str) -> Result<(), String> {
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

fn call_daemon(profile: &Profile, session_id: &str) -> Result<(), String> {
    ensure_daemon(profile)?;
    let socket_path = daemon_socket_path(profile)?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("failed to connect {}: {error}", socket_path.display()))?;
    let mut read_stream = stream
        .try_clone()
        .map_err(|error| format!("failed to clone daemon stream: {error}"))?;
    let mut daemon_reader = BufReader::new(&mut read_stream);

    bind_session(&mut stream, &mut daemon_reader, session_id)?;

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

fn bind_session(
    stream: &mut UnixStream,
    reader: &mut impl BufRead,
    session_id: &str,
) -> Result<(), String> {
    let request = format!("__chrome_devtools_daemon__:bind session={session_id}\n");
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|error| format!("failed to send bind request: {error}"))?;
    let mut response = String::new();
    let bytes = reader
        .read_line(&mut response)
        .map_err(|error| format!("failed to read bind response: {error}"))?;
    if bytes == 0 {
        return Err("daemon closed connection before bind response".to_string());
    }
    let response = response.trim_end();
    if let Some(message) = response.strip_prefix("error=") {
        return Err(message.to_string());
    }
    if !response.starts_with("bound=") {
        return Err(format!("unexpected bind response: {response}"));
    }
    Ok(())
}

struct BatchOptions {
    profile_name: String,
    session_id: String,
    script_path: String,
    output_path: Option<String>,
    fail_fast: bool,
}

fn run_batch(config: &Config, args: &[String]) -> Result<(), String> {
    let options = parse_batch_args(args)?;
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

    bind_session(&mut stream, &mut reader, &options.session_id)?;

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
            .ok_or_else(|| "step missing 'type'".to_string())?;
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
                    .ok_or_else(|| "tool requires 'name'".to_string())?;
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
            other => return Err(format!("unknown step type: {other}")),
        }
    }

    let _ = stream.shutdown(Shutdown::Write);

    let output = serde_json::to_string_pretty(&serde_json::Value::Array(results))
        .map_err(|error| format!("failed to serialize results: {error}"))?;
    if let Some(path) = &options.output_path {
        fs::write(path, &output).map_err(|error| format!("failed to write {path}: {error}"))?;
    } else {
        println!("{output}");
    }

    if let Some(label) = stopped_on_error {
        return Err(format!("scenario stopped on error at step: {label}"));
    }
    Ok(())
}

fn read_script_source(path: &str) -> Result<String, String> {
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

fn parse_batch_args(args: &[String]) -> Result<BatchOptions, String> {
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

/// Recursively resolve `{ "$ref": "<label>.<path>..." }` markers using `results`.
/// `path` segments are dot-separated; numeric segments index into arrays.
fn resolve_refs(
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

fn resolve_path(path: &str, results: &[serde_json::Value]) -> serde_json::Value {
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

fn find_profile(config: &Config, name: &str) -> Result<Profile, String> {
    config
        .profiles
        .iter()
        .find(|profile| profile.name == name)
        .cloned()
        .ok_or_else(|| format!("unknown profile: {name}"))
}

fn list_mcp_tools_via_daemon(profile: &Profile) -> Result<(), String> {
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

fn ensure_daemon(profile: &Profile) -> Result<(), String> {
    if is_daemon_ready(profile)? {
        ensure_chrome(profile)?;
        return Ok(());
    }
    start_daemon(profile, true)
}

fn wait_for_daemon(profile: &Profile, timeout: Duration) -> Result<(), String> {
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

fn is_daemon_ready(profile: &Profile) -> Result<bool, String> {
    let socket_path = daemon_socket_path(profile)?;
    if !socket_path.exists() {
        return Ok(false);
    }
    match send_daemon_control(profile, "status") {
        Ok(response) => Ok(response.contains("daemon=ready")),
        Err(_) => Ok(false),
    }
}

fn print_daemon_status(profile: &Profile) -> Result<(), String> {
    let socket_path = daemon_socket_path(profile)?;
    let pid_path = daemon_pid_path(profile)?;
    let ready = is_daemon_ready(profile)?;
    let pid = read_pid_file(&pid_path)
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!(
        "profile={} daemon={} pid={} socket={}",
        profile.name,
        if ready { "ready" } else { "stopped" },
        pid,
        socket_path.display()
    );
    Ok(())
}

fn stop_daemon(profile: &Profile) -> Result<(), String> {
    if is_daemon_ready(profile)? {
        let response = send_daemon_control(profile, "stop")?;
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

fn wait_for_daemon_stop(profile: &Profile, timeout: Duration) -> Result<(), String> {
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

fn send_daemon_control(profile: &Profile, command: &str) -> Result<String, String> {
    send_daemon_request(profile, &format!("__chrome_devtools_daemon__:{command}\n"))
}

fn send_daemon_request(profile: &Profile, request: &str) -> Result<String, String> {
    let socket_path = daemon_socket_path(profile)?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("failed to connect {}: {error}", socket_path.display()))?;
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.shutdown(Shutdown::Write))
        .map_err(|error| format!("failed to send daemon request: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("failed to read daemon response: {error}"))?;
    Ok(response)
}

fn read_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn cleanup_stale_daemon_files(profile: &Profile) -> Result<(), String> {
    let pid_path = daemon_pid_path(profile)?;
    if let Some(pid) = read_pid_file(&pid_path) {
        if process_exists(pid) {
            return Ok(());
        }
    }
    let socket_path = daemon_socket_path(profile)?;
    let port_path = daemon_port_path(profile)?;
    let _ = fs::remove_file(socket_path);
    let _ = fs::remove_file(pid_path);
    let _ = fs::remove_file(port_path);
    Ok(())
}

fn reject_extra_args(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("unknown argument: {}", args[0]))
    }
}

fn require_profile(config: &Config, args: &[String]) -> Result<Profile, String> {
    let mut profile_name = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--profile" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--profile requires a value".to_string());
                };
                profile_name = Some(value.as_str());
                index += 2;
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }

    let Some(profile_name) = profile_name else {
        return Err("--profile is required".to_string());
    };

    config
        .profiles
        .iter()
        .find(|profile| profile.name == profile_name)
        .cloned()
        .ok_or_else(|| format!("unknown profile: {profile_name}"))
}

fn require_profile_and_session(
    config: &Config,
    args: &[String],
) -> Result<(Profile, String), String> {
    let mut profile_name = None;
    let mut session_id = None;
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
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }

    let profile_name = profile_name.ok_or_else(|| "--profile is required".to_string())?;
    let session_id = session_id.ok_or_else(|| "--session is required".to_string())?;
    let profile = config
        .profiles
        .iter()
        .find(|profile| profile.name == profile_name)
        .cloned()
        .ok_or_else(|| format!("unknown profile: {profile_name}"))?;
    Ok((profile, session_id))
}

fn list_profiles(config: &Config) {
    for profile in &config.profiles {
        println!("{}\tuser_data_dir={}", profile.name, profile.user_data_dir);
    }
}

fn print_status(profile: &Profile) {
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

fn default_chrome_binary() -> String {
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

fn ensure_chrome(profile: &Profile) -> Result<(), String> {
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
        .arg("--disable-gpu")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start Chrome: {error}"))?;

    wait_for_devtools(port, Duration::from_secs(30))?;
    write_runtime_port(profile, port)
}

fn exec_mcp(profile: &Profile) -> Result<(), String> {
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

fn list_mcp_tools(profile: &Profile) -> Result<(), String> {
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

fn mcp_command(profile: &Profile) -> Command {
    let mut command = if let Ok(program) = env::var("CHROME_DEVTOOLS_MCP_COMMAND") {
        Command::new(program)
    } else {
        let mut command = Command::new("npx");
        command.arg("-y").arg("chrome-devtools-mcp@latest");
        command
    };
    let port = current_port(profile)
        .expect("ensure_chrome must run before mcp_command so the runtime port is recorded");
    command
        .arg("--browser-url")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg("--no-usage-statistics")
        .arg("--no-performance-crux");
    command
}

fn write_json_line(stdin: &mut impl Write, json: &str) -> Result<(), String> {
    stdin
        .write_all(json.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("failed to write MCP request: {error}"))
}

fn read_response(
    reader: &mut impl BufRead,
    stdin: &mut impl Write,
    target_id: u64,
) -> Result<String, String> {
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read MCP response: {error}"))?;
        if bytes == 0 {
            return Err("chrome-devtools-mcp closed stdout before responding".to_string());
        }

        let line = line.trim_end().to_string();
        if json_has_method(&line, "roots/list") {
            if let Some(id) = extract_jsonrpc_id(&line) {
                let response = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"roots":[]}}}}"#);
                write_json_line(stdin, &response)?;
            }
            continue;
        }

        if extract_jsonrpc_id(&line) == Some(target_id) {
            return Ok(line);
        }
    }
}

fn daemon_initialize_response(id: u64) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"serverInfo":{{"name":"chrome-devtools-daemon","version":"0.1.0"}}}}}}"#
    )
}

fn sanitize_outgoing_request(line: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    let Some(obj) = value.as_object_mut() else {
        return line.to_string();
    };
    let method_is_tools_call = obj
        .get("method")
        .and_then(|m| m.as_str())
        .map(|m| m == "tools/call")
        .unwrap_or(false);
    if !method_is_tools_call {
        return line.to_string();
    }
    let Some(params) = obj.get_mut("params").and_then(|p| p.as_object_mut()) else {
        return line.to_string();
    };
    let name_is_new_page = params
        .get("name")
        .and_then(|n| n.as_str())
        .map(|n| n == "new_page")
        .unwrap_or(false);
    if !name_is_new_page {
        return line.to_string();
    }
    let Some(args) = params.get_mut("arguments").and_then(|a| a.as_object_mut()) else {
        return line.to_string();
    };
    if args.remove("isolatedContext").is_none() {
        return line.to_string();
    }
    eprintln!(
        "warning: stripped 'isolatedContext' from new_page; isolated browser contexts disable extensions"
    );
    serde_json::to_string(&value).unwrap_or_else(|_| line.to_string())
}

fn json_has_method(line: &str, method: &str) -> bool {
    compact_json_line(line).contains(&format!(r#""method":"{method}""#))
}

fn compact_json_line(line: &str) -> String {
    line.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn extract_jsonrpc_id(line: &str) -> Option<u64> {
    let compact = compact_json_line(line);
    let marker = r#""id":"#;
    let start = compact.find(marker)? + marker.len();
    let rest = &compact[start..];
    let digits = rest
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn find_running_chrome_port(profile: &Profile) -> Option<u16> {
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
fn list_browser_cmdlines() -> Vec<String> {
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
fn list_browser_cmdlines() -> Vec<String> {
    let Ok(output) = Command::new("ps").args(["-ax", "-o", "command="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn list_browser_cmdlines() -> Vec<String> {
    Vec::new()
}

fn extract_remote_debugging_port(cmdline: &str) -> Option<u16> {
    let prefix = "--remote-debugging-port=";
    let start = cmdline.find(prefix)? + prefix.len();
    let rest = &cmdline[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn stop_profile(profile: &Profile) -> Result<(), String> {
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

fn wait_for_devtools(port: u16, timeout: Duration) -> Result<(), String> {
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

fn is_devtools_ready(port: u16) -> bool {
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

fn expand_home(path: &str) -> Result<PathBuf, String> {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_string());
    }

    if let Some(rest) = path.strip_prefix("~/") {
        let Some(home) = env::var_os("HOME") else {
            return Err("HOME is not set".to_string());
        };
        return Ok(PathBuf::from(home).join(rest));
    }

    Ok(PathBuf::from(path))
}

fn print_usage() {
    eprintln!(
        "Usage:\n  chrome-devtools --version\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile> --session <id>\n  chrome-devtools mcp batch --profile <profile> --session <id> --script <path>\n  chrome-devtools mcp direct-list --profile <profile>\n  chrome-devtools mcp direct-call --profile <profile>\n  chrome-devtools mcp help\n  chrome-devtools session create --profile <profile>\n  chrome-devtools session list --profile <profile>\n  chrome-devtools session close --profile <profile> --session <id>\n  chrome-devtools daemon start --profile <profile>\n  chrome-devtools daemon status --profile <profile>\n  chrome-devtools daemon stop --profile <profile>\n  chrome-devtools profile status --profile <profile>\n  chrome-devtools profile stop --profile <profile>\n  chrome-devtools profile list\n\nConfig:\n  ~/.config/chrome-devtools/config.toml is created on startup if missing.\n\nConcurrency:\n  MCP commands take a per-profile lock under ~/.cache/chrome-devtools/locks.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the default 300 second wait."
    );
}

fn print_version() {
    println!("chrome-devtools {}", env!("CARGO_PKG_VERSION"));
}

fn print_mcp_help() {
    println!(
        "chrome-devtools mcp\n\nUsage:\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile> --session <id>\n  chrome-devtools mcp batch --profile <profile> --session <id> --script <path>\n  chrome-devtools mcp direct-list --profile <profile>\n  chrome-devtools mcp direct-call --profile <profile>\n  chrome-devtools mcp help\n\nCommands:\n  list         Start the selected profile daemon if needed, query tools/list through it, and print the raw MCP JSON response.\n\n  call         Start the selected profile daemon if needed, bind the named session, and forward stdin MCP JSON-RPC lines through its long-lived MCP process.\n\n  batch        Bind the named session and run a JSON batch file of tool/sleep steps through the profile daemon. Prints a JSON array of results.\n\n  direct-list  Bypass the daemon, run chrome-devtools-mcp directly, query tools/list, and print the raw MCP JSON response.\n\n  direct-call  Bypass the daemon, run chrome-devtools-mcp directly over stdio. Use only for fallback/manual debugging.\n\n  help         Show this help.\n\nOptions:\n  -h, --help   Show this help and exit.\n\nExamples:\n  chrome-devtools daemon start --profile default\n  chrome-devtools mcp list --profile default\n  ID=$(chrome-devtools session create --profile default | awk -F= '{{print $2}}' | awk '{{print $1}}')\n  chrome-devtools mcp call --profile default --session \"$ID\"\n  chrome-devtools mcp batch --profile default --session \"$ID\" --script /tmp/batch.json\n\nConfig:\n  Profiles are read from ~/.config/chrome-devtools/config.toml.\n  If the config file is missing, chrome-devtools creates a default profile using ~/.config/chrome-devtools/profiles/default.\n  user_data_dir is optional; when omitted, it defaults to ~/.config/chrome-devtools/profiles/<profile-name>.\n  Prefer user_data_dir values under ~/.config/chrome-devtools/profiles/<profile-name>.\n\nSessions:\n  mcp call and mcp batch require --session <id>. Mint one with `session create`.\n  Sessions live in-memory on the daemon and expire after 30 minutes of inactivity.\n\nDaemon:\n  mcp call, mcp list and mcp batch route through one long-lived per-profile daemon by default.\n  Daemon sockets and pid files live under ~/.cache/chrome-devtools/daemons.\n  direct-call and direct-list bypass the daemon and take a per-profile lock under ~/.cache/chrome-devtools/locks.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the direct-mode/default daemon lock wait.\n\nNotes:\n  Profiles define the Chrome user data directory. The daemon picks a free DevTools port automatically.\n  The call/batch commands do not reimplement MCP tools; they delegate to a daemon-owned chrome-devtools-mcp process.\n  Snapshot uid values are only valid inside the MCP process that produced them.\n  Daemon-routed calls preserve that MCP process across invocations until the daemon stops."
    );
}

fn print_profile_help() {
    println!(
        "chrome-devtools profile\n\nUsage:\n  chrome-devtools profile list\n  chrome-devtools profile status --profile <profile>\n  chrome-devtools profile stop --profile <profile>\n\nCommands:\n  list    List all profiles defined in the config file.\n  status  Show whether the Chrome instance bound to the given profile is running.\n  stop    Stop the Chrome instance bound to the given profile.\n\nOptions:\n  -h, --help  Show this help and exit."
    );
}

fn print_daemon_help() {
    println!(
        "chrome-devtools daemon\n\nUsage:\n  chrome-devtools daemon start --profile <profile>\n  chrome-devtools daemon status --profile <profile>\n  chrome-devtools daemon stop --profile <profile>\n\nCommands:\n  start   Start a background daemon for the profile, or report that one is already ready.\n  status  Show whether the per-profile daemon is ready, along with its pid and socket path.\n  stop    Ask the per-profile daemon to stop and clean up its socket/pid files.\n\nOptions:\n  -h, --help  Show this help and exit.\n\nNotes:\n  Daemon metadata lives under ~/.cache/chrome-devtools/daemons."
    );
}

fn print_mcp_call_help() {
    println!(
        "chrome-devtools mcp call\n\nUsage:\n  chrome-devtools mcp call --profile <profile> --session <id>\n\nDescription:\n  Start the selected profile daemon if needed, bind the named session, then\n  forward stdin MCP JSON-RPC lines through its long-lived chrome-devtools-mcp\n  process and print responses. The session is held for the lifetime of this\n  invocation; activity refreshes its 30 minute idle timer.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --session <id>    Required. Session id minted by `session create`.\n  -h, --help        Show this help and exit.\n\nExamples:\n  ID=$(chrome-devtools session create --profile default | awk -F= '{{print $2}}' | awk '{{print $1}}')\n  printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{{}}}}' \\\n    | chrome-devtools mcp call --profile default --session \"$ID\""
    );
}

fn print_mcp_batch_help() {
    println!(
        "chrome-devtools mcp batch\n\nUsage:\n  chrome-devtools mcp batch --profile <profile> --session <id> --script <path> [--output <path>] [--fail-fast]\n\nDescription:\n  Read a JSON array of steps from --script, bind the named session, and\n  execute each step in order through the profile daemon (one initialize\n  handshake, then a tools/call per tool step). Prints a JSON array of\n  results to stdout. Activity refreshes the session's 30 minute idle timer.\n\nStep shapes:\n  {{\"type\":\"tool\",\"name\":\"<mcp-tool>\",\"args\":{{...}},\"label\":\"<optional>\",\"on_error\":\"continue|stop\"}}\n  {{\"type\":\"sleep_ms\",\"ms\":<u64>,\"label\":\"<optional>\"}}\n\nValue references inside args:\n  Replace any value in args with {{\"$ref\":\"<label>.<path>\"}} to substitute it\n  with a previous result. <path> is dot-separated; numeric segments index\n  arrays. Example: {{\"$ref\":\"snap.result.content.0.text\"}} resolves to the\n  text of the first content entry returned by the step labelled 'snap'.\n\nResult shape (per step):\n  {{\"type\":\"tool\",\"name\":\"...\",\"label\":\"...\",\"result\":<mcp tools/call result>,\"error\":<mcp error or null>}}\n  {{\"type\":\"sleep_ms\",\"ms\":<u64>,\"label\":\"...\"}}\n\nError handling:\n  A tool step is considered to have errored if the MCP response carries a\n  non-null 'error' field or the result has isError=true. By default the\n  batch continues; pass --fail-fast or set on_error=stop on a step to\n  stop execution after that error. When stopped, batch writes the partial\n  results to stdout/--output and exits non-zero.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --session <id>    Required. Session id minted by `session create`.\n  --script <path>   Required. Path to a JSON file with the step array, or `-` for stdin.\n  --output <path>   Optional. Write the JSON results to <path> instead of stdout.\n  --fail-fast       Optional. Stop on the first errored tool step.\n  -h, --help        Show this help and exit.\n\nExamples:\n  cat > /tmp/batch.json <<'EOF'\n  [\n    {{\"type\":\"tool\",\"name\":\"navigate_page\",\"args\":{{\"type\":\"reload\",\"timeout\":15000}}}},\n    {{\"type\":\"sleep_ms\",\"ms\":5000}},\n    {{\"type\":\"tool\",\"name\":\"evaluate_script\",\"label\":\"title\",\"args\":{{\"function\":\"() => document.title\"}}}}\n  ]\n  EOF\n  ID=$(chrome-devtools session create --profile default | awk -F= '{{print $2}}' | awk '{{print $1}}')\n  chrome-devtools mcp batch --profile default --session \"$ID\" --script /tmp/batch.json"
    );
}

fn print_mcp_list_help() {
    println!(
        "chrome-devtools mcp list\n\nUsage:\n  chrome-devtools mcp list --profile <profile>\n\nDescription:\n  Start the selected profile daemon if needed, query tools/list through it,\n  and print the raw MCP JSON response.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_mcp_direct_call_help() {
    println!(
        "chrome-devtools mcp direct-call\n\nUsage:\n  chrome-devtools mcp direct-call --profile <profile>\n\nDescription:\n  Bypass the daemon and run chrome-devtools-mcp directly over stdio.\n  Use only for fallback/manual debugging; this mode cannot preserve snapshot\n  state across independent process invocations.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit.\n\nNotes:\n  Acquires a per-profile lock under ~/.cache/chrome-devtools/locks.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the 300 second wait."
    );
}

fn print_mcp_direct_list_help() {
    println!(
        "chrome-devtools mcp direct-list\n\nUsage:\n  chrome-devtools mcp direct-list --profile <profile>\n\nDescription:\n  Bypass the daemon, run chrome-devtools-mcp directly, query tools/list, and\n  print the raw MCP JSON response.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit.\n\nNotes:\n  Acquires a per-profile lock under ~/.cache/chrome-devtools/locks."
    );
}

fn print_profile_status_help() {
    println!(
        "chrome-devtools profile status\n\nUsage:\n  chrome-devtools profile status --profile <profile>\n\nDescription:\n  Show whether the Chrome DevTools endpoint for the given profile is reachable,\n  along with its runtime port and user_data_dir.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_profile_stop_help() {
    println!(
        "chrome-devtools profile stop\n\nUsage:\n  chrome-devtools profile stop --profile <profile>\n\nDescription:\n  Stop the Chrome instance bound to the given profile by matching processes\n  whose command line contains --user-data-dir=<profile user_data_dir>.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_profile_list_help() {
    println!(
        "chrome-devtools profile list\n\nUsage:\n  chrome-devtools profile list\n\nDescription:\n  Print all profiles defined in ~/.config/chrome-devtools/config.toml, one per\n  line, as: <name>\\tuser_data_dir=<path>.\n\nOptions:\n  -h, --help  Show this help and exit."
    );
}

fn print_daemon_start_help() {
    println!(
        "chrome-devtools daemon start\n\nUsage:\n  chrome-devtools daemon start --profile <profile>\n\nDescription:\n  Start a background daemon for the profile if one is not already running.\n  The daemon owns one chrome-devtools-mcp process and serializes MCP calls\n  over a Unix socket under ~/.cache/chrome-devtools/daemons.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_daemon_run_help() {
    println!(
        "chrome-devtools daemon run\n\nUsage:\n  chrome-devtools daemon run --profile <profile>\n\nDescription:\n  Run the per-profile broker in the foreground. This subcommand is normally\n  spawned by `daemon start` and is not intended to be invoked directly.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_daemon_status_help() {
    println!(
        "chrome-devtools daemon status\n\nUsage:\n  chrome-devtools daemon status --profile <profile>\n\nDescription:\n  Print whether the per-profile daemon is ready or stopped, with its pid and\n  Unix socket path.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_daemon_stop_help() {
    println!(
        "chrome-devtools daemon stop\n\nUsage:\n  chrome-devtools daemon stop --profile <profile>\n\nDescription:\n  Ask the per-profile daemon to stop and clean up its socket and pid files.\n  If the daemon is unreachable but a pid file exists, fall back to sending it\n  a TERM signal via kill.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_session_help() {
    println!(
        "chrome-devtools session\n\nUsage:\n  chrome-devtools session create --profile <profile>\n  chrome-devtools session list --profile <profile>\n  chrome-devtools session close --profile <profile> --session <id>\n\nCommands:\n  create  Mint a new session id on the profile daemon.\n  list    List active sessions held by the profile daemon.\n  close   Close (drop) the named session.\n\nOptions:\n  -h, --help  Show this help and exit.\n\nNotes:\n  Sessions live in-memory on the profile daemon. They are dropped after\n  30 minutes of inactivity or when the daemon stops.\n  mcp call and mcp batch require --session <id>; use session create to mint it."
    );
}

fn print_session_create_help() {
    println!(
        "chrome-devtools session create\n\nUsage:\n  chrome-devtools session create --profile <profile>\n\nDescription:\n  Start the profile daemon if needed, then ask it to mint a new in-memory\n  session id. The session is dropped after 30 minutes of inactivity or when\n  the daemon stops. Prints one line to stdout:\n\n    session=<id> created=<unix-ts> last_used=<unix-ts> owned=false\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_session_list_help() {
    println!(
        "chrome-devtools session list\n\nUsage:\n  chrome-devtools session list --profile <profile>\n\nDescription:\n  List active sessions held by the profile daemon. Each session is printed\n  on one line as:\n\n    session=<id> created=<unix-ts> last_used=<unix-ts> owned=<true|false>\n\n  Prints nothing when the daemon is not running.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_session_close_help() {
    println!(
        "chrome-devtools session close\n\nUsage:\n  chrome-devtools session close --profile <profile> --session <id>\n\nDescription:\n  Ask the profile daemon to drop the named session. Fails if the session is\n  unknown or the daemon is not running.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --session <id>    Required. Session id minted by `session create`.\n  -h, --help        Show this help and exit."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parse_config_accepts_single_minimal_profile() {
        let config = parse_config("[[profiles]]\nname = \"default\"\n").unwrap();
        assert_eq!(config.profiles.len(), 1);
        let profile = &config.profiles[0];
        assert_eq!(profile.name, "default");
        assert_eq!(
            profile.user_data_dir,
            "~/.config/chrome-devtools/profiles/default"
        );
    }

    #[test]
    fn parse_config_accepts_explicit_user_data_dir() {
        let config =
            parse_config("[[profiles]]\nname = \"work\"\nuser_data_dir = \"/tmp/work\"\n").unwrap();
        assert_eq!(config.profiles[0].user_data_dir, "/tmp/work");
    }

    #[test]
    fn parse_config_ignores_deprecated_port_field() {
        let config = parse_config("[[profiles]]\nname = \"legacy\"\nport = 9222\n").unwrap();
        assert_eq!(config.profiles[0].name, "legacy");
    }

    #[test]
    fn parse_config_accepts_multiple_profiles_with_comments() {
        let toml = "
            # leading comment
            [[profiles]]
            name = \"a\"

            [[profiles]]
            name = \"b\"
        ";
        let config = parse_config(toml).unwrap();
        let names: Vec<_> = config
            .profiles
            .iter()
            .map(|profile| &profile.name)
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn parse_config_rejects_empty_config() {
        assert!(parse_config("").is_err());
    }

    #[test]
    fn parse_config_rejects_duplicate_profile_names() {
        let toml = "[[profiles]]\nname = \"a\"\n[[profiles]]\nname = \"a\"\n";
        let error = parse_config(toml).unwrap_err();
        assert!(error.contains("duplicate profile name"));
    }

    #[test]
    fn parse_config_rejects_field_outside_profile_block() {
        assert!(parse_config("name = \"x\"\n").is_err());
    }

    #[test]
    fn parse_config_rejects_unknown_key() {
        let toml = "[[profiles]]\nname = \"a\"\nweird = \"x\"\n";
        assert!(parse_config(toml).is_err());
    }

    #[test]
    fn parse_toml_string_handles_escapes() {
        assert_eq!(
            parse_toml_string("\"line\\nbreak\"", 1).unwrap(),
            "line\nbreak"
        );
        assert_eq!(parse_toml_string("\"a\\\\b\"", 1).unwrap(), "a\\b");
        assert_eq!(parse_toml_string("\"a\\\"b\"", 1).unwrap(), "a\"b");
    }

    #[test]
    fn parse_toml_string_rejects_unquoted() {
        assert!(parse_toml_string("foo", 1).is_err());
    }

    #[test]
    fn parse_toml_string_rejects_unknown_escape() {
        assert!(parse_toml_string("\"\\q\"", 1).is_err());
    }

    #[test]
    fn safe_lock_name_keeps_allowed_characters() {
        assert_eq!(safe_lock_name("abc_def-123"), "abc_def-123");
    }

    #[test]
    fn safe_lock_name_replaces_disallowed_characters() {
        assert_eq!(safe_lock_name("a/b c.d"), "a_b_c_d");
    }

    #[test]
    fn parse_lock_pid_extracts_pid_line() {
        assert_eq!(parse_lock_pid("pid=4242\nprofile=default\n"), Some(4242));
        assert_eq!(parse_lock_pid("profile=default\npid=7\n"), Some(7));
    }

    #[test]
    fn parse_lock_pid_returns_none_without_pid() {
        assert_eq!(parse_lock_pid("profile=default\n"), None);
    }

    #[test]
    fn compact_json_line_strips_whitespace() {
        assert_eq!(compact_json_line(" { \"id\" : 1 } "), "{\"id\":1}");
    }

    #[test]
    fn extract_jsonrpc_id_reads_numeric_id() {
        assert_eq!(
            extract_jsonrpc_id(r#"{"jsonrpc":"2.0","id":42,"result":{}}"#),
            Some(42)
        );
        assert_eq!(extract_jsonrpc_id(r#"{ "id" : 7 }"#), Some(7));
    }

    #[test]
    fn extract_jsonrpc_id_returns_none_when_missing() {
        assert_eq!(extract_jsonrpc_id(r#"{"result":{}}"#), None);
    }

    #[test]
    fn json_has_method_detects_method() {
        assert!(json_has_method(
            r#"{"jsonrpc":"2.0","method":"initialize","params":{}}"#,
            "initialize"
        ));
        assert!(!json_has_method(
            r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
            "initialize"
        ));
    }

    #[test]
    fn parse_session_arg_accepts_session_assignment() {
        assert_eq!(parse_session_arg("session=abc").unwrap(), "abc");
        assert_eq!(parse_session_arg("session=foo other=bar").unwrap(), "foo");
    }

    #[test]
    fn parse_session_arg_rejects_missing_session() {
        assert!(parse_session_arg("").is_err());
        assert!(parse_session_arg("other=bar").is_err());
        assert!(parse_session_arg("session=").is_err());
    }

    #[test]
    fn format_session_line_renders_all_fields() {
        let now = SystemTime::now();
        let state = SessionState {
            id: "sess-test".to_string(),
            created_at: now,
            last_used_at: now,
            owned: true,
        };
        let line = format_session_line(&state);
        assert!(line.starts_with("session=sess-test "));
        assert!(line.contains(" owned=true"));
        assert!(line.contains(" created="));
        assert!(line.contains(" last_used="));
    }

    #[test]
    fn generate_session_id_produces_unique_ids() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..100 {
            assert!(ids.insert(generate_session_id()));
        }
    }

    #[test]
    fn session_registry_create_and_list() {
        let mut registry = SessionRegistry::default();
        let first = registry.create();
        let second = registry.create();
        assert_ne!(first.id, second.id);
        let listed = registry.list();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].created_at <= listed[1].created_at);
    }

    #[test]
    fn session_registry_close_removes_known_and_errors_on_unknown() {
        let mut registry = SessionRegistry::default();
        let state = registry.create();
        assert!(registry.close(&state.id).is_ok());
        assert!(registry.list().is_empty());
        let error = registry.close("sess-missing").unwrap_err();
        assert!(error.contains("unknown session"));
    }

    #[test]
    fn session_registry_bind_marks_owned_and_rejects_second_bind() {
        let mut registry = SessionRegistry::default();
        let state = registry.create();
        assert!(registry.bind(&state.id).is_ok());
        assert!(registry
            .sessions
            .get(&state.id)
            .map(|session| session.owned)
            .unwrap_or(false));
        let error = registry.bind(&state.id).unwrap_err();
        assert!(error.contains("session in use"));
    }

    #[test]
    fn session_registry_unbind_clears_owned() {
        let mut registry = SessionRegistry::default();
        let state = registry.create();
        registry.bind(&state.id).unwrap();
        registry.unbind(&state.id);
        let session = registry.sessions.get(&state.id).unwrap();
        assert!(!session.owned);
    }

    #[test]
    fn session_registry_touch_updates_last_used_at() {
        let mut registry = SessionRegistry::default();
        let state = registry.create();
        let before = registry.sessions.get(&state.id).unwrap().last_used_at;
        std::thread::sleep(Duration::from_millis(5));
        registry.touch(&state.id);
        let after = registry.sessions.get(&state.id).unwrap().last_used_at;
        assert!(after > before);
    }

    #[test]
    fn session_registry_reap_expired_drops_only_expired_unowned_sessions() {
        let mut registry = SessionRegistry::default();
        let fresh = registry.create();
        let expired_idle = registry.create();
        let expired_owned = registry.create();
        let stale = SystemTime::now() - SESSION_IDLE_TTL - Duration::from_secs(60);
        registry
            .sessions
            .get_mut(&expired_idle.id)
            .unwrap()
            .last_used_at = stale;
        let owned = registry.sessions.get_mut(&expired_owned.id).unwrap();
        owned.last_used_at = stale;
        owned.owned = true;
        registry.reap_expired();
        let remaining: Vec<_> = registry
            .list()
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert!(remaining.contains(&fresh.id));
        assert!(remaining.contains(&expired_owned.id));
        assert!(!remaining.contains(&expired_idle.id));
    }

    #[test]
    fn resolve_refs_substitutes_ref_with_previous_result() {
        let results = vec![serde_json::json!({
            "label": "snap",
            "result": {"content": [{"text": "hello"}]}
        })];
        let template = serde_json::json!({
            "value": {"$ref": "snap.result.content.0.text"}
        });
        let resolved = resolve_refs(template, &results).unwrap();
        assert_eq!(resolved, serde_json::json!({"value": "hello"}));
    }

    #[test]
    fn resolve_refs_returns_null_for_unknown_label() {
        let results: Vec<serde_json::Value> = vec![];
        let template = serde_json::json!({"value": {"$ref": "missing.x"}});
        let resolved = resolve_refs(template, &results).unwrap();
        assert_eq!(resolved, serde_json::json!({"value": null}));
    }

    #[test]
    fn resolve_refs_passes_through_non_refs() {
        let template = serde_json::json!({"a": 1, "b": [2, 3]});
        let resolved = resolve_refs(template.clone(), &[]).unwrap();
        assert_eq!(resolved, template);
    }

    #[test]
    fn wants_help_detects_help_flags() {
        assert!(wants_help(&args(&["--help"])));
        assert!(wants_help(&args(&["-h"])));
        assert!(wants_help(&args(&["help"])));
        assert!(!wants_help(&args(&["--profile", "default"])));
    }

    fn dummy_config() -> Config {
        Config {
            profiles: vec![Profile {
                name: "default".to_string(),
                user_data_dir: "/tmp/x".to_string(),
            }],
        }
    }

    #[test]
    fn require_profile_returns_named_profile() {
        let profile = require_profile(&dummy_config(), &args(&["--profile", "default"])).unwrap();
        assert_eq!(profile.name, "default");
    }

    #[test]
    fn require_profile_rejects_missing_flag_and_unknown_name() {
        assert!(require_profile(&dummy_config(), &[]).is_err());
        assert!(require_profile(&dummy_config(), &args(&["--profile", "other"])).is_err());
    }

    #[test]
    fn require_profile_and_session_returns_both() {
        let (profile, session) = require_profile_and_session(
            &dummy_config(),
            &args(&["--profile", "default", "--session", "sess-x"]),
        )
        .unwrap();
        assert_eq!(profile.name, "default");
        assert_eq!(session, "sess-x");
    }

    #[test]
    fn require_profile_and_session_rejects_missing_session() {
        assert!(
            require_profile_and_session(&dummy_config(), &args(&["--profile", "default"]),)
                .is_err()
        );
    }

    #[test]
    fn parse_batch_args_collects_all_flags() {
        let options = parse_batch_args(&args(&[
            "--profile",
            "default",
            "--session",
            "sess-1",
            "--script",
            "/tmp/x.json",
            "--output",
            "/tmp/out.json",
            "--fail-fast",
        ]))
        .unwrap();
        assert_eq!(options.profile_name, "default");
        assert_eq!(options.session_id, "sess-1");
        assert_eq!(options.script_path, "/tmp/x.json");
        assert_eq!(options.output_path.as_deref(), Some("/tmp/out.json"));
        assert!(options.fail_fast);
    }

    #[test]
    fn parse_batch_args_rejects_missing_required_flags() {
        assert!(
            parse_batch_args(&args(&["--session", "sess-1", "--script", "/tmp/x.json"])).is_err()
        );
        assert!(
            parse_batch_args(&args(&["--profile", "default", "--script", "/tmp/x.json"])).is_err()
        );
        assert!(parse_batch_args(&args(&["--profile", "default", "--session", "sess-1"])).is_err());
    }

    #[test]
    fn expand_home_resolves_tilde_paths() {
        let prev = env::var("HOME").ok();
        env::set_var("HOME", "/tmp/fakehome");
        assert_eq!(expand_home("~").unwrap(), PathBuf::from("/tmp/fakehome"));
        assert_eq!(
            expand_home("~/foo").unwrap(),
            PathBuf::from("/tmp/fakehome/foo")
        );
        assert_eq!(
            expand_home("/abs/path").unwrap(),
            PathBuf::from("/abs/path")
        );
        if let Some(value) = prev {
            env::set_var("HOME", value);
        } else {
            env::remove_var("HOME");
        }
    }

    #[test]
    fn daemon_initialize_response_includes_id() {
        let response = daemon_initialize_response(7);
        assert!(response.contains("\"id\":7"));
        assert!(response.contains("\"protocolVersion\""));
    }

    #[test]
    fn default_user_data_dir_for_profile_follows_prefix() {
        assert_eq!(
            default_user_data_dir_for_profile("conao3"),
            "~/.config/chrome-devtools/profiles/conao3"
        );
    }

    #[test]
    fn find_profile_returns_named_profile_or_error() {
        let config = dummy_config();
        assert_eq!(find_profile(&config, "default").unwrap().name, "default");
        assert!(find_profile(&config, "missing").is_err());
    }

    #[test]
    fn unix_secs_returns_zero_for_unix_epoch() {
        assert_eq!(unix_secs(UNIX_EPOCH), 0);
    }

    #[test]
    fn extract_remote_debugging_port_reads_port_number() {
        let cmd = "/path/chrome --foo --remote-debugging-port=39277 --user-data-dir=/x";
        assert_eq!(extract_remote_debugging_port(cmd), Some(39277));
    }

    #[test]
    fn extract_remote_debugging_port_returns_none_when_missing() {
        assert_eq!(extract_remote_debugging_port("/path/chrome --foo"), None);
    }

    #[test]
    fn extract_remote_debugging_port_stops_at_next_flag() {
        let cmd = "chrome --remote-debugging-port=8080 --type=renderer";
        assert_eq!(extract_remote_debugging_port(cmd), Some(8080));
    }

    #[test]
    fn extract_remote_debugging_port_returns_none_for_invalid_value() {
        let cmd = "chrome --remote-debugging-port=abc --user-data-dir=/x";
        assert_eq!(extract_remote_debugging_port(cmd), None);
    }

    #[test]
    fn sanitize_outgoing_request_strips_isolated_context_from_new_page() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"new_page","arguments":{"url":"https://example.com","isolatedContext":"foo"}}}"#;
        let output = sanitize_outgoing_request(input);
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(value["params"]["arguments"]
            .get("isolatedContext")
            .is_none());
        assert_eq!(value["params"]["arguments"]["url"], "https://example.com");
    }

    #[test]
    fn sanitize_outgoing_request_keeps_other_tools_untouched() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"navigate_page","arguments":{"isolatedContext":"foo"}}}"#;
        let output = sanitize_outgoing_request(input);
        assert_eq!(output, input);
    }

    #[test]
    fn sanitize_outgoing_request_passes_through_non_tools_call() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let output = sanitize_outgoing_request(input);
        assert_eq!(output, input);
    }

    #[test]
    fn sanitize_outgoing_request_passes_through_invalid_json() {
        let input = "not json";
        assert_eq!(sanitize_outgoing_request(input), input);
    }
}
