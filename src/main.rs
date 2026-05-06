use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
struct Profile {
    name: &'static str,
    port: u16,
    user_data_dir: &'static str,
}

const PROFILES: &[Profile] = &[
    Profile {
        name: "note",
        port: 9222,
        user_data_dir: "~/.config/google-chrome-note",
    },
    Profile {
        name: "sana-twitter",
        port: 9223,
        user_data_dir: "~/.config/google-chrome-sana-twitter",
    },
];

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

    let Some(action) = args.next() else {
        print_usage();
        return Err(format!("missing action for object: {object}"));
    };

    let rest = args.collect::<Vec<_>>();

    match (object.as_str(), action.as_str()) {
        ("mcp", "call") => {
            let profile = require_profile(&rest)?;
            ensure_chrome(profile)?;
            exec_mcp(profile)
        }
        ("mcp", "list") => {
            let profile = require_profile(&rest)?;
            ensure_chrome(profile)?;
            list_mcp_tools(profile)
        }
        ("mcp", "help") => {
            reject_extra_args(&rest)?;
            print_mcp_help();
            Ok(())
        }
        ("profile", "status") => {
            let profile = require_profile(&rest)?;
            print_status(profile);
            Ok(())
        }
        ("profile", "stop") => {
            let profile = require_profile(&rest)?;
            stop_profile(profile)
        }
        ("profile", "list") => {
            reject_extra_args(&rest)?;
            list_profiles();
            Ok(())
        }
        (_, "help" | "--help" | "-h") => {
            print_usage();
            Ok(())
        }
        _ => Err(format!("unknown command: {object} {action}")),
    }
}

fn reject_extra_args(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("unknown argument: {}", args[0]))
    }
}

fn require_profile(args: &[String]) -> Result<Profile, String> {
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

    PROFILES
        .iter()
        .find(|profile| profile.name == profile_name)
        .copied()
        .ok_or_else(|| format!("unknown profile: {profile_name}"))
}

fn list_profiles() {
    for profile in PROFILES {
        println!(
            "{}\tport={}\tuser_data_dir={}",
            profile.name, profile.port, profile.user_data_dir
        );
    }
}

fn print_status(profile: Profile) {
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

fn ensure_chrome(profile: Profile) -> Result<(), String> {
    if is_devtools_ready(profile.port) {
        return Ok(());
    }

    let chrome = env::var("CHROME").unwrap_or_else(|_| "google-chrome-stable".to_string());
    let user_data_dir = expand_home(profile.user_data_dir)?;

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

fn exec_mcp(profile: Profile) -> Result<(), String> {
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

fn list_mcp_tools(profile: Profile) -> Result<(), String> {
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

fn mcp_command(profile: Profile) -> Command {
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

fn stop_profile(profile: Profile) -> Result<(), String> {
    let user_data_dir = expand_home(profile.user_data_dir)?;
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
        "Usage:\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile>\n  chrome-devtools mcp help\n  chrome-devtools profile status --profile <profile>\n  chrome-devtools profile stop --profile <profile>\n  chrome-devtools profile list"
    );
}

fn print_mcp_help() {
    println!(
        "chrome-devtools mcp\n\nUsage:\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile>\n  chrome-devtools mcp help\n\nCommands:\n  list    Start or reuse Chrome for the selected profile, query tools/list, and print the raw MCP JSON response.\n\n  call    Start or reuse Chrome for the selected profile, then run chrome-devtools-mcp over stdio.\n          MCP JSON-RPC input is read from stdin and output is written to stdout.\n\n  help    Show this help.\n\nExamples:\n  chrome-devtools mcp list --profile sana-twitter\n  chrome-devtools mcp call --profile sana-twitter\n  printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"probe\",\"version\":\"0.0.0\"}}}}}}' | chrome-devtools mcp call --profile sana-twitter\n\nNotes:\n  Profiles define the Chrome user data directory and DevTools port.\n  The call command does not reimplement MCP tools; it delegates to chrome-devtools-mcp."
    );
}
