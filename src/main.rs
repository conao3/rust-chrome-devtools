use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
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
        ("mcp", "help") => {
            reject_extra_args(&rest)?;
            print_usage();
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
    let status = Command::new("npx")
        .arg("-y")
        .arg("chrome-devtools-mcp@latest")
        .arg("--browser-url")
        .arg(format!("http://127.0.0.1:{}", profile.port))
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

fn stop_profile(profile: Profile) -> Result<(), String> {
    let user_data_dir = expand_home(profile.user_data_dir)?;
    let pattern = format!("--user-data-dir={}", user_data_dir.display());

    let status = Command::new("pkill")
        .arg("-f")
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
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };

    let request = b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if stream.write_all(request).is_err() {
        return false;
    }

    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok() && response.contains("200 OK")
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
        "Usage:\n  chrome-devtools mcp call --profile <profile>\n  chrome-devtools mcp help\n  chrome-devtools profile status --profile <profile>\n  chrome-devtools profile stop --profile <profile>\n  chrome-devtools profile list"
    );
}
