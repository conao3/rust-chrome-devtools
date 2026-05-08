use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
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

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = load_or_create_config()?;
    let mut args = env::args().skip(1);
    let Some(object) = args.next() else {
        print_usage();
        return Err("missing object".to_string());
    };

    if matches!(object.as_str(), "help" | "--help" | "-h") {
        print_usage();
        return Ok(());
    }

    let Some(action) = args.next() else {
        print_usage();
        return Err(format!("missing action for object: {object}"));
    };

    let rest = args.collect::<Vec<_>>();

    match (object.as_str(), action.as_str()) {
        ("mcp", "call") => {
            let profile = require_profile(&config, &rest)?;
            let _lock = acquire_profile_lock(&profile)?;
            ensure_chrome(&profile)?;
            exec_mcp(&profile)
        }
        ("mcp", "list") => {
            let profile = require_profile(&config, &rest)?;
            let _lock = acquire_profile_lock(&profile)?;
            ensure_chrome(&profile)?;
            list_mcp_tools(&profile)
        }
        ("mcp", "help") => {
            reject_extra_args(&rest)?;
            print_mcp_help();
            Ok(())
        }
        ("profile", "status") => {
            let profile = require_profile(&config, &rest)?;
            print_status(&profile);
            Ok(())
        }
        ("profile", "stop") => {
            let profile = require_profile(&config, &rest)?;
            stop_profile(&profile)
        }
        ("profile", "list") => {
            reject_extra_args(&rest)?;
            list_profiles(&config);
            Ok(())
        }
        (_, "help" | "--help" | "-h") => {
            print_usage();
            Ok(())
        }
        _ => Err(format!("unknown command: {object} {action}")),
    }
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
    let mut command = Command::new("npx");
    command
        .arg("-y")
        .arg("chrome-devtools-mcp@latest")
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
    let target = format!(r#""id":{target_id}"#);

    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read MCP response: {error}"))?;
        if bytes == 0 {
            return Err("chrome-devtools-mcp closed stdout before responding".to_string());
        }

        let line = line.trim_end().to_string();
        if line.contains(r#""method":"roots/list""#) {
            if let Some(id) = extract_jsonrpc_id(&line) {
                let response = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"roots":[]}}}}"#);
                write_json_line(stdin, &response)?;
            }
            continue;
        }

        if line.contains(&target) {
            return Ok(line);
        }
    }
}

fn extract_jsonrpc_id(line: &str) -> Option<u64> {
    let marker = r#""id":"#;
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find(|character: char| !character.is_ascii_digit())?;
    rest[..end].parse().ok()
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
        "Usage:\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile>\n  chrome-devtools mcp help\n  chrome-devtools profile status --profile <profile>\n  chrome-devtools profile stop --profile <profile>\n  chrome-devtools profile list\n\nConfig:\n  ~/.config/chrome-devtools/config.toml is created on startup if missing.\n\nConcurrency:\n  MCP commands take a per-profile lock under ~/.cache/chrome-devtools/locks.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the default 300 second wait."
    );
}

fn print_mcp_help() {
    println!(
        "chrome-devtools mcp\n\nUsage:\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile>\n  chrome-devtools mcp help\n\nCommands:\n  list    Start or reuse Chrome for the selected profile, query tools/list, and print the raw MCP JSON response.\n\n  call    Start or reuse Chrome for the selected profile, then run chrome-devtools-mcp over stdio.\n          MCP JSON-RPC input is read from stdin and output is written to stdout.\n\n  help    Show this help.\n\nExamples:\n  chrome-devtools mcp list --profile default\n  chrome-devtools mcp call --profile default\n  printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"probe\",\"version\":\"0.0.0\"}}}}}}' | chrome-devtools mcp call --profile default\n\nConfig:\n  Profiles are read from ~/.config/chrome-devtools/config.toml.\n  If the config file is missing, chrome-devtools creates a default profile using ~/.config/chrome-devtools/profiles/default.\n  user_data_dir is optional; when omitted, it defaults to ~/.config/chrome-devtools/profiles/<profile-name>.\n  Prefer user_data_dir values under ~/.config/chrome-devtools/profiles/<profile-name>.\n\nConcurrency:\n  MCP commands take a per-profile lock under ~/.cache/chrome-devtools/locks.\n  This keeps take_snapshot and later uid-based click/fill calls in one non-interleaved MCP process.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the default 300 second wait.\n\nNotes:\n  Profiles define the Chrome user data directory and DevTools port.\n  The call command does not reimplement MCP tools; it delegates to chrome-devtools-mcp.\n  Snapshot uid values are only valid inside the MCP process that produced them.\n  Do not split take_snapshot and later click/fill calls across separate mcp call invocations."
    );
}
