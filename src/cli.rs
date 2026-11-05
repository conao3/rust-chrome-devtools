use crate::batch::*;
use crate::config::*;
use crate::daemon::*;
use crate::error::{Result, other, target};
use crate::grammar;
use crate::lock::*;
use crate::output::{self, OutputFormat, Response};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::PathBuf;

const ROOT_AFTER_HELP: &str = "\
Profiles are read from ~/.config/chrome-devtools/config.toml (created with a default profile when missing).
The per-profile daemon assigns each session its own tab and injects pageId for page-scoped MCP tools.
Different sessions may run concurrently; a second bind on the same session id is rejected.

Examples:
  SESSION=$(chrome-devtools session create-session --profile default | jq -r .SessionId)
  chrome-devtools mcp run-batch --profile default --session-id \"$SESSION\" --steps Type=tool,Name=list_pages
  chrome-devtools mcp run-batch --profile default --session-id \"$SESSION\" --steps file:///tmp/steps.json
  chrome-devtools session close-session --profile default --session-id \"$SESSION\"";

const RUN_BATCH_AFTER_HELP: &str = "\
Steps:
  Each --steps value is a JSON array of steps, one JSON step object, or one step in shorthand syntax.
  file://<path> reads the value from a file; file:///dev/stdin reads it from standard input.
    {\"Type\":\"tool\",\"Name\":\"<mcp-tool>\",\"Args\":{...},\"Label\":\"<optional>\",\"OnError\":\"continue|stop\"}
    {\"Type\":\"sleep_ms\",\"Ms\":<u64>,\"Label\":\"<optional>\"}
    Type=tool,Name=evaluate_script,Label=title,Args={\"function\":\"() => document.title\"}
    Type=sleep_ms,Ms=500

Value references inside Args:
  Replace any value in Args with {\"$ref\":\"<label>.<path>\"} to substitute a previous step's result entry.
  <path> is dot-separated and numeric segments index arrays: {\"$ref\":\"snap.Result.content.0.text\"}.

Output:
  {\"Steps\":[{\"Type\":\"tool\",\"Name\":...,\"Label\":...,\"Result\":<MCP tools/call result>,\"Error\":<MCP error or null>}, ...]}
  A step whose page navigated (Execution context was destroyed) has \"Navigated\":true and Error null.
  A tool step errored when Error is non-null or Result.isError is true. By default the batch continues;
  --fail-fast or \"OnError\":\"stop\" stops after that step, prints the partial Steps and exits 254.
  When the batch fails before the first step, Steps is empty and Error holds the message.

Examples:
  chrome-devtools mcp run-batch --profile default --session-id \"$SESSION\" \\
    --steps 'Type=tool,Name=goto,Args={\"url\":\"https://example.com/\"}' \\
            'Type=tool,Name=evaluate_script,Label=title,Args={\"function\":\"() => document.title\"}' \\
    | jq -r '.Steps[] | select(.Label == \"title\") | .Result.content[0].text'";

const FORWARD_AFTER_HELP: &str = "\
Reads MCP JSON-RPC requests from standard input, one per line, and streams the JSON-RPC responses to
standard output as they arrive. --output does not apply to this stream.

Example:
  printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}' \\
    | chrome-devtools mcp forward-requests --profile default --session-id \"$SESSION\"";

#[derive(Parser)]
#[command(
    name = "chrome-devtools",
    version,
    about = "Profile-aware broker for Chrome DevTools MCP with isolated Chrome user data directories",
    after_help = ROOT_AFTER_HELP
)]
struct Cli {
    /// Output format
    #[arg(long, global = true, help_heading = "Global Options", value_enum, default_value_t = OutputFormat::Json)]
    output: OutputFormat,
    /// Print debug logs to stderr
    #[arg(long, global = true, help_heading = "Global Options")]
    debug: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mint, list, and close daemon sessions (one tab and uid namespace each)
    #[command(subcommand)]
    Session(SessionCommand),
    /// Call Chrome DevTools MCP tools through the profile daemon
    #[command(subcommand)]
    Mcp(McpCommand),
    /// Start, inspect, and stop the per-profile daemon
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// List profiles and inspect or stop the Chrome bound to a profile
    #[command(subcommand)]
    Profile(ProfileCommand),
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Start the profile daemon if needed and mint a new session. Sessions expire after 30 minutes idle (24 hours without a tab)
    #[command(name = "create-session")]
    Create(ProfileArgs),
    /// List the sessions held by the profile daemon (empty when the daemon is not running)
    #[command(name = "list-sessions")]
    List(ProfileArgs),
    /// Drop a session and close its daemon-created tab (Chrome's last tab is kept)
    #[command(name = "close-session")]
    Close(SessionArgs),
}

#[derive(Subcommand)]
enum McpCommand {
    /// List the MCP tools (chrome-devtools-mcp tools and daemon-native tools) through the profile daemon
    #[command(name = "list-tools")]
    ListTools(ProfileArgs),
    /// Run steps (tool calls and sleeps) in order on one session and print the result of every step
    #[command(name = "run-batch", after_help = RUN_BATCH_AFTER_HELP)]
    RunBatch(RunBatchArgs),
    /// Bind a session and stream stdin JSON-RPC lines to the daemon's MCP process, printing responses as they arrive
    #[command(name = "forward-requests", after_help = FORWARD_AFTER_HELP)]
    ForwardRequests(SessionArgs),
    /// List the MCP tools by running chrome-devtools-mcp directly (through the daemon while it holds the profile lock)
    #[command(name = "list-direct-tools")]
    ListDirectTools(ProfileArgs),
    /// Bypass the daemon and run chrome-devtools-mcp over stdio. For manual debugging only: snapshot uids do not survive across invocations
    #[command(name = "forward-direct-requests")]
    ForwardDirectRequests(ProfileArgs),
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Start the background daemon for the profile, or report the one already running
    #[command(name = "start-daemon")]
    Start(ProfileArgs),
    /// Show the daemon's version, sessions, tabs, queue, latency, Chrome reachability, and exit history
    #[command(name = "get-daemon-status")]
    Status(ProfileArgs),
    /// Stop the daemon. Refused while sessions exist unless --force (their sessions are destroyed)
    #[command(name = "stop-daemon")]
    Stop(ForceArgs),
    /// Run the daemon in the foreground (spawned by start-daemon)
    #[command(name = "run-daemon", hide = true)]
    Run(ProfileArgs),
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// List the profiles defined in the config file
    #[command(name = "list-profiles")]
    List,
    /// Show whether the Chrome DevTools endpoint of the profile responds
    #[command(name = "get-profile-status")]
    Status(ProfileArgs),
    /// Stop the Chrome bound to the profile. Refused while the profile daemon runs unless --force
    #[command(name = "stop-profile")]
    Stop(ForceArgs),
}

#[derive(Args)]
struct ProfileArgs {
    /// Profile name from ~/.config/chrome-devtools/config.toml
    #[arg(long)]
    profile: String,
}

#[derive(Args)]
struct SessionArgs {
    /// Profile name from ~/.config/chrome-devtools/config.toml
    #[arg(long)]
    profile: String,
    /// Session id minted by `session create-session`
    #[arg(long)]
    session_id: String,
}

#[derive(Args)]
struct ForceArgs {
    /// Profile name from ~/.config/chrome-devtools/config.toml
    #[arg(long)]
    profile: String,
    /// Stop even while other agents may be using it
    #[arg(long, overrides_with = "no_force")]
    force: bool,
    /// Refuse to stop while other agents may be using it (default)
    #[arg(long, overrides_with = "force")]
    no_force: bool,
}

#[derive(Args)]
struct RunBatchArgs {
    /// Profile name from ~/.config/chrome-devtools/config.toml
    #[arg(long)]
    profile: String,
    /// Session id minted by `session create-session`
    #[arg(long)]
    session_id: String,
    /// Steps: JSON arrays, JSON objects, or shorthand Type=tool,Name=...,Args={...} (space-separated; file:// reads a file)
    #[arg(long, num_args = 1.., required = true)]
    steps: Vec<String>,
    /// Write the output to this file instead of standard output
    #[arg(long)]
    output_file: Option<PathBuf>,
    /// Stop after the first errored tool step
    #[arg(long, overrides_with = "no_fail_fast")]
    fail_fast: bool,
    /// Continue after errored tool steps (default)
    #[arg(long, overrides_with = "fail_fast")]
    no_fail_fast: bool,
}

pub(crate) fn run() -> Result<()> {
    let mut root = Cli::command();
    root.build();
    let argv = grammar::prepare(&root, std::env::args_os().collect())?;
    if argv.iter().any(|token| token == "--debug") {
        eprintln!("debug: argv {:?}", &argv[1..]);
    }

    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() { 252 } else { 0 };
            let _ = error.print();
            std::process::exit(code);
        }
    };

    let config = load_or_create_config()?;
    let (name, value, output_file) = dispatch(&config, cli.command)?;
    let rendered = output::render(
        &grammar::pascal_case(name),
        Response { value, text: None },
        cli.output,
    )?;
    match output_file {
        Some(path) => fs::write(&path, rendered)
            .map_err(|error| other(format!("failed to write {}: {error}", path.display()))),
        None => output::print_stdout(&rendered),
    }
}

type Dispatched = (&'static str, Value, Option<PathBuf>);

fn done(name: &'static str, value: Value) -> Result<Dispatched> {
    Ok((name, value, None))
}

fn dispatch(config: &Config, command: Command) -> Result<Dispatched> {
    match command {
        Command::Session(SessionCommand::Create(args)) => done(
            "create-session",
            create_session(&profile(config, &args.profile)?)?,
        ),
        Command::Session(SessionCommand::List(args)) => done(
            "list-sessions",
            list_sessions(&profile(config, &args.profile)?)?,
        ),
        Command::Session(SessionCommand::Close(args)) => done(
            "close-session",
            close_session(&profile(config, &args.profile)?, &args.session_id)?,
        ),
        Command::Mcp(McpCommand::ListTools(args)) => {
            let response = list_mcp_tools_via_daemon(&profile(config, &args.profile)?)?;
            done("list-tools", tools_output(&response)?)
        }
        Command::Mcp(McpCommand::ListDirectTools(args)) => {
            let profile = profile(config, &args.profile)?;
            let response = if is_daemon_ready(&profile)? {
                eprintln!(
                    "note: the daemon for profile {} holds the profile lock, so the tools are listed through the daemon (same as `mcp list-tools`)",
                    profile.name
                );
                list_mcp_tools_via_daemon(&profile)?
            } else {
                let _lock = acquire_profile_lock(&profile)?;
                ensure_chrome(&profile)?;
                list_mcp_tools(&profile)?
            };
            done("list-direct-tools", tools_output(&response)?)
        }
        Command::Mcp(McpCommand::RunBatch(args)) => run_batch(config, args),
        Command::Mcp(McpCommand::ForwardRequests(args)) => {
            call_daemon(&profile(config, &args.profile)?, &args.session_id)?;
            std::process::exit(0);
        }
        Command::Mcp(McpCommand::ForwardDirectRequests(args)) => {
            let profile = profile(config, &args.profile)?;
            let _lock = acquire_profile_lock(&profile)?;
            ensure_chrome(&profile)?;
            exec_mcp(&profile)?;
            std::process::exit(0);
        }
        Command::Daemon(DaemonCommand::Start(args)) => {
            let profile = profile(config, &args.profile)?;
            let started = start_daemon(&profile)?;
            let socket = daemon_socket_path(&profile)?.display().to_string();
            let value = match started {
                Some(pid) => {
                    json!({"Profile": profile.name, "Daemon": "started", "Pid": pid, "Socket": socket})
                }
                None => json!({
                    "Profile": profile.name,
                    "Daemon": "ready",
                    "Pid": read_pid_file(&daemon_pid_path(&profile)?),
                    "Socket": socket
                }),
            };
            done("start-daemon", value)
        }
        Command::Daemon(DaemonCommand::Run(args)) => {
            run_daemon(&profile(config, &args.profile)?)?;
            std::process::exit(0);
        }
        Command::Daemon(DaemonCommand::Status(args)) => done(
            "get-daemon-status",
            daemon_status(&profile(config, &args.profile)?)?,
        ),
        Command::Daemon(DaemonCommand::Stop(args)) => {
            let profile = profile(config, &args.profile)?;
            match stop_daemon(&profile, args.force)? {
                DaemonStop::Stopped => done(
                    "stop-daemon",
                    json!({"Profile": profile.name, "Daemon": "stopped"}),
                ),
                DaemonStop::Refused(message) => Err(target(message)),
            }
        }
        Command::Profile(ProfileCommand::List) => {
            let profiles: Vec<Value> = config
                .profiles
                .iter()
                .map(|profile| json!({"Name": profile.name, "UserDataDir": profile.user_data_dir}))
                .collect();
            done("list-profiles", json!({ "Profiles": profiles }))
        }
        Command::Profile(ProfileCommand::Status(args)) => {
            let profile = profile(config, &args.profile)?;
            let port = current_port(&profile).filter(|port| is_devtools_ready(*port));
            done(
                "get-profile-status",
                json!({
                    "Profile": profile.name,
                    "Status": if port.is_some() { "ready" } else { "stopped" },
                    "Port": port,
                    "UserDataDir": profile.user_data_dir
                }),
            )
        }
        Command::Profile(ProfileCommand::Stop(args)) => {
            let profile = profile(config, &args.profile)?;
            if !args.force && is_daemon_ready(&profile)? {
                return Err(target(format!(
                    "daemon for profile {} is running and other agents may be using it; stopping Chrome would break their sessions. run 'chrome-devtools daemon stop-daemon --profile {}' first, or pass --force",
                    profile.name, profile.name
                )));
            }
            let stopped = stop_profile(&profile)?;
            done(
                "stop-profile",
                json!({"Profile": profile.name, "Stopped": stopped}),
            )
        }
    }
}

fn profile(config: &Config, name: &str) -> Result<Profile> {
    find_profile(config, name).map_err(target)
}

fn run_batch(config: &Config, args: RunBatchArgs) -> Result<Dispatched> {
    let steps = parse_steps(&args.steps)?;
    let options = BatchOptions {
        profile: profile(config, &args.profile)?,
        session_id: args.session_id,
        steps,
        fail_fast: args.fail_fast,
    };
    let (value, failure) = match execute_batch(&options) {
        Ok(run) => {
            let failure = run
                .stopped_on_error
                .map(|label| target(format!("batch stopped on error at step: {label}")));
            (json!({ "Steps": run.results }), failure)
        }
        Err(error) => (
            json!({ "Steps": [], "Error": error.message() }),
            Some(error),
        ),
    };
    match failure {
        None => Ok(("run-batch", value, args.output_file)),
        Some(error) => {
            let rendered =
                serde_json::to_string_pretty(&value).map_err(|error| other(error.to_string()))?;
            match &args.output_file {
                Some(path) => {
                    let _ = fs::write(path, format!("{rendered}\n"));
                }
                None => output::print_stdout(&format!("{rendered}\n"))?,
            }
            Err(error)
        }
    }
}

fn tools_output(response: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(response)
        .map_err(|error| other(format!("MCP returned invalid tools/list JSON: {error}")))?;
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(target(format!("MCP tools/list failed: {error}")));
    }
    let tools = value
        .pointer("/result/tools")
        .cloned()
        .unwrap_or_else(|| json!([]));
    Ok(json!({ "Tools": tools }))
}

fn control_line(profile: &Profile, command: &str) -> Result<String> {
    let response = send_daemon_control(profile, command)?;
    let line = response
        .lines()
        .next()
        .ok_or_else(|| other("daemon returned empty response"))?;
    if let Some(message) = line.strip_prefix("error=") {
        return Err(target(message));
    }
    Ok(line.to_string())
}

fn create_session(profile: &Profile) -> Result<Value> {
    ensure_daemon(profile)?;
    Ok(session_value(&control_line(profile, "session_create")?))
}

fn list_sessions(profile: &Profile) -> Result<Value> {
    if !is_daemon_ready(profile)? {
        return Ok(json!({ "Sessions": [] }));
    }
    let response = send_daemon_control(profile, "session_list")?;
    let sessions: Vec<Value> = response
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(session_value)
        .collect();
    Ok(json!({ "Sessions": sessions }))
}

fn close_session(profile: &Profile, session_id: &str) -> Result<Value> {
    if !is_daemon_ready(profile)? {
        return Err(target(format!(
            "unknown session: {session_id} (the daemon for profile {} is not running)",
            profile.name
        )));
    }
    control_line(profile, &format!("session_close session={session_id}"))?;
    Ok(json!({"SessionId": session_id, "Closed": true}))
}

/// Splits a daemon `key=value key=value` line; a token without `=` continues the previous value.
pub(crate) fn parse_fields(line: &str) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for token in line.split_whitespace() {
        match token.split_once('=') {
            Some((key, value))
                if !key.is_empty()
                    && key.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_') =>
            {
                fields.push((key.to_string(), value.to_string()))
            }
            _ => {
                if let Some((_, value)) = fields.last_mut() {
                    value.push(' ');
                    value.push_str(token);
                }
            }
        }
    }
    fields
}

pub(crate) fn snake_to_pascal(key: &str) -> String {
    grammar::pascal_case(&key.replace('_', "-"))
}

pub(crate) fn typed_value(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::Null;
    }
    match raw {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    match raw.parse::<u64>() {
        Ok(number) => Value::from(number),
        Err(_) => Value::String(raw.to_string()),
    }
}

pub(crate) fn session_value(line: &str) -> Value {
    let mut map = Map::new();
    for (key, raw) in parse_fields(line) {
        let key = match key.as_str() {
            "session" => "SessionId".to_string(),
            other => snake_to_pascal(other),
        };
        let value = if key == "SessionId" || key == "PageUrl" || key == "TargetId" {
            if raw.is_empty() {
                Value::Null
            } else {
                Value::String(raw)
            }
        } else {
            typed_value(&raw)
        };
        map.insert(key, value);
    }
    Value::Object(map)
}

fn daemon_status(profile: &Profile) -> Result<Value> {
    let socket_path = daemon_socket_path(profile)?;
    let pid = read_pid_file(&daemon_pid_path(profile)?);
    let status_response = if socket_path.exists() {
        send_daemon_control(profile, "status").ok()
    } else {
        None
    };
    let (last_event, abnormal_exits, respawns) = match read_daemon_health(profile) {
        Some((last, abnormal, respawns)) => (Value::String(last), abnormal, respawns),
        None => (Value::Null, 0, 0),
    };
    let mut map = Map::new();
    map.insert("Profile".to_string(), json!(profile.name));
    match status_response.filter(|response| response.contains("daemon=ready")) {
        None => {
            map.insert("Daemon".to_string(), json!("stopped"));
        }
        Some(response) => {
            let fields: Map<String, Value> = parse_fields(response.trim())
                .into_iter()
                .map(|(key, raw)| (key, Value::String(raw)))
                .collect();
            let field = |key: &str| {
                fields
                    .get(key)
                    .and_then(Value::as_str)
                    .map(typed_value)
                    .unwrap_or(Value::Null)
            };
            map.insert("Daemon".to_string(), json!("ready"));
            map.insert(
                "Version".to_string(),
                fields
                    .get("version")
                    .cloned()
                    .unwrap_or_else(|| json!("pre-0.3.1")),
            );
            map.insert("Sessions".to_string(), field("sessions"));
            map.insert("ActiveSessions".to_string(), field("active_sessions"));
            let pages: Vec<Value> = fields
                .get("pages")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split(',')
                .filter(|page| !page.is_empty())
                .map(typed_value)
                .collect();
            map.insert("Pages".to_string(), Value::Array(pages));
            map.insert(
                "QueuedMcpRequests".to_string(),
                field("queued_mcp_requests"),
            );
            map.insert(
                "MaxControlLatencyMs".to_string(),
                field("max_control_latency_ms"),
            );
            map.insert(
                "MaxForwardLatencyMs".to_string(),
                field("max_forward_latency_ms"),
            );
            map.insert(
                "DiagnosticWindowSecs".to_string(),
                field("diagnostic_window_secs"),
            );
            let port = fields
                .get("mcp_port")
                .and_then(Value::as_str)
                .and_then(|port| port.parse::<u16>().ok())
                .filter(|port| *port != 0);
            let chrome = match port {
                Some(port) if is_devtools_ready(port) => "ready",
                Some(_) => "unreachable",
                None => "unknown",
            };
            map.insert("Chrome".to_string(), json!(chrome));
            map.insert("ChromePort".to_string(), json!(port));
        }
    }
    map.insert("Pid".to_string(), json!(pid));
    map.insert(
        "Socket".to_string(),
        json!(socket_path.display().to_string()),
    );
    map.insert("AbnormalExits".to_string(), json!(abnormal_exits));
    map.insert("Respawns".to_string(), json!(respawns));
    map.insert("LastEvent".to_string(), last_event);
    Ok(Value::Object(map))
}
