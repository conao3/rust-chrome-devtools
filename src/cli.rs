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

pub(crate) fn print_usage() {
    eprintln!(
        "Usage:\n  chrome-devtools --version\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile> --session <id>\n  chrome-devtools mcp batch --profile <profile> --session <id> --script <path>\n  chrome-devtools mcp direct-list --profile <profile>\n  chrome-devtools mcp direct-call --profile <profile>\n  chrome-devtools mcp help\n  chrome-devtools session create --profile <profile>\n  chrome-devtools session list --profile <profile>\n  chrome-devtools session close --profile <profile> --session <id>\n  chrome-devtools daemon start --profile <profile>\n  chrome-devtools daemon status --profile <profile>\n  chrome-devtools daemon stop --profile <profile>\n  chrome-devtools profile status --profile <profile>\n  chrome-devtools profile stop --profile <profile>\n  chrome-devtools profile list\n\nConfig:\n  ~/.config/chrome-devtools/config.toml is created on startup if missing.\n\nConcurrency:\n  The daemon assigns each session to a page and injects pageId for page-scoped\n  MCP tools. Different sessions may run concurrently; a second bind on the same\n  session id is rejected. Direct MCP commands take a per-profile lock under\n  ~/.cache/chrome-devtools/locks."
    );
}

pub(crate) fn print_version() {
    println!("chrome-devtools {}", env!("CARGO_PKG_VERSION"));
}

pub(crate) fn print_mcp_help() {
    println!(
        "chrome-devtools mcp\n\nUsage:\n  chrome-devtools mcp list --profile <profile>\n  chrome-devtools mcp call --profile <profile> --session <id>\n  chrome-devtools mcp batch --profile <profile> --session <id> --script <path>\n  chrome-devtools mcp direct-list --profile <profile>\n  chrome-devtools mcp direct-call --profile <profile>\n  chrome-devtools mcp help\n\nCommands:\n  list         Start the selected profile daemon if needed, query tools/list through it, and print the raw MCP JSON response.\n\n  call         Start the selected profile daemon if needed, bind the named session, and forward stdin MCP JSON-RPC lines through its long-lived MCP process.\n\n  batch        Bind the named session and run a JSON batch file of tool/sleep steps through the profile daemon. Prints a JSON array of results.\n\n  direct-list  Bypass the daemon, run chrome-devtools-mcp directly, query tools/list, and print the raw MCP JSON response.\n\n  direct-call  Bypass the daemon, run chrome-devtools-mcp directly over stdio. Use only for fallback/manual debugging.\n\n  help         Show this help.\n\nOptions:\n  -h, --help   Show this help and exit.\n\nExamples:\n  chrome-devtools daemon start --profile default\n  chrome-devtools mcp list --profile default\n  ID=$(chrome-devtools session create --profile default | awk -F= '{{print $2}}' | awk '{{print $1}}')\n  chrome-devtools mcp call --profile default --session \"$ID\"\n  chrome-devtools mcp batch --profile default --session \"$ID\" --script /tmp/batch.json\n\nConfig:\n  Profiles are read from ~/.config/chrome-devtools/config.toml.\n  If the config file is missing, chrome-devtools creates a default profile using ~/.config/chrome-devtools/profiles/default.\n  user_data_dir is optional; when omitted, it defaults to ~/.config/chrome-devtools/profiles/<profile-name>.\n  Prefer user_data_dir values under ~/.config/chrome-devtools/profiles/<profile-name>.\n\nSessions:\n  mcp call and mcp batch require --session <id>. Mint one with `session create`.\n  Sessions live in-memory on the daemon and expire after 30 minutes of inactivity.\n  Each session gets its own page target. Different sessions may bind at the same time.\n\nDaemon:\n  mcp call, mcp list and mcp batch route through one long-lived per-profile daemon by default.\n  Daemon sockets and pid files live under ~/.cache/chrome-devtools/daemons.\n  direct-call and direct-list bypass the daemon and take a per-profile lock under ~/.cache/chrome-devtools/locks.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the direct-mode/default daemon lock wait.\n\nNotes:\n  Profiles define the Chrome user data directory. The daemon picks a free DevTools port automatically.\n  The call/batch commands do not reimplement MCP tools; they delegate to a daemon-owned chrome-devtools-mcp process.\n  The daemon injects pageId for page-scoped tools so each session uses its assigned page.\n  Snapshot uid values are valid for the selected session page inside the daemon-owned MCP process.\n  Daemon-routed calls preserve that MCP process across invocations until the daemon stops."
    );
}

pub(crate) fn print_profile_help() {
    println!(
        "chrome-devtools profile\n\nUsage:\n  chrome-devtools profile list\n  chrome-devtools profile status --profile <profile>\n  chrome-devtools profile stop --profile <profile>\n\nCommands:\n  list    List all profiles defined in the config file.\n  status  Show whether the Chrome instance bound to the given profile is running.\n  stop    Stop the Chrome instance bound to the given profile.\n\nOptions:\n  -h, --help  Show this help and exit."
    );
}

pub(crate) fn print_daemon_help() {
    println!(
        "chrome-devtools daemon\n\nUsage:\n  chrome-devtools daemon start --profile <profile>\n  chrome-devtools daemon status --profile <profile>\n  chrome-devtools daemon stop --profile <profile>\n\nCommands:\n  start   Start a background daemon for the profile, or report that one is already ready.\n  status  Show whether the per-profile daemon is ready, along with its pid and socket path.\n  stop    Ask the per-profile daemon to stop and clean up its socket/pid files.\n\nOptions:\n  -h, --help  Show this help and exit.\n\nNotes:\n  Daemon metadata lives under ~/.cache/chrome-devtools/daemons."
    );
}

pub(crate) fn print_mcp_call_help() {
    println!(
        "chrome-devtools mcp call\n\nUsage:\n  chrome-devtools mcp call --profile <profile> --session <id>\n\nDescription:\n  Start the selected profile daemon if needed, bind the named session, then\n  forward stdin MCP JSON-RPC lines through its long-lived chrome-devtools-mcp\n  process and print responses. The daemon assigns the session to a page and\n  injects pageId for page-scoped tools. Activity refreshes the session's\n  30 minute idle timer.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --session <id>    Required. Session id minted by `session create`.\n  -h, --help        Show this help and exit.\n\nExamples:\n  ID=$(chrome-devtools session create --profile default | awk -F= '{{print $2}}' | awk '{{print $1}}')\n  printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{{}}}}' \\\n    | chrome-devtools mcp call --profile default --session \"$ID\""
    );
}

pub(crate) fn print_mcp_batch_help() {
    println!(
        "chrome-devtools mcp batch\n\nUsage:\n  chrome-devtools mcp batch --profile <profile> --session <id> --script <path> [--output <path>] [--fail-fast]\n\nDescription:\n  Read a JSON array of steps from --script, bind the named session, and\n  execute each step in order through the profile daemon (one initialize\n  handshake, then a tools/call per tool step). Prints a JSON array of\n  results to stdout. Activity refreshes the session's 30 minute idle timer.\n\nStep shapes:\n  {{\"type\":\"tool\",\"name\":\"<mcp-tool>\",\"args\":{{...}},\"label\":\"<optional>\",\"on_error\":\"continue|stop\"}}\n  {{\"type\":\"sleep_ms\",\"ms\":<u64>,\"label\":\"<optional>\"}}\n\nValue references inside args:\n  Replace any value in args with {{\"$ref\":\"<label>.<path>\"}} to substitute it\n  with a previous result. <path> is dot-separated; numeric segments index\n  arrays. Example: {{\"$ref\":\"snap.result.content.0.text\"}} resolves to the\n  text of the first content entry returned by the step labelled 'snap'.\n\nResult shape (per step):\n  {{\"type\":\"tool\",\"name\":\"...\",\"label\":\"...\",\"result\":<mcp tools/call result>,\"error\":<mcp error or null>}}\n  {{\"type\":\"sleep_ms\",\"ms\":<u64>,\"label\":\"...\"}}\n\nError handling:\n  A tool step is considered to have errored if the MCP response carries a\n  non-null 'error' field or the result has isError=true. By default the\n  batch continues; pass --fail-fast or set on_error=stop on a step to\n  stop execution after that error. When stopped, batch writes the partial\n  results to stdout/--output and exits non-zero.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --session <id>    Required. Session id minted by `session create`.\n  --script <path>   Required. Path to a JSON file with the step array, or `-` for stdin.\n  --output <path>   Optional. Write the JSON results to <path> instead of stdout.\n  --fail-fast       Optional. Stop on the first errored tool step.\n  -h, --help        Show this help and exit.\n\nExamples:\n  cat > /tmp/batch.json <<'EOF'\n  [\n    {{\"type\":\"tool\",\"name\":\"navigate_page\",\"args\":{{\"type\":\"reload\",\"timeout\":15000}}}},\n    {{\"type\":\"sleep_ms\",\"ms\":5000}},\n    {{\"type\":\"tool\",\"name\":\"evaluate_script\",\"label\":\"title\",\"args\":{{\"function\":\"() => document.title\"}}}}\n  ]\n  EOF\n  ID=$(chrome-devtools session create --profile default | awk -F= '{{print $2}}' | awk '{{print $1}}')\n  chrome-devtools mcp batch --profile default --session \"$ID\" --script /tmp/batch.json"
    );
}

pub(crate) fn print_mcp_list_help() {
    println!(
        "chrome-devtools mcp list\n\nUsage:\n  chrome-devtools mcp list --profile <profile>\n\nDescription:\n  Start the selected profile daemon if needed, query tools/list through it,\n  and print the raw MCP JSON response.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_mcp_direct_call_help() {
    println!(
        "chrome-devtools mcp direct-call\n\nUsage:\n  chrome-devtools mcp direct-call --profile <profile>\n\nDescription:\n  Bypass the daemon and run chrome-devtools-mcp directly over stdio.\n  Use only for fallback/manual debugging; this mode cannot preserve snapshot\n  state across independent process invocations.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit.\n\nNotes:\n  Acquires a per-profile lock under ~/.cache/chrome-devtools/locks.\n  Set CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS to override the 300 second wait."
    );
}

pub(crate) fn print_mcp_direct_list_help() {
    println!(
        "chrome-devtools mcp direct-list\n\nUsage:\n  chrome-devtools mcp direct-list --profile <profile>\n\nDescription:\n  Bypass the daemon, run chrome-devtools-mcp directly, query tools/list, and\n  print the raw MCP JSON response.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit.\n\nNotes:\n  Acquires a per-profile lock under ~/.cache/chrome-devtools/locks."
    );
}

pub(crate) fn print_profile_status_help() {
    println!(
        "chrome-devtools profile status\n\nUsage:\n  chrome-devtools profile status --profile <profile>\n\nDescription:\n  Show whether the Chrome DevTools endpoint for the given profile is reachable,\n  along with its runtime port and user_data_dir.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_profile_stop_help() {
    println!(
        "chrome-devtools profile stop\n\nUsage:\n  chrome-devtools profile stop --profile <profile> [--force]\n\nDescription:\n  Stop the Chrome instance bound to the given profile by matching processes\n  whose command line contains --user-data-dir=<profile user_data_dir>.\n  Refused while the profile daemon is running, because other agents may be\n  driving that Chrome through it; stop the daemon first or pass --force.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --force           Stop Chrome even while the profile daemon is running.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_profile_list_help() {
    println!(
        "chrome-devtools profile list\n\nUsage:\n  chrome-devtools profile list\n\nDescription:\n  Print all profiles defined in ~/.config/chrome-devtools/config.toml, one per\n  line, as: <name>\\tuser_data_dir=<path>.\n\nOptions:\n  -h, --help  Show this help and exit."
    );
}

pub(crate) fn print_daemon_start_help() {
    println!(
        "chrome-devtools daemon start\n\nUsage:\n  chrome-devtools daemon start --profile <profile>\n\nDescription:\n  Start a background daemon for the profile if one is not already running.\n  The daemon owns one chrome-devtools-mcp process and serializes MCP calls\n  over a Unix socket under ~/.cache/chrome-devtools/daemons.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_daemon_run_help() {
    println!(
        "chrome-devtools daemon run\n\nUsage:\n  chrome-devtools daemon run --profile <profile>\n\nDescription:\n  Run the per-profile broker in the foreground. This subcommand is normally\n  spawned by `daemon start` and is not intended to be invoked directly.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_daemon_status_help() {
    println!(
        "chrome-devtools daemon status\n\nUsage:\n  chrome-devtools daemon status --profile <profile>\n\nDescription:\n  Print whether the per-profile daemon is ready or stopped. Ready output:\n\n    profile=<p> daemon=ready version=<v> sessions=<n> chrome=<state> pid=<pid> socket=<path>\n\n  chrome=ready means the DevTools endpoint the daemon's MCP is attached to\n  responds; chrome=unreachable means every tool call will fail until the\n  daemon is restarted.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_daemon_stop_help() {
    println!(
        "chrome-devtools daemon stop\n\nUsage:\n  chrome-devtools daemon stop --profile <profile> [--force]\n\nDescription:\n  Ask the per-profile daemon to stop and clean up its socket and pid files.\n  Refused while sessions are active, because other agents may own them;\n  pass --force to stop anyway (their sessions are destroyed).\n  If the daemon is unreachable but a pid file exists, fall back to sending it\n  a TERM signal via kill.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --force           Stop even while sessions are active.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_session_help() {
    println!(
        "chrome-devtools session\n\nUsage:\n  chrome-devtools session create --profile <profile>\n  chrome-devtools session list --profile <profile>\n  chrome-devtools session close --profile <profile> --session <id>\n\nCommands:\n  create  Mint a new session id on the profile daemon.\n  list    List active sessions held by the profile daemon.\n  close   Close (drop) the named session.\n\nOptions:\n  -h, --help  Show this help and exit.\n\nNotes:\n  Sessions live in-memory on the profile daemon. They are dropped after\n  30 minutes of inactivity or when the daemon stops.\n  mcp call and mcp batch require --session <id>; use session create to mint it."
    );
}

pub(crate) fn print_session_create_help() {
    println!(
        "chrome-devtools session create\n\nUsage:\n  chrome-devtools session create --profile <profile>\n\nDescription:\n  Start the profile daemon if needed, then ask it to mint a new in-memory\n  session id. The session is dropped after 30 minutes of inactivity or when\n  the daemon stops. Prints one line to stdout:\n\n    session=<id> created=<unix-ts> last_used=<unix-ts> owned=false\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_session_list_help() {
    println!(
        "chrome-devtools session list\n\nUsage:\n  chrome-devtools session list --profile <profile>\n\nDescription:\n  List active sessions held by the profile daemon. Each session is printed\n  on one line as:\n\n    session=<id> created=<unix-ts> last_used=<unix-ts> owned=<true|false> page=<id>\n\n  Prints nothing when the daemon is not running.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  -h, --help        Show this help and exit."
    );
}

pub(crate) fn print_session_close_help() {
    println!(
        "chrome-devtools session close\n\nUsage:\n  chrome-devtools session close --profile <profile> --session <id>\n\nDescription:\n  Ask the profile daemon to drop the named session. Fails if the session is\n  unknown or the daemon is not running.\n\nOptions:\n  --profile <name>  Required. Profile name from ~/.config/chrome-devtools/config.toml.\n  --session <id>    Required. Session id minted by `session create`.\n  -h, --help        Show this help and exit."
    );
}
