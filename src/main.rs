use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Config {
    profiles: Vec<Profile>,
}

#[derive(Clone, Debug)]
struct Profile {
    name: String,
    port: u16,
    user_data_dir: String,
}

#[derive(Default)]
struct ProfileBuilder {
    name: Option<String>,
    port: Option<u16>,
    user_data_dir: Option<String>,
}

const CONFIG_RELATIVE_PATH: &str = ".config/chrome-devtools/config.toml";
const CACHE_RELATIVE_PATH: &str = ".cache/chrome-devtools";
const DEFAULT_PROFILE_NAME: &str = "default";
const DEFAULT_PROFILE_PORT: u16 = 9222;
const DEFAULT_PROFILE_USER_DATA_DIR: &str = "~/.config/chrome-devtools/profiles/default";
const PROFILE_USER_DATA_DIR_PREFIX: &str = "~/.config/chrome-devtools/profiles";
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(30);

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
        ("mcp", "call") => {
            if wants_help(&rest) {
                print_mcp_call_help();
                return Ok(());
            }
            let profile = require_profile(&config, &rest)?;
            call_daemon(&profile)
        }
        ("mcp", "exec") => {
            if wants_help(&rest) {
                print_mcp_exec_help();
                return Ok(());
            }
            exec_scenario(&config, &rest)
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
    format!("[[profiles]]\nname = \"{DEFAULT_PROFILE_NAME}\"\nport = {DEFAULT_PROFILE_PORT}\n")
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
                profile.port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|error| format!("line {line_number}: invalid port: {error}"))?,
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
    let port = builder
        .port
        .ok_or_else(|| format!("line {line_number}: profile {name} is missing port"))?;
    let user_data_dir = builder
        .user_data_dir
        .unwrap_or_else(|| default_user_data_dir_for_profile(&name));

    if profiles.iter().any(|profile| profile.name == name) {
        return Err(format!("duplicate profile name: {name}"));
    }

    profiles.push(Profile {
        name,
        port,
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

    for stream in listener.incoming() {
        let mut stream =
            stream.map_err(|error| format!("failed to accept daemon client: {error}"))?;
        let should_stop = handle_daemon_client(&mut stream, &mut mcp_stdin, &mut mcp_reader)?;
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

fn handle_daemon_client(
    stream: &mut UnixStream,
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
) -> Result<bool, String> {
    let mut client_reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|error| format!("failed to clone daemon client stream: {error}"))?,
    );
    let mut line = String::new();

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
        if line == "__chrome_devtools_daemon__:status" {
            stream
                .write_all(b"daemon=ready\n")
                .and_then(|_| stream.flush())
                .map_err(|error| format!("failed to write daemon status response: {error}"))?;
            return Ok(false);
        }
        if line == "__chrome_devtools_daemon__:stop" {
            stream
                .write_all(b"daemon=stopping\n")
                .and_then(|_| stream.flush())
                .map_err(|error| format!("failed to write daemon stop response: {error}"))?;
            return Ok(true);
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

        let Some(pending_id) = extract_jsonrpc_id(line) else {
            write_json_line(mcp_stdin, line)?;
            continue;
        };

        write_json_line(mcp_stdin, line)?;

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
                break;
            }
        }
    }
}

fn call_daemon(profile: &Profile) -> Result<(), String> {
    ensure_daemon(profile)?;
    let socket_path = daemon_socket_path(profile)?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("failed to connect {}: {error}", socket_path.display()))?;
    let mut read_stream = stream
        .try_clone()
        .map_err(|error| format!("failed to clone daemon stream: {error}"))?;

    let stdin_to_daemon = thread::spawn(move || -> Result<(), String> {
        let mut stdin = std::io::stdin().lock();
        std::io::copy(&mut stdin, &mut stream)
            .map_err(|error| format!("failed to send daemon request: {error}"))?;
        stream
            .shutdown(Shutdown::Write)
            .map_err(|error| format!("failed to close daemon request stream: {error}"))?;
        Ok(())
    });

    let mut daemon_reader = BufReader::new(&mut read_stream);
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

struct ExecOptions {
    profile_name: String,
    script_path: String,
    output_path: Option<String>,
    fail_fast: bool,
}

fn exec_scenario(config: &Config, args: &[String]) -> Result<(), String> {
    let options = parse_exec_args(args)?;
    let profile = find_profile(config, &options.profile_name)?;
    let script_content = read_script_source(&options.script_path)?;
    let steps_value: serde_json::Value = serde_json::from_str(&script_content).map_err(|error| {
        format!("failed to parse {}: {error}", options.script_path)
    })?;
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

    write_json_line(
        &mut stream,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"roots":{"listChanged":false}},"clientInfo":{"name":"chrome-devtools-exec","version":"0.1.0"}}}"#,
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
        fs::write(path, &output)
            .map_err(|error| format!("failed to write {path}: {error}"))?;
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

fn parse_exec_args(args: &[String]) -> Result<ExecOptions, String> {
    let mut profile_name = None;
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
    let script_path = script_path.ok_or_else(|| "--script is required".to_string())?;
    Ok(ExecOptions {
        profile_name,
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
    let _ = fs::remove_file(socket_path);
    let _ = fs::remove_file(pid_path);
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

fn list_profiles(config: &Config) {
    for profile in &config.profiles {
        println!(
            "{}\tport={}\tuser_data_dir={}",
            profile.name, profile.port, profile.user_data_dir
        );
    }
}

fn print_status(profile: &Profile) {
    let state = if is_devtools_ready(profile.port) {
        "ready"
    } else {
        "stopped"
    };

    println!(
        "profile={} status={} port={} user_data_dir={}",
        profile.name, state, profile.port, profile.user_data_dir
    );
}

fn ensure_chrome(profile: &Profile) -> Result<(), String> {
    if is_devtools_ready(profile.port) {
        return Ok(());
    }

    let chrome = env::var("CHROME").unwrap_or_else(|_| "google-chrome-stable".to_string());
    let user_data_dir = expand_home(&profile.user_data_dir)?;

    Command::new(chrome)
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--remote-debugging-port={}", profile.port))
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-gpu")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start Chrome: {error}"))?;

    wait_for_devtools(profile.port, Duration::from_secs(15))
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
    command
        .arg("--browser-url")
        .arg(format!("http://127.0.0.1:{}", profile.port))
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
        "Usage:\n  chrome-devtools --version\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile>\n  chrome-devtools mcp direct-list --profile <profile>\n  chrome-devtools mcp direct-call --profile <profile>\n  chrome-devtools mcp help\n  chrome-devtools daemon start --profile <profile>\n  chrome-devtools daemon status --profile <profile>\n  chrome-devtools daemon stop --profile <profile>\n  chrome-devtools profile status --profile <profile>\n  chrome-devtools profile stop --profile <profile>\n  chrome-devtools profile list\n\nConfig:\n  ~/.config/chrome-devtools/config.toml is created on startup if missing.\n\nConcurrency:\n  MCP commands take a per-profile lock under ~/.cache/chrome-devtools/locks.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the default 300 second wait."
    );
}

fn print_version() {
    println!("chrome-devtools {}", env!("CARGO_PKG_VERSION"));
}

fn print_mcp_help() {
    println!(
        "chrome-devtools mcp\n\nUsage:\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile>\n  chrome-devtools mcp exec --profile <profile> --script <path>\n  chrome-devtools mcp direct-list --profile <profile>\n  chrome-devtools mcp direct-call --profile <profile>\n  chrome-devtools mcp help\n\nCommands:\n  list         Start the selected profile daemon if needed, query tools/list through it, and print the raw MCP JSON response.\n\n  call         Start the selected profile daemon if needed, then forward stdin MCP JSON-RPC lines through its long-lived MCP process.\n\n  exec         Run a JSON scenario file of tool/sleep steps through the profile daemon and print a JSON array of results.\n\n  direct-list  Bypass the daemon, run chrome-devtools-mcp directly, query tools/list, and print the raw MCP JSON response.\n\n  direct-call  Bypass the daemon, run chrome-devtools-mcp directly over stdio. Use only for fallback/manual debugging.\n\n  help         Show this help.\n\nOptions:\n  -h, --help   Show this help and exit.\n\nExamples:\n  chrome-devtools daemon start --profile default\n  chrome-devtools mcp list --profile default\n  chrome-devtools mcp call --profile default\n  chrome-devtools mcp exec --profile default --script /tmp/scenario.json\n  printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"probe\",\"version\":\"0.0.0\"}}}}}}' | chrome-devtools mcp call --profile default\n\nConfig:\n  Profiles are read from ~/.config/chrome-devtools/config.toml.\n  If the config file is missing, chrome-devtools creates a default profile using ~/.config/chrome-devtools/profiles/default.\n  user_data_dir is optional; when omitted, it defaults to ~/.config/chrome-devtools/profiles/<profile-name>.\n  Prefer user_data_dir values under ~/.config/chrome-devtools/profiles/<profile-name>.\n\nDaemon:\n  mcp call, mcp list and mcp exec route through one long-lived per-profile daemon by default.\n  Daemon sockets and pid files live under ~/.cache/chrome-devtools/daemons.\n  direct-call and direct-list bypass the daemon and take a per-profile lock under ~/.cache/chrome-devtools/locks.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the direct-mode/default daemon lock wait.\n\nNotes:\n  Profiles define the Chrome user data directory and DevTools port.\n  The call/exec commands do not reimplement MCP tools; they delegate to a daemon-owned chrome-devtools-mcp process.\n  Snapshot uid values are only valid inside the MCP process that produced them.\n  Daemon-routed calls preserve that MCP process across invocations until the daemon stops."
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
        "chrome-devtools mcp call\n\nUsage:\n  chrome-devtools mcp call --profile <profile>\n\nDescription:\n  Start the selected profile daemon if needed, then forward stdin MCP JSON-RPC\n  lines through its long-lived chrome-devtools-mcp process and print responses.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit.\n\nExamples:\n  printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{{}}}}' \\\n    | chrome-devtools mcp call --profile default"
    );
}

fn print_mcp_exec_help() {
    println!(
        "chrome-devtools mcp exec\n\nUsage:\n  chrome-devtools mcp exec --profile <profile> --script <path> [--output <path>] [--fail-fast]\n\nDescription:\n  Read a JSON array of steps from --script, execute each step in order through\n  the profile daemon (one initialize handshake, then a tools/call per tool\n  step), and print a JSON array of results to stdout.\n\nStep shapes:\n  {{\"type\":\"tool\",\"name\":\"<mcp-tool>\",\"args\":{{...}},\"label\":\"<optional>\",\"on_error\":\"continue|stop\"}}\n  {{\"type\":\"sleep_ms\",\"ms\":<u64>,\"label\":\"<optional>\"}}\n\nValue references inside args:\n  Replace any value in args with {{\"$ref\":\"<label>.<path>\"}} to substitute it\n  with a previous result. <path> is dot-separated; numeric segments index\n  arrays. Example: {{\"$ref\":\"snap.result.content.0.text\"}} resolves to the\n  text of the first content entry returned by the step labelled 'snap'.\n\nResult shape (per step):\n  {{\"type\":\"tool\",\"name\":\"...\",\"label\":\"...\",\"result\":<mcp tools/call result>,\"error\":<mcp error or null>}}\n  {{\"type\":\"sleep_ms\",\"ms\":<u64>,\"label\":\"...\"}}\n\nError handling:\n  A tool step is considered to have errored if the MCP response carries a\n  non-null 'error' field or the result has isError=true. By default the\n  scenario continues; pass --fail-fast or set on_error=stop on a step to\n  stop execution after that error. When stopped, exec writes the partial\n  results to stdout/--output and exits non-zero.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --script <path>   Required. Path to a JSON file with the step array, or `-` for stdin.\n  --output <path>   Optional. Write the JSON results to <path> instead of stdout.\n  --fail-fast       Optional. Stop on the first errored tool step.\n  -h, --help        Show this help and exit.\n\nExamples:\n  cat > /tmp/scenario.json <<'EOF'\n  [\n    {{\"type\":\"tool\",\"name\":\"navigate_page\",\"args\":{{\"type\":\"reload\",\"timeout\":15000}}}},\n    {{\"type\":\"sleep_ms\",\"ms\":5000}},\n    {{\"type\":\"tool\",\"name\":\"evaluate_script\",\"label\":\"title\",\"args\":{{\"function\":\"() => document.title\"}}}}\n  ]\n  EOF\n  chrome-devtools mcp exec --profile default --script /tmp/scenario.json\n\n  printf '%s' '[{{\"type\":\"tool\",\"name\":\"list_pages\"}}]' \\\n    | chrome-devtools mcp exec --profile default --script -"
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
        "chrome-devtools profile status\n\nUsage:\n  chrome-devtools profile status --profile <profile>\n\nDescription:\n  Show whether the Chrome DevTools endpoint for the given profile is reachable,\n  along with its port and user_data_dir.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_profile_stop_help() {
    println!(
        "chrome-devtools profile stop\n\nUsage:\n  chrome-devtools profile stop --profile <profile>\n\nDescription:\n  Stop the Chrome instance bound to the given profile by matching processes\n  whose command line contains --user-data-dir=<profile user_data_dir>.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

fn print_profile_list_help() {
    println!(
        "chrome-devtools profile list\n\nUsage:\n  chrome-devtools profile list\n\nDescription:\n  Print all profiles defined in ~/.config/chrome-devtools/config.toml, one per\n  line, as: <name>\\tport=<port>\\tuser_data_dir=<path>.\n\nOptions:\n  -h, --help  Show this help and exit."
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
