use crate::batch::*;
use crate::config::*;
use crate::daemon::*;
use crate::lock::*;
use std::env;

pub(crate) fn run() -> Result<(), String> {
    let (positional, rest) = split_command_args(env::args().skip(1).collect());
    let Some(object) = positional.first() else {
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
    let Some(action) = positional.get(1) else {
        print_usage();
        return Err(format!("missing action for object: {object}"));
    };

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
            let (force, rest) = extract_flag(&rest, "--force");
            let profile = require_profile(&config, &rest)?;
            stop_profile(&profile, force)
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
            let (force, rest) = extract_flag(&rest, "--force");
            let profile = require_profile(&config, &rest)?;
            stop_daemon(&profile, force)
        }
        _ => Err(format!("unknown command: {object} {action}")),
    }
}

pub(crate) fn extract_flag(args: &[String], flag: &str) -> (bool, Vec<String>) {
    let found = args.iter().any(|arg| arg == flag);
    let remaining = args.iter().filter(|arg| *arg != flag).cloned().collect();
    (found, remaining)
}

pub(crate) fn split_command_args(args: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut positional = Vec::new();
    let mut rest = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if positional.len() >= 2 {
            rest.push(arg);
            continue;
        }
        if matches!(arg.as_str(), "--profile" | "--session") {
            rest.push(arg);
            if let Some(value) = iter.next() {
                rest.push(value);
            }
            continue;
        }
        positional.push(arg);
    }
    (positional, rest)
}

pub(crate) fn wants_help(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
}

pub(crate) fn reject_extra_args(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("unknown argument: {}", args[0]))
    }
}

pub(crate) fn require_profile(config: &Config, args: &[String]) -> Result<Profile, String> {
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

    find_profile(config, profile_name)
}

pub(crate) fn require_profile_and_session(
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
    let profile = find_profile(config, &profile_name)?;
    Ok((profile, session_id))
}

pub(crate) fn list_profiles(config: &Config) {
    for profile in &config.profiles {
        println!("{}\tuser_data_dir={}", profile.name, profile.user_data_dir);
    }
}

#[derive(Default)]
struct Help {
    lines: Vec<String>,
}

impl Help {
    fn titled(title: &str) -> Self {
        Self::default().line(title).blank()
    }

    fn line(mut self, line: &str) -> Self {
        self.lines.push(line.to_string());
        self
    }

    fn blank(mut self) -> Self {
        self.lines.push(String::new());
        self
    }

    fn section(mut self, title: &str, lines: &[&str]) -> Self {
        if !self.lines.is_empty() && self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(String::new());
        }
        self.lines.push(title.to_string());
        for line in lines {
            if line.is_empty() {
                self.lines.push(String::new());
            } else {
                self.lines.push(format!("  {line}"));
            }
        }
        self
    }

    fn section_raw(mut self, title: &str, lines: &[&str]) -> Self {
        if !self.lines.is_empty() && self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(String::new());
        }
        self.lines.push(title.to_string());
        for line in lines {
            self.lines.push((*line).to_string());
        }
        self
    }

    fn print_stdout(self) {
        println!("{}", self.lines.join("\n"));
    }

    fn print_stderr(self) {
        eprintln!("{}", self.lines.join("\n"));
    }
}

pub(crate) fn print_usage() {
    Help::default()
        .section(
            "Usage:",
            &[
                "chrome-devtools --version",
                "chrome-devtools mcp list --profile <profile>",
                "chrome-devtools mcp call --profile <profile> --session <id>",
                "chrome-devtools mcp batch --profile <profile> --session <id> --script <path>",
                "chrome-devtools mcp direct-list --profile <profile>",
                "chrome-devtools mcp direct-call --profile <profile>",
                "chrome-devtools mcp help",
                "chrome-devtools session create --profile <profile>",
                "chrome-devtools session list --profile <profile>",
                "chrome-devtools session close --profile <profile> --session <id>",
                "chrome-devtools daemon start --profile <profile>",
                "chrome-devtools daemon status --profile <profile>",
                "chrome-devtools daemon stop --profile <profile>",
                "chrome-devtools profile status --profile <profile>",
                "chrome-devtools profile stop --profile <profile>",
                "chrome-devtools profile list",
            ],
        )
        .section(
            "Config:",
            &["~/.config/chrome-devtools/config.toml is created on startup if missing."],
        )
        .section(
            "Concurrency:",
            &[
                "The daemon assigns each session to a page and injects pageId for page-scoped",
                "MCP tools. Different sessions may run concurrently; a second bind on the same",
                "session id is rejected. Direct MCP commands take a per-profile lock under",
                "~/.cache/chrome-devtools/locks.",
            ],
        )
        .print_stderr();
}

pub(crate) fn print_version() {
    println!("chrome-devtools {}", env!("CARGO_PKG_VERSION"));
}

pub(crate) fn print_mcp_help() {
    Help::titled("chrome-devtools mcp")
        .section(
            "Usage:",
            &[
                "chrome-devtools mcp list --profile <profile>",
                "chrome-devtools mcp call --profile <profile> --session <id>",
                "chrome-devtools mcp batch --profile <profile> --session <id> --script <path>",
                "chrome-devtools mcp direct-list --profile <profile>",
                "chrome-devtools mcp direct-call --profile <profile>",
                "chrome-devtools mcp help",
            ],
        )
        .section(
            "Commands:",
            &[
                "list         Start the selected profile daemon if needed, query tools/list through it, and print the raw MCP JSON response.",
                "",
                "call         Start the selected profile daemon if needed, bind the named session, and forward stdin MCP JSON-RPC lines through its long-lived MCP process.",
                "",
                "batch        Bind the named session and run a JSON batch file of tool/sleep steps through the profile daemon. Prints a JSON array of results.",
                "",
                "direct-list  Bypass the daemon, run chrome-devtools-mcp directly, query tools/list, and print the raw MCP JSON response.",
                "",
                "direct-call  Bypass the daemon, run chrome-devtools-mcp directly over stdio. Use only for fallback/manual debugging.",
                "",
                "help         Show this help.",
            ],
        )
        .section("Options:", &["-h, --help   Show this help and exit."])
        .section(
            "Examples:",
            &[
                "chrome-devtools daemon start --profile default",
                "chrome-devtools mcp list --profile default",
                "ID=$(chrome-devtools session create --profile default | awk -F= '{print $2}' | awk '{print $1}')",
                "chrome-devtools mcp call --profile default --session \"$ID\"",
                "chrome-devtools mcp batch --profile default --session \"$ID\" --script /tmp/batch.json",
            ],
        )
        .section(
            "Config:",
            &[
                "Profiles are read from ~/.config/chrome-devtools/config.toml.",
                "If the config file is missing, chrome-devtools creates a default profile using ~/.config/chrome-devtools/profiles/default.",
                "user_data_dir is optional; when omitted, it defaults to ~/.config/chrome-devtools/profiles/<profile-name>.",
                "Prefer user_data_dir values under ~/.config/chrome-devtools/profiles/<profile-name>.",
            ],
        )
        .section(
            "Sessions:",
            &[
                "mcp call and mcp batch require --session <id>. Mint one with `session create`.",
                "Sessions live in-memory on the daemon and expire after 30 minutes of inactivity.",
                "Each session gets its own page target. Different sessions may bind at the same time.",
            ],
        )
        .section(
            "Daemon:",
            &[
                "mcp call, mcp list and mcp batch route through one long-lived per-profile daemon by default.",
                "Daemon sockets and pid files live under ~/.cache/chrome-devtools/daemons.",
                "direct-call and direct-list bypass the daemon and take a per-profile lock under ~/.cache/chrome-devtools/locks.",
                "Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the direct-mode/default daemon lock wait.",
            ],
        )
        .section(
            "Notes:",
            &[
                "Profiles define the Chrome user data directory. The daemon picks a free DevTools port automatically.",
                "The call/batch commands do not reimplement MCP tools; they delegate to a daemon-owned chrome-devtools-mcp process.",
                "The daemon injects pageId for page-scoped tools so each session uses its assigned page.",
                "Snapshot uid values are returned as session uid tokens and are valid for the latest snapshot on that session page.",
                "Daemon-routed calls preserve that MCP process across invocations until the daemon stops.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_profile_help() {
    Help::titled("chrome-devtools profile")
        .section(
            "Usage:",
            &[
                "chrome-devtools profile list",
                "chrome-devtools profile status --profile <profile>",
                "chrome-devtools profile stop --profile <profile>",
            ],
        )
        .section(
            "Commands:",
            &[
                "list    List all profiles defined in the config file.",
                "status  Show whether the Chrome instance bound to the given profile is running.",
                "stop    Stop the Chrome instance bound to the given profile.",
            ],
        )
        .section("Options:", &["-h, --help  Show this help and exit."])
        .print_stdout();
}

pub(crate) fn print_daemon_help() {
    Help::titled("chrome-devtools daemon")
        .section(
            "Usage:",
            &[
                "chrome-devtools daemon start --profile <profile>",
                "chrome-devtools daemon status --profile <profile>",
                "chrome-devtools daemon stop --profile <profile>",
            ],
        )
        .section(
            "Commands:",
            &[
                "start   Start a background daemon for the profile, or report that one is already ready.",
                "status  Show whether the per-profile daemon is ready, along with its pid and socket path.",
                "stop    Ask the per-profile daemon to stop and clean up its socket/pid files.",
            ],
        )
        .section("Options:", &["-h, --help  Show this help and exit."])
        .section(
            "Notes:",
            &["Daemon metadata lives under ~/.cache/chrome-devtools/daemons."],
        )
        .print_stdout();
}

pub(crate) fn print_mcp_call_help() {
    Help::titled("chrome-devtools mcp call")
        .section("Usage:", &["chrome-devtools mcp call --profile <profile> --session <id>"])
        .section(
            "Description:",
            &[
                "Start the selected profile daemon if needed, bind the named session, then",
                "forward stdin MCP JSON-RPC lines through its long-lived chrome-devtools-mcp",
                "process and print responses. The daemon assigns the session to a page and",
                "injects pageId for page-scoped tools. Activity refreshes the session's",
                "30 minute idle timer.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "--session <id>    Required. Session id minted by `session create`.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .section(
            "Examples:",
            &[
                "ID=$(chrome-devtools session create --profile default | awk -F= '{print $2}' | awk '{print $1}')",
                "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}' \\",
                "  | chrome-devtools mcp call --profile default --session \"$ID\"",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_mcp_batch_help() {
    Help::titled("chrome-devtools mcp batch")
        .section(
            "Usage:",
            &["chrome-devtools mcp batch --profile <profile> --session <id> --script <path> [--output <path>] [--fail-fast]"],
        )
        .section(
            "Description:",
            &[
                "Read a JSON array of steps from --script, bind the named session, and",
                "execute each step in order through the profile daemon (one initialize",
                "handshake, then a tools/call per tool step). Prints a JSON array of",
                "results to stdout. Activity refreshes the session's 30 minute idle timer.",
            ],
        )
        .section(
            "Step shapes:",
            &[
                "{\"type\":\"tool\",\"name\":\"<mcp-tool>\",\"args\":{...},\"label\":\"<optional>\",\"on_error\":\"continue|stop\"}",
                "{\"type\":\"sleep_ms\",\"ms\":<u64>,\"label\":\"<optional>\"}",
            ],
        )
        .section(
            "Value references inside args:",
            &[
                "Replace any value in args with {\"$ref\":\"<label>.<path>\"} to substitute it",
                "with a previous result. <path> is dot-separated; numeric segments index",
                "arrays. Example: {\"$ref\":\"snap.result.content.0.text\"} resolves to the",
                "text of the first content entry returned by the step labelled 'snap'.",
            ],
        )
        .section(
            "Result shape (per step):",
            &[
                "{\"type\":\"tool\",\"name\":\"...\",\"label\":\"...\",\"result\":<mcp tools/call result>,\"error\":<mcp error or null>}",
                "{\"type\":\"sleep_ms\",\"ms\":<u64>,\"label\":\"...\"}",
            ],
        )
        .section(
            "Error handling:",
            &[
                "A tool step is considered to have errored if the MCP response carries a",
                "non-null 'error' field or the result has isError=true. By default the",
                "batch continues; pass --fail-fast or set on_error=stop on a step to",
                "stop execution after that error. When stopped, batch writes the partial",
                "results to stdout/--output and exits non-zero.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "--session <id>    Required. Session id minted by `session create`.",
                "--script <path>   Required. Path to a JSON file with the step array, or `-` for stdin.",
                "--output <path>   Optional. Write the JSON results to <path> instead of stdout.",
                "--fail-fast       Optional. Stop on the first errored tool step.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .section_raw(
            "Examples:",
            &[
                "  cat > /tmp/batch.json <<'EOF'",
                "  [",
                "    {\"type\":\"tool\",\"name\":\"navigate_page\",\"args\":{\"type\":\"reload\",\"timeout\":15000}},",
                "    {\"type\":\"sleep_ms\",\"ms\":5000},",
                "    {\"type\":\"tool\",\"name\":\"evaluate_script\",\"label\":\"title\",\"args\":{\"function\":\"() => document.title\"}}",
                "  ]",
                "  EOF",
                "  ID=$(chrome-devtools session create --profile default | awk -F= '{print $2}' | awk '{print $1}')",
                "  chrome-devtools mcp batch --profile default --session \"$ID\" --script /tmp/batch.json",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_mcp_list_help() {
    Help::titled("chrome-devtools mcp list")
        .section("Usage:", &["chrome-devtools mcp list --profile <profile>"])
        .section(
            "Description:",
            &[
                "Start the selected profile daemon if needed, query tools/list through it,",
                "and print the raw MCP JSON response.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_mcp_direct_call_help() {
    Help::titled("chrome-devtools mcp direct-call")
        .section("Usage:", &["chrome-devtools mcp direct-call --profile <profile>"])
        .section(
            "Description:",
            &[
                "Bypass the daemon and run chrome-devtools-mcp directly over stdio.",
                "Use only for fallback/manual debugging; this mode cannot preserve snapshot",
                "state across independent process invocations.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .section(
            "Notes:",
            &[
                "Acquires a per-profile lock under ~/.cache/chrome-devtools/locks.",
                "Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the 300 second wait.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_mcp_direct_list_help() {
    Help::titled("chrome-devtools mcp direct-list")
        .section("Usage:", &["chrome-devtools mcp direct-list --profile <profile>"])
        .section(
            "Description:",
            &[
                "Bypass the daemon, run chrome-devtools-mcp directly, query tools/list, and",
                "print the raw MCP JSON response.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .section("Notes:", &["Acquires a per-profile lock under ~/.cache/chrome-devtools/locks."])
        .print_stdout();
}

pub(crate) fn print_profile_status_help() {
    Help::titled("chrome-devtools profile status")
        .section("Usage:", &["chrome-devtools profile status --profile <profile>"])
        .section(
            "Description:",
            &[
                "Show whether the Chrome DevTools endpoint for the given profile is reachable,",
                "along with its runtime port and user_data_dir.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_profile_stop_help() {
    Help::titled("chrome-devtools profile stop")
        .section("Usage:", &["chrome-devtools profile stop --profile <profile> [--force]"])
        .section(
            "Description:",
            &[
                "Stop the Chrome instance bound to the given profile by matching processes",
                "whose command line contains --user-data-dir=<profile user_data_dir>.",
                "Refused while the profile daemon is running, because other agents may be",
                "driving that Chrome through it; stop the daemon first or pass --force.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "--force           Stop Chrome even while the profile daemon is running.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_profile_list_help() {
    Help::titled("chrome-devtools profile list")
        .section("Usage:", &["chrome-devtools profile list"])
        .section(
            "Description:",
            &[
                "Print all profiles defined in ~/.config/chrome-devtools/config.toml, one per",
                "line, as: <name>\\tuser_data_dir=<path>.",
            ],
        )
        .section("Options:", &["-h, --help  Show this help and exit."])
        .print_stdout();
}

pub(crate) fn print_daemon_start_help() {
    Help::titled("chrome-devtools daemon start")
        .section("Usage:", &["chrome-devtools daemon start --profile <profile>"])
        .section(
            "Description:",
            &[
                "Start a background daemon for the profile if one is not already running.",
                "The daemon owns one chrome-devtools-mcp process and routes MCP calls",
                "over a Unix socket under ~/.cache/chrome-devtools/daemons.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_daemon_run_help() {
    Help::titled("chrome-devtools daemon run")
        .section("Usage:", &["chrome-devtools daemon run --profile <profile>"])
        .section(
            "Description:",
            &[
                "Run the per-profile broker in the foreground. This subcommand is normally",
                "spawned by `daemon start` and is not intended to be invoked directly.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_daemon_status_help() {
    Help::titled("chrome-devtools daemon status")
        .section("Usage:", &["chrome-devtools daemon status --profile <profile>"])
        .section(
            "Description:",
            &[
                "Print whether the per-profile daemon is ready or stopped. Ready output:",
                "",
                "  profile=<p> daemon=ready version=<v> sessions=<n> active_sessions=<n> pages=<ids> queued_mcp_requests=<n> max_control_latency_ms=<n> max_forward_latency_ms=<n> diagnostic_window_secs=<n> chrome=<state> respawns=<n> pid=<pid> socket=<path>",
                "",
                "chrome=ready means the DevTools endpoint currently responds.",
                "respawns=<n> counts daemon-managed MCP respawns; mint a new session after it changes.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_daemon_stop_help() {
    Help::titled("chrome-devtools daemon stop")
        .section("Usage:", &["chrome-devtools daemon stop --profile <profile> [--force]"])
        .section(
            "Description:",
            &[
                "Ask the per-profile daemon to stop and clean up its socket and pid files.",
                "Refused while sessions are active, because other agents may own them;",
                "pass --force to stop anyway (their sessions are destroyed).",
                "If the daemon is unreachable but a pid file exists, fall back to sending it",
                "a TERM signal via kill.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "--force           Stop even while sessions are active.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_session_help() {
    Help::titled("chrome-devtools session")
        .section(
            "Usage:",
            &[
                "chrome-devtools session create --profile <profile>",
                "chrome-devtools session list --profile <profile>",
                "chrome-devtools session close --profile <profile> --session <id>",
            ],
        )
        .section(
            "Commands:",
            &[
                "create  Mint a new session id on the profile daemon.",
                "list    List active sessions held by the profile daemon.",
                "close   Close (drop) the named session and its daemon-created page.",
            ],
        )
        .section("Options:", &["-h, --help  Show this help and exit."])
        .section(
            "Notes:",
            &[
                "Sessions live in-memory on the profile daemon. They are dropped after",
                "30 minutes of inactivity or when the daemon stops.",
                "mcp call and mcp batch require --session <id>; use session create to mint it.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_session_create_help() {
    Help::titled("chrome-devtools session create")
        .section("Usage:", &["chrome-devtools session create --profile <profile>"])
        .section(
            "Description:",
            &[
                "Start the profile daemon if needed, then ask it to mint a new in-memory",
                "session id. The session is dropped after 30 minutes of inactivity or when",
                "the daemon stops. Prints one line to stdout:",
                "",
                "  session=<id> created=<unix-ts> last_used=<unix-ts> owned=false",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_session_list_help() {
    Help::titled("chrome-devtools session list")
        .section("Usage:", &["chrome-devtools session list --profile <profile>"])
        .section(
            "Description:",
            &[
                "List active sessions held by the profile daemon. Each session is printed",
                "on one line as:",
                "",
                "  session=<id> created=<unix-ts> last_used=<unix-ts> owned=<true|false> page=<id>",
                "",
                "Prints nothing when the daemon is not running.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}

pub(crate) fn print_session_close_help() {
    Help::titled("chrome-devtools session close")
        .section("Usage:", &["chrome-devtools session close --profile <profile> --session <id>"])
        .section(
            "Description:",
            &[
                "Ask the profile daemon to drop the named session. If the session owns a",
                "daemon-created page, the daemon closes that page while preserving Chrome's",
                "last tab. Fails if the session is unknown or the daemon is not running.",
            ],
        )
        .section(
            "Options:",
            &[
                "--profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.",
                "--session <id>    Required. Session id minted by `session create`.",
                "-h, --help        Show this help and exit.",
            ],
        )
        .print_stdout();
}
