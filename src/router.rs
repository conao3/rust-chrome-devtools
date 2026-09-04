use crate::cdp;
use crate::config::Profile;
use crate::daemon::{
    current_port, daemon_mcp_command, ensure_chrome, initialize_daemon_mcp, is_devtools_ready,
    record_daemon_respawn, terminate_child,
};
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::env;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

const POLLHUP: i16 = 0x10;

pub(crate) const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

pub(crate) const SESSION_REAPER_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) fn mcp_request_timeout() -> Duration {
    env::var("CHROME_DEVTOOLS_MCP_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(MCP_REQUEST_TIMEOUT)
}

pub(crate) const MCP_HEAVY_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn mcp_heavy_request_timeout() -> Duration {
    env::var("CHROME_DEVTOOLS_MCP_HEAVY_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(MCP_HEAVY_REQUEST_TIMEOUT)
}

// Chrome の Accessibility.getFullAXTree は DOM ノード数に対し超線形 (~O(n^2)) で、
// 巨大ページでは分単位かかる。timeout で見放しても MCP は処理を中断しないため
// (単一 tool mutex も保持したまま)、これらの tool は長い deadline で待つ方が損が少ない。
pub(crate) fn is_heavy_mcp_tool(tool: &str) -> bool {
    matches!(
        tool,
        "take_snapshot"
            | "take_screenshot"
            | "performance_start_trace"
            | "performance_stop_trace"
            | "performance_analyze_insight"
    )
}

pub(crate) fn request_timeout_for_line(line: &str) -> Duration {
    let (_, tool) = request_label(line);
    if is_heavy_mcp_tool(&tool) {
        mcp_heavy_request_timeout().max(mcp_request_timeout())
    } else {
        mcp_request_timeout()
    }
}

pub(crate) const MCP_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn mcp_probe_timeout() -> Duration {
    env::var("CHROME_DEVTOOLS_MCP_PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(MCP_PROBE_TIMEOUT)
}

pub(crate) const DIAGNOSTIC_WINDOW: Duration = Duration::from_secs(10 * 60);

pub(crate) fn diagnostic_window() -> Duration {
    env::var("CHROME_DEVTOOLS_DIAGNOSTIC_WINDOW_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DIAGNOSTIC_WINDOW)
}

pub(crate) const CONTROL_WARN_LATENCY: Duration = Duration::from_secs(1);

pub(crate) fn control_warn_latency() -> Duration {
    env::var("CHROME_DEVTOOLS_CONTROL_WARN_LATENCY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(CONTROL_WARN_LATENCY)
}

pub(crate) const FORWARD_WARN_LATENCY: Duration = Duration::from_secs(30);

pub(crate) fn forward_warn_latency() -> Duration {
    env::var("CHROME_DEVTOOLS_FORWARD_WARN_LATENCY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(FORWARD_WARN_LATENCY)
}

pub(crate) fn session_idle_ttl() -> Duration {
    env::var("CHROME_DEVTOOLS_SESSION_IDLE_TTL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(SESSION_IDLE_TTL)
}

pub(crate) fn session_reaper_interval() -> Duration {
    env::var("CHROME_DEVTOOLS_SESSION_REAPER_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(SESSION_REAPER_INTERVAL)
}

#[derive(Clone, Debug)]
pub(crate) struct PageCleanup {
    pub(crate) page_id: u64,
    pub(crate) page_url: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct UidBinding {
    pub(crate) page_id: u64,
    pub(crate) snapshot_epoch: u64,
    pub(crate) raw_uid: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionState {
    pub(crate) id: String,
    pub(crate) created_at: SystemTime,
    pub(crate) last_used_at: SystemTime,
    pub(crate) owned: bool,
    pub(crate) page_id: Option<u64>,
    pub(crate) page_created_by_daemon: bool,
    pub(crate) page_url: Option<String>,
    /// CDP の target id。タブの寿命の間ずっと変わらないので、daemon-native tool は
    /// URL ではなくこれで接続先を決める (URL は遷移で変わり、他タブと衝突する)。
    pub(crate) target_id: Option<String>,
    pub(crate) snapshot_epoch: u64,
    pub(crate) uid_bindings: HashMap<String, UidBinding>,
}

#[derive(Default)]
pub(crate) struct SessionRegistry {
    pub(crate) sessions: HashMap<String, SessionState>,
}

impl SessionRegistry {
    pub(crate) fn create(&mut self) -> SessionState {
        let now = SystemTime::now();
        let id = generate_session_id();
        let state = SessionState {
            id: id.clone(),
            created_at: now,
            last_used_at: now,
            owned: false,
            page_id: None,
            page_created_by_daemon: false,
            page_url: None,
            target_id: None,
            snapshot_epoch: 0,
            uid_bindings: HashMap::new(),
        };
        self.sessions.insert(id.clone(), state.clone());
        state
    }

    pub(crate) fn list(&self) -> Vec<SessionState> {
        let mut sessions = self.sessions.values().cloned().collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.created_at);
        sessions
    }

    pub(crate) fn close(&mut self, id: &str) -> Result<Option<PageCleanup>, String> {
        let Some(session) = self.sessions.get(id) else {
            return Err(format!("unknown session: {id}"));
        };
        if session.owned {
            return Err(format!("session in use: {id}"));
        }
        let session = self
            .sessions
            .remove(id)
            .ok_or_else(|| format!("unknown session: {id}"))?;
        log_session_removed("session_close", &session);
        Ok(page_cleanup_for_session(session))
    }

    pub(crate) fn bind(&mut self, id: &str) -> Result<(), String> {
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

    pub(crate) fn unbind(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.owned = false;
            session.last_used_at = SystemTime::now();
        }
    }

    pub(crate) fn touch(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.last_used_at = SystemTime::now();
        }
    }

    pub(crate) fn reap_expired(&mut self, ttl: Duration) -> Vec<PageCleanup> {
        let now = SystemTime::now();
        let mut cleanup = Vec::new();
        self.sessions.retain(|_, session| {
            if session.owned {
                return true;
            }
            let keep = match now.duration_since(session.last_used_at) {
                Ok(elapsed) => elapsed < ttl,
                Err(_) => true,
            };
            if !keep {
                log_session_removed("idle_reap", session);
                if let Some(page) = page_cleanup_for_session(session.clone()) {
                    cleanup.push(page);
                }
            }
            keep
        });
        cleanup
    }

    pub(crate) fn page_id(&self, id: &str) -> Option<u64> {
        self.sessions.get(id).and_then(|session| session.page_id)
    }

    pub(crate) fn set_page(
        &mut self,
        id: &str,
        page_id: u64,
        page_created_by_daemon: bool,
        page_url: Option<String>,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| format!("unknown session: {id}"))?;
        if session.page_id != Some(page_id) {
            session.snapshot_epoch = session.snapshot_epoch.wrapping_add(1);
            session.uid_bindings.clear();
            // 別ページに移ったら target id は無効。次の native call で解決し直す。
            session.target_id = None;
        }
        session.page_id = Some(page_id);
        session.page_created_by_daemon = page_created_by_daemon;
        session.page_url = page_url;
        session.last_used_at = SystemTime::now();
        Ok(())
    }

    pub(crate) fn set_target_id(&mut self, id: &str, target_id: Option<String>) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.target_id = target_id;
        }
    }

    pub(crate) fn clear_page(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.page_id = None;
            session.page_created_by_daemon = false;
            session.page_url = None;
            session.target_id = None;
            session.snapshot_epoch = session.snapshot_epoch.wrapping_add(1);
            session.uid_bindings.clear();
            session.last_used_at = SystemTime::now();
        }
    }

    pub(crate) fn clear_sessions(&mut self, reason: &str) {
        for session in self.sessions.values() {
            log_session_removed(reason, session);
        }
        self.sessions.clear();
    }

    pub(crate) fn record_snapshot_uids(
        &mut self,
        id: &str,
        page_id: u64,
        raw_uids: &[String],
    ) -> Result<HashMap<String, String>, String> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| format!("unknown session: {id}"))?;
        session.snapshot_epoch = session.snapshot_epoch.wrapping_add(1);
        session.uid_bindings.clear();
        let mut replacements = HashMap::new();
        for raw_uid in raw_uids {
            let token = format!(
                "u:{}:{}:{}",
                session_short_id(id),
                session.snapshot_epoch,
                raw_uid
            );
            replacements.insert(raw_uid.clone(), token.clone());
            session.uid_bindings.insert(
                token,
                UidBinding {
                    page_id,
                    snapshot_epoch: session.snapshot_epoch,
                    raw_uid: raw_uid.clone(),
                },
            );
        }
        session.last_used_at = SystemTime::now();
        Ok(replacements)
    }

    pub(crate) fn translate_uid_token(
        &self,
        id: &str,
        page_id: u64,
        token: &str,
    ) -> Result<String, String> {
        let session = self
            .sessions
            .get(id)
            .ok_or_else(|| format!("unknown session: {id}"))?;
        let Some(rest) = token.strip_prefix("u:") else {
            return Err("session uid token is required".to_string());
        };
        let parts = rest.splitn(3, ':').collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err("invalid session uid token".to_string());
        }
        if parts[0] != session_short_id(id) {
            return Err("uid token belongs to another session".to_string());
        }
        let epoch = parts[1]
            .parse::<u64>()
            .map_err(|_| "invalid session uid token epoch".to_string())?;
        if epoch != session.snapshot_epoch {
            return Err("stale uid token".to_string());
        }
        let binding = session
            .uid_bindings
            .get(token)
            .ok_or_else(|| "unknown uid token".to_string())?;
        if binding.page_id != page_id {
            return Err("uid token belongs to another page".to_string());
        }
        if binding.snapshot_epoch != session.snapshot_epoch {
            return Err("stale uid token".to_string());
        }
        Ok(binding.raw_uid.clone())
    }
}

fn page_cleanup_for_session(session: SessionState) -> Option<PageCleanup> {
    if session.page_created_by_daemon {
        session.page_id.map(|page_id| PageCleanup {
            page_id,
            page_url: session.page_url,
        })
    } else {
        None
    }
}

fn log_session_removed(reason: &str, session: &SessionState) {
    eprintln!(
        "session_removed reason={reason} session={} owned={} page={} page_created_by_daemon={} last_used={}",
        session.id,
        session.owned,
        session
            .page_id
            .map(|page_id| page_id.to_string())
            .unwrap_or_default(),
        session.page_created_by_daemon,
        unix_secs(session.last_used_at)
    );
}

pub(crate) fn generate_session_id() -> String {
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

pub(crate) fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn session_short_id(id: &str) -> &str {
    id.strip_prefix("sess-").unwrap_or(id)
}

#[derive(Debug)]
pub(crate) enum DaemonError {
    Client(String),
    Fatal(String),
}

pub(crate) type SharedSessions = Arc<(Mutex<SessionRegistry>, Condvar)>;

#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(target_os = "macos")]
const O_NONBLOCK: i32 = 0x0004;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;

unsafe extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

pub(crate) struct McpRuntime {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    port: u16,
}

pub(crate) fn set_nonblocking(fd: i32) -> Result<(), String> {
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err("failed to read pipe flags".to_string());
    }
    let result = unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) };
    if result < 0 {
        return Err("failed to set pipe nonblocking".to_string());
    }
    Ok(())
}

pub(crate) fn start_mcp_runtime(profile: &Profile) -> Result<McpRuntime, String> {
    ensure_chrome(profile)?;
    let port = current_port(profile)
        .ok_or_else(|| "runtime port is missing after ensure_chrome".to_string())?;
    let mut command = daemon_mcp_command(profile);
    eprintln!(
        "chrome-devtools {} daemon starting MCP: {:?}",
        env!("CARGO_PKG_VERSION"),
        command
    );
    let mut child = command
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
    initialize_daemon_mcp(&mut stdin, &mut reader)?;
    set_nonblocking(reader.get_ref().as_raw_fd())?;
    Ok(McpRuntime {
        child,
        stdin,
        reader,
        port,
    })
}

impl Drop for McpRuntime {
    fn drop(&mut self) {
        terminate_child(&mut self.child);
    }
}

fn clear_sessions(sessions: &SharedSessions, reason: &str) {
    let (lock, cvar) = &**sessions;
    if let Ok(mut registry) = lock.lock() {
        registry.clear_sessions(reason);
        cvar.notify_all();
    }
}

fn daemon_page_cleanups(sessions: &SharedSessions) -> Vec<PageCleanup> {
    let (lock, _) = &**sessions;
    lock.lock()
        .map(|registry| {
            registry
                .list()
                .into_iter()
                .filter_map(page_cleanup_for_session)
                .collect()
        })
        .unwrap_or_default()
}

fn respawn_runtime(
    profile: &Profile,
    runtime: &mut McpRuntime,
    sessions: &SharedSessions,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
    reason: &str,
) -> Result<(), String> {
    let pages = daemon_page_cleanups(sessions);
    terminate_child(&mut runtime.child);
    ensure_chrome(profile)?;
    let next = start_mcp_runtime(profile)?;
    *runtime = next;
    if let Err(error) = close_respawned_daemon_pages(
        &mut runtime.stdin,
        &mut runtime.reader,
        next_id,
        abandoned_ids,
        pages,
    ) {
        eprintln!("warning: failed to clean daemon-created pages after MCP respawn: {error}");
    }
    clear_sessions(sessions, reason);
    record_daemon_respawn(profile, reason);
    Ok(())
}

fn ensure_runtime_ready(
    profile: &Profile,
    runtime: &mut McpRuntime,
    sessions: &SharedSessions,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<(), String> {
    if runtime
        .child
        .try_wait()
        .map_err(|error| format!("failed to poll chrome-devtools-mcp: {error}"))?
        .is_some()
    {
        return respawn_runtime(
            profile,
            runtime,
            sessions,
            next_id,
            abandoned_ids,
            "mcp exited",
        );
    }
    if !is_devtools_ready(runtime.port) {
        return respawn_runtime(
            profile,
            runtime,
            sessions,
            next_id,
            abandoned_ids,
            "devtools unreachable",
        );
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct RouterHandle {
    sender: mpsc::Sender<RouterRequest>,
    queued: Arc<AtomicUsize>,
    diagnostics: Arc<Mutex<Diagnostics>>,
}

pub(crate) enum RouterRequest {
    Forward {
        line: String,
        session_id: Option<String>,
        enqueued_at: Instant,
        queued_at_enqueue: usize,
        response: mpsc::Sender<Result<Vec<String>, String>>,
    },
    ClosePages {
        pages: Vec<PageCleanup>,
        response: mpsc::Sender<Result<(), String>>,
    },
}

#[derive(Default)]
struct Diagnostics {
    control: VecDeque<(Instant, Duration)>,
    forward: VecDeque<(Instant, Duration)>,
}

#[derive(Clone, Copy)]
pub(crate) struct DiagnosticsSnapshot {
    pub(crate) window_secs: u64,
    pub(crate) max_control_latency_ms: u128,
    pub(crate) max_forward_latency_ms: u128,
}

impl Diagnostics {
    fn record_control(&mut self, elapsed: Duration, window: Duration) -> u128 {
        self.control.push_back((Instant::now(), elapsed));
        self.prune(window);
        self.max_control_latency_ms(window)
    }

    fn record_forward(&mut self, elapsed: Duration, window: Duration) -> u128 {
        self.forward.push_back((Instant::now(), elapsed));
        self.prune(window);
        self.max_forward_latency_ms(window)
    }

    fn snapshot(&mut self, window: Duration) -> DiagnosticsSnapshot {
        self.prune(window);
        DiagnosticsSnapshot {
            window_secs: window.as_secs(),
            max_control_latency_ms: self.max_control_latency_ms(window),
            max_forward_latency_ms: self.max_forward_latency_ms(window),
        }
    }

    fn prune(&mut self, window: Duration) {
        let now = Instant::now();
        let cutoff = now.checked_sub(window).unwrap_or(now);
        while self
            .control
            .front()
            .map(|(recorded_at, _)| *recorded_at < cutoff)
            == Some(true)
        {
            self.control.pop_front();
        }
        while self
            .forward
            .front()
            .map(|(recorded_at, _)| *recorded_at < cutoff)
            == Some(true)
        {
            self.forward.pop_front();
        }
    }

    fn max_control_latency_ms(&self, window: Duration) -> u128 {
        Self::max_latency_ms(&self.control, window)
    }

    fn max_forward_latency_ms(&self, window: Duration) -> u128 {
        Self::max_latency_ms(&self.forward, window)
    }

    fn max_latency_ms(records: &VecDeque<(Instant, Duration)>, window: Duration) -> u128 {
        let now = Instant::now();
        let cutoff = now.checked_sub(window).unwrap_or(now);
        records
            .iter()
            .filter(|(recorded_at, _)| *recorded_at >= cutoff)
            .map(|(_, elapsed)| elapsed.as_millis())
            .max()
            .unwrap_or(0)
    }
}

fn record_forward_diagnostic(
    diagnostics: &Arc<Mutex<Diagnostics>>,
    session_id: Option<&str>,
    line: &str,
    queue_wait: Duration,
    total: Duration,
    queued_at_enqueue: usize,
    queued_after_done: usize,
) {
    let max_ms = diagnostics
        .lock()
        .map(|mut diagnostics| diagnostics.record_forward(total, diagnostic_window()))
        .unwrap_or(0);
    if total >= forward_warn_latency() {
        let (method, tool) = request_label(line);
        eprintln!(
            "warning: slow forward method={method} tool={tool} session={} queue_wait_ms={} total_ms={} queued_at_enqueue={queued_at_enqueue} queued_after_done={queued_after_done} max_forward_latency_ms={max_ms}",
            session_id.unwrap_or(""),
            queue_wait.as_millis(),
            total.as_millis()
        );
    }
}

fn request_label(line: &str) -> (String, String) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return ("invalid".to_string(), String::new());
    };
    let method = value
        .get("method")
        .and_then(|method| method.as_str())
        .unwrap_or("")
        .to_string();
    let tool = value
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(|name| name.as_str())
        .unwrap_or("")
        .to_string();
    (method, tool)
}

impl RouterHandle {
    pub(crate) fn start(
        profile: Profile,
        mut runtime: McpRuntime,
        sessions: SharedSessions,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<RouterRequest>();
        let queued = Arc::new(AtomicUsize::new(0));
        let queued_for_router = Arc::clone(&queued);
        let diagnostics = Arc::new(Mutex::new(Diagnostics::default()));
        let diagnostics_for_router = Arc::clone(&diagnostics);
        thread::spawn(move || {
            let mut next_id: u64 = 10_000;
            let mut abandoned_ids = HashMap::new();
            for request in receiver {
                queued_for_router.fetch_sub(1, Ordering::SeqCst);
                if let Err(error) = ensure_runtime_ready(
                    &profile,
                    &mut runtime,
                    &sessions,
                    &mut next_id,
                    &mut abandoned_ids,
                ) {
                    eprintln!("warning: failed to self-heal MCP runtime: {error}");
                }
                match request {
                    RouterRequest::Forward {
                        line,
                        session_id,
                        enqueued_at,
                        queued_at_enqueue,
                        response,
                    } => {
                        let started_at = Instant::now();
                        let result = route_request(
                            &mut runtime.stdin,
                            &mut runtime.reader,
                            &profile,
                            &sessions,
                            session_id.as_deref(),
                            &line,
                            &mut next_id,
                            &mut abandoned_ids,
                        );
                        match result {
                            Ok(lines) => {
                                let _ = response.send(Ok(lines));
                            }
                            Err(message) if message.contains("MCP request timed out") => {
                                let client_message = format!(
                                    "{message}; the tool may still have completed in the background (e.g. screenshot files may still be written) — its late result is discarded"
                                );
                                if response
                                    .send(Ok(error_response_for_line(&line, &client_message)))
                                    .is_ok()
                                {
                                    if let Err(error) = probe_mcp_runtime(
                                        &mut runtime.stdin,
                                        &mut runtime.reader,
                                        &mut next_id,
                                        &mut abandoned_ids,
                                    ) {
                                        eprintln!(
                                            "warning: MCP probe failed after request timeout: {error}"
                                        );
                                        let _ = respawn_runtime(
                                            &profile,
                                            &mut runtime,
                                            &sessions,
                                            &mut next_id,
                                            &mut abandoned_ids,
                                            "mcp probe failed after request timeout",
                                        );
                                        abandoned_ids.clear();
                                    }
                                } else {
                                    eprintln!(
                                        "warning: skipped MCP timeout probe because client disconnected"
                                    );
                                }
                            }
                            Err(message) => {
                                let _ = response.send(Ok(error_response_for_line(&line, &message)));
                            }
                        }
                        let total = enqueued_at.elapsed();
                        let queue_wait = started_at.duration_since(enqueued_at);
                        record_forward_diagnostic(
                            &diagnostics_for_router,
                            session_id.as_deref(),
                            &line,
                            queue_wait,
                            total,
                            queued_at_enqueue,
                            queued_for_router.load(Ordering::SeqCst),
                        );
                    }
                    RouterRequest::ClosePages { pages, response } => {
                        let result = close_daemon_pages(
                            &mut runtime.stdin,
                            &mut runtime.reader,
                            &mut next_id,
                            &mut abandoned_ids,
                            pages,
                        );
                        let _ = response.send(result);
                    }
                }
            }
        });
        Self {
            sender,
            queued,
            diagnostics,
        }
    }

    pub(crate) fn enqueue_forward(
        &self,
        session_id: Option<&str>,
        line: &str,
    ) -> Result<mpsc::Receiver<Result<Vec<String>, String>>, DaemonError> {
        let (response_tx, response_rx) = mpsc::channel();
        let enqueued_at = Instant::now();
        let queued_at_enqueue = self.queued.fetch_add(1, Ordering::SeqCst) + 1;
        if let Err(error) = self.sender.send(RouterRequest::Forward {
            line: line.to_string(),
            session_id: session_id.map(|id| id.to_string()),
            enqueued_at,
            queued_at_enqueue,
            response: response_tx,
        }) {
            self.queued.fetch_sub(1, Ordering::SeqCst);
            return Err(DaemonError::Fatal(format!(
                "failed to queue MCP request: {error}"
            )));
        }
        Ok(response_rx)
    }

    pub(crate) fn close_pages(&self, pages: Vec<PageCleanup>) -> Result<(), DaemonError> {
        if pages.is_empty() {
            return Ok(());
        }
        let (response_tx, response_rx) = mpsc::channel();
        self.queued.fetch_add(1, Ordering::SeqCst);
        if let Err(error) = self.sender.send(RouterRequest::ClosePages {
            pages,
            response: response_tx,
        }) {
            self.queued.fetch_sub(1, Ordering::SeqCst);
            return Err(DaemonError::Fatal(format!(
                "failed to queue MCP page cleanup: {error}"
            )));
        }
        response_rx
            .recv()
            .map_err(|error| {
                DaemonError::Fatal(format!(
                    "failed to receive MCP page cleanup response: {error}"
                ))
            })?
            .map_err(DaemonError::Fatal)
    }

    pub(crate) fn queued_requests(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }

    pub(crate) fn record_control_latency(&self, name: &str, elapsed: Duration) {
        let max_ms = self
            .diagnostics
            .lock()
            .map(|mut diagnostics| diagnostics.record_control(elapsed, diagnostic_window()))
            .unwrap_or(0);
        if elapsed >= control_warn_latency() {
            eprintln!(
                "warning: slow control command={} elapsed_ms={} max_control_latency_ms={max_ms}",
                name,
                elapsed.as_millis()
            );
        }
    }

    pub(crate) fn diagnostics_snapshot(&self) -> DiagnosticsSnapshot {
        self.diagnostics
            .lock()
            .map(|mut diagnostics| diagnostics.snapshot(diagnostic_window()))
            .unwrap_or(DiagnosticsSnapshot {
                window_secs: diagnostic_window().as_secs(),
                max_control_latency_ms: 0,
                max_forward_latency_ms: 0,
            })
    }
}

pub(crate) const PAGE_SCOPED_TOOLS: &[&str] = &[
    "click",
    "drag",
    "emulate",
    "evaluate_script",
    "fill",
    "fill_form",
    "get_console_message",
    "get_network_request",
    "handle_dialog",
    "hover",
    "lighthouse_audit",
    "list_console_messages",
    "list_network_requests",
    "navigate_page",
    "performance_analyze_insight",
    "performance_start_trace",
    "performance_stop_trace",
    "press_key",
    "resize_page",
    "take_heapsnapshot",
    "take_screenshot",
    "take_snapshot",
    "type_text",
    "upload_file",
    "wait_for",
];

pub(crate) fn is_page_scoped_tool(name: &str) -> bool {
    PAGE_SCOPED_TOOLS.contains(&name)
}

pub(crate) fn forward_line(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    line: &str,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<Vec<String>, String> {
    let Some(original_id) = extract_jsonrpc_id_value(line) else {
        write_json_line(mcp_stdin, line)?;
        return Ok(Vec::new());
    };
    let internal_id = *next_id;
    *next_id = next_id.wrapping_add(1);
    let forwarded = rewrite_jsonrpc_id(line, serde_json::json!(internal_id))?;
    write_json_line(mcp_stdin, &forwarded)?;
    let mut lines = Vec::new();
    let deadline = Instant::now() + request_timeout_for_line(line);
    loop {
        let response_line = read_mcp_response_line_until_ignoring(
            mcp_stdin,
            mcp_reader,
            deadline,
            abandoned_ids,
        )
        .map_err(|error| {
            if matches!(error, DaemonError::Fatal(ref message) if message == "MCP request timed out")
            {
                abandoned_ids.insert(internal_id, Instant::now());
            }
            format!("{error:?}")
        })?;
        if extract_jsonrpc_id(&response_line) == Some(internal_id) {
            lines.push(rewrite_jsonrpc_id(&response_line, original_id.clone())?);
            return Ok(lines);
        }
        // MCP server 発の notification (notifications/cancelled 等) は daemon⇔MCP 間の
        // 制御情報であり、クライアントの JSON-RPC ストリームに混ぜると誤解を生む。
        if json_method_starts_with(&response_line, "notifications/") {
            eprintln!("mcp_notification dropped={response_line}");
            continue;
        }
        lines.push(response_line);
    }
}

fn mcp_call(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
    request: serde_json::Value,
) -> Result<serde_json::Value, String> {
    mcp_call_until(
        mcp_stdin,
        mcp_reader,
        next_id,
        abandoned_ids,
        request,
        Instant::now() + mcp_request_timeout(),
    )
}

fn mcp_call_until(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
    mut request: serde_json::Value,
    deadline: Instant,
) -> Result<serde_json::Value, String> {
    let internal_id = *next_id;
    *next_id = next_id.wrapping_add(1);
    request["jsonrpc"] = serde_json::json!("2.0");
    request["id"] = serde_json::json!(internal_id);
    write_json_line(mcp_stdin, &request.to_string())?;
    loop {
        let response_line = read_mcp_response_line_until_ignoring(
            mcp_stdin,
            mcp_reader,
            deadline,
            abandoned_ids,
        )
        .map_err(|error| {
            if matches!(error, DaemonError::Fatal(ref message) if message == "MCP request timed out")
            {
                abandoned_ids.insert(internal_id, Instant::now());
            }
            format!("{error:?}")
        })?;
        if extract_jsonrpc_id(&response_line) == Some(internal_id) {
            return serde_json::from_str(&response_line)
                .map_err(|error| format!("failed to parse MCP response: {error}"));
        }
    }
}

fn probe_mcp_runtime(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<(), String> {
    let _ = mcp_call_until(
        mcp_stdin,
        mcp_reader,
        next_id,
        abandoned_ids,
        serde_json::json!({
            "method": "tools/list",
            "params": {}
        }),
        Instant::now() + mcp_probe_timeout(),
    )?;
    Ok(())
}

fn selected_page_id(response: &serde_json::Value) -> Option<u64> {
    let pages = response
        .get("result")?
        .get("structuredContent")?
        .get("pages")?
        .as_array()?;
    pages
        .iter()
        .find(|page| page.get("selected").and_then(|value| value.as_bool()) == Some(true))
        .and_then(|page| page.get("id"))
        .and_then(|id| id.as_u64())
}

fn response_page_ids(response: &serde_json::Value) -> Vec<u64> {
    response
        .get("result")
        .and_then(|result| result.get("structuredContent"))
        .and_then(|structured| structured.get("pages"))
        .and_then(|pages| pages.as_array())
        .map(|pages| {
            pages
                .iter()
                .filter_map(|page| page.get("id").and_then(|id| id.as_u64()))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
struct PageEntry {
    id: u64,
    url: String,
}

fn response_page_entries(response: &serde_json::Value) -> Vec<PageEntry> {
    response
        .get("result")
        .and_then(|result| result.get("structuredContent"))
        .and_then(|structured| structured.get("pages"))
        .and_then(|pages| pages.as_array())
        .map(|pages| {
            pages
                .iter()
                .filter_map(|page| {
                    Some(PageEntry {
                        id: page.get("id").and_then(|id| id.as_u64())?,
                        url: page
                            .get("url")
                            .and_then(|url| url.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn list_page_ids(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<Vec<u64>, String> {
    let response = mcp_call(
        mcp_stdin,
        mcp_reader,
        next_id,
        abandoned_ids,
        serde_json::json!({
            "method": "tools/call",
            "params": {
                "name": "list_pages",
                "arguments": {}
            }
        }),
    )?;
    Ok(response_page_ids(&response))
}

fn close_respawned_daemon_pages(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
    pages: Vec<PageCleanup>,
) -> Result<(), String> {
    if pages.is_empty() {
        return Ok(());
    }
    let response = mcp_call_until(
        mcp_stdin,
        mcp_reader,
        next_id,
        abandoned_ids,
        serde_json::json!({
            "method": "tools/call",
            "params": {
                "name": "list_pages",
                "arguments": {}
            }
        }),
        Instant::now() + mcp_probe_timeout(),
    )?;
    let entries = response_page_entries(&response);
    let mut open_count = entries.len();
    let mut closed_ids = HashSet::new();
    let mut remaining_urls = pages.iter().filter_map(|page| page.page_url.as_ref()).fold(
        HashMap::<String, usize>::new(),
        |mut counts, url| {
            *counts.entry(url.clone()).or_default() += 1;
            counts
        },
    );

    for page in &pages {
        if open_count <= 1 {
            return Ok(());
        }
        let Some(entry) = entries.iter().find(|entry| {
            entry.id == page.page_id
                && page
                    .page_url
                    .as_ref()
                    .map(|url| &entry.url == url)
                    .unwrap_or(true)
        }) else {
            continue;
        };
        let _ = mcp_call_until(
            mcp_stdin,
            mcp_reader,
            next_id,
            abandoned_ids,
            serde_json::json!({
                "method": "tools/call",
                "params": {
                    "name": "close_page",
                    "arguments": { "pageId": entry.id }
                }
            }),
            Instant::now() + mcp_probe_timeout(),
        )?;
        closed_ids.insert(entry.id);
        open_count -= 1;
        if let Some(url) = page.page_url.as_ref() {
            if let Some(count) = remaining_urls.get_mut(url) {
                *count = count.saturating_sub(1);
            }
        }
    }

    for entry in &entries {
        if open_count <= 1 {
            return Ok(());
        }
        if closed_ids.contains(&entry.id) || entry.url == "about:blank" {
            continue;
        }
        let Some(count) = remaining_urls.get_mut(&entry.url) else {
            continue;
        };
        if *count == 0 {
            continue;
        }
        let _ = mcp_call_until(
            mcp_stdin,
            mcp_reader,
            next_id,
            abandoned_ids,
            serde_json::json!({
                "method": "tools/call",
                "params": {
                    "name": "close_page",
                    "arguments": { "pageId": entry.id }
                }
            }),
            Instant::now() + mcp_probe_timeout(),
        )?;
        *count -= 1;
        open_count -= 1;
    }
    Ok(())
}

fn close_daemon_pages(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
    pages: Vec<PageCleanup>,
) -> Result<(), String> {
    for page in pages {
        let current = list_page_ids(mcp_stdin, mcp_reader, next_id, abandoned_ids)?;
        if current.len() <= 1 || !current.contains(&page.page_id) {
            continue;
        }
        let _ = mcp_call(
            mcp_stdin,
            mcp_reader,
            next_id,
            abandoned_ids,
            serde_json::json!({
                "method": "tools/call",
                "params": {
                    "name": "close_page",
                    "arguments": { "pageId": page.page_id }
                }
            }),
        )?;
    }
    Ok(())
}

fn ensure_session_page(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    sessions: &SharedSessions,
    session_id: &str,
    port: Option<u16>,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<u64, String> {
    {
        let (lock, _) = &**sessions;
        let registry = lock
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        if !registry.sessions.contains_key(session_id) {
            return Err(format!("unknown session: {session_id}"));
        }
        if let Some(page_id) = registry.page_id(session_id) {
            return Ok(page_id);
        }
    }
    // new_page の前後で CDP target を差分し、割り当てられたタブの target id を確定する。
    // about:blank のタブは複数並びうるので URL 一致では特定できない。
    let before = port
        .and_then(|port| cdp::page_target_ids(port).ok())
        .unwrap_or_default();
    let response = mcp_call(
        mcp_stdin,
        mcp_reader,
        next_id,
        abandoned_ids,
        serde_json::json!({
            "method": "tools/call",
            "params": {
                "name": "new_page",
                "arguments": { "url": "about:blank", "background": true }
            }
        }),
    )?;
    let page_id = selected_page_id(&response)
        .ok_or_else(|| "failed to determine the allocated page id".to_string())?;
    let target_id = port.and_then(|port| {
        let after = cdp::page_target_ids(port).ok()?;
        let mut added = after.into_iter().filter(|id| !before.contains(id));
        let first = added.next()?;
        // 同時に複数タブが増えたら、どれが自分のものか決められない。
        added.next().is_none().then_some(first)
    });
    let (lock, _) = &**sessions;
    let mut registry = lock
        .lock()
        .map_err(|_| "session registry poisoned".to_string())?;
    registry.set_page(session_id, page_id, true, Some("about:blank".to_string()))?;
    registry.set_target_id(session_id, target_id);
    Ok(page_id)
}

fn session_page(sessions: &SharedSessions, session_id: &str) -> Option<u64> {
    let (lock, _) = &**sessions;
    lock.lock()
        .ok()
        .and_then(|registry| registry.page_id(session_id))
}

fn session_page_created_by_daemon(sessions: &SharedSessions, session_id: &str) -> bool {
    let (lock, _) = &**sessions;
    lock.lock()
        .ok()
        .and_then(|registry| registry.sessions.get(session_id).cloned())
        .map(|session| session.page_created_by_daemon)
        .unwrap_or(false)
}

fn session_page_url(sessions: &SharedSessions, session_id: &str) -> Option<String> {
    let (lock, _) = &**sessions;
    lock.lock()
        .ok()
        .and_then(|registry| registry.sessions.get(session_id).cloned())
        .and_then(|session| session.page_url)
}

fn session_target_id(sessions: &SharedSessions, session_id: &str) -> Option<String> {
    let (lock, _) = &**sessions;
    lock.lock()
        .ok()
        .and_then(|registry| registry.sessions.get(session_id).cloned())
        .and_then(|session| session.target_id)
}

fn set_session_target_id(sessions: &SharedSessions, session_id: &str, target_id: Option<String>) {
    let (lock, _) = &**sessions;
    if let Ok(mut registry) = lock.lock() {
        registry.set_target_id(session_id, target_id);
    }
}

fn record_session_page(
    sessions: &SharedSessions,
    session_id: &str,
    page_id: u64,
    created_by_daemon: bool,
    url: Option<String>,
) -> Result<(), String> {
    let (lock, _) = &**sessions;
    let mut registry = lock
        .lock()
        .map_err(|_| "session registry poisoned".to_string())?;
    registry.set_page(session_id, page_id, created_by_daemon, url)
}

fn inject_page_id(value: &mut serde_json::Value, page_id: u64) {
    let params = value
        .as_object_mut()
        .and_then(|object| object.get_mut("params"))
        .and_then(|params| params.as_object_mut());
    let Some(params) = params else {
        return;
    };
    let arguments = params
        .entry("arguments")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(arguments) = arguments.as_object_mut() {
        arguments.insert("pageId".to_string(), serde_json::json!(page_id));
    }
}

pub(crate) fn is_native_tool(name: &str) -> bool {
    matches!(
        name,
        "goto"
            | "screenshot_quiet"
            | "wait_for_js"
            | "click_at"
            | "click_selector"
            | "dispatch_key"
            | "set_file_input"
            | "type_into"
            | "close_page_quiet"
    )
}

fn native_tool_defs() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "goto",
            "description": "Daemon-native: navigate the session page with CDP without bringing Chrome to the front, then wait for page readiness, a selector, or a JavaScript expression.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "waitSelector": {"type": "string", "description": "CSS selector to wait for after navigation"},
                    "waitExpression": {"type": "string", "description": "JS expression to wait for after navigation; mutually exclusive with waitSelector"},
                    "timeout": {"type": "number", "description": "max wait in ms (default 30000, max 120000)"},
                    "interval": {"type": "number", "description": "poll interval in ms (default 250)"}
                },
                "required": ["url"]
            }
        }),
        serde_json::json!({
            "name": "screenshot_quiet",
            "description": "Daemon-native: capture the visible viewport of the session page directly with CDP, without selecting or foregrounding the page and without entering the MCP tool queue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filePath": {"type": "string", "description": "absolute output path under HOME or the OS temp directory"},
                    "format": {"type": "string", "enum": ["png", "jpeg", "webp"], "description": "default png"},
                    "quality": {"type": "number", "description": "JPEG/WebP quality from 0 to 100"}
                },
                "required": ["filePath"]
            }
        }),
        serde_json::json!({
            "name": "wait_for_js",
            "description": "Daemon-native: poll a JavaScript expression (or CSS selector) on the session page until it becomes truthy. Lightweight (no snapshot). Prefer this over fixed sleeps for SPA rendering waits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expression": {"type": "string", "description": "JS expression evaluated on the page; resolves when truthy"},
                    "selector": {"type": "string", "description": "CSS selector; resolves when it matches an element (alternative to expression)"},
                    "timeout": {"type": "number", "description": "max wait in ms (default 30000, max 120000)"},
                    "interval": {"type": "number", "description": "poll interval in ms (default 500)"}
                }
            }
        }),
        serde_json::json!({
            "name": "click_at",
            "description": "Daemon-native: dispatch a raw left click at viewport coordinates via CDP Input.dispatchMouseEvent. Works without a snapshot (use on pages too large for take_snapshot).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": {"type": "number"},
                    "y": {"type": "number"}
                },
                "required": ["x", "y"]
            }
        }),
        serde_json::json!({
            "name": "click_selector",
            "description": "Daemon-native: scroll the first element matching a CSS selector into view and click its center via CDP. Works without a snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": {"type": "string"}
                },
                "required": ["selector"]
            }
        }),
        serde_json::json!({
            "name": "dispatch_key",
            "description": "Daemon-native: dispatch a single key press (e.g. Escape, Enter, ArrowDown, Tab, or a single character) via CDP Input.dispatchKeyEvent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "modifiers": {"type": "number", "description": "bitmask: Alt=1, Ctrl=2, Meta=4, Shift=8"}
                },
                "required": ["key"]
            }
        }),
        serde_json::json!({
            "name": "set_file_input",
            "description": "Daemon-native: attach files to the input[type=file] matched by a CSS selector via CDP DOM.setFileInputFiles. No snapshot and no file chooser, so it stays inside the session page and picks the exact input on pages that have several.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": {"type": "string", "description": "CSS selector for an input[type=file] on the session page"},
                    "filePaths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "absolute paths under $HOME or the OS temp directory"
                    }
                },
                "required": ["selector", "filePaths"]
            }
        }),
        serde_json::json!({
            "name": "close_page_quiet",
            "description": "Daemon-native: close the session's own tab through the DevTools HTTP endpoint. The MCP close_page queues behind the tool mutex, so it cannot reach a tab that is holding the mutex open (a JavaScript dialog, a tool that never returns). Closing that tab releases the mutex. The session picks up a fresh tab on its next call.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        serde_json::json!({
            "name": "type_into",
            "description": "Daemon-native: focus the element matched by a CSS selector and insert text with CDP Input.insertText. Use when a native value setter plus an input event does not reach the framework state (some React-based ATS forms).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "text": {"type": "string"},
                    "clear": {"type": "boolean", "description": "select all and delete before inserting (default false)"}
                },
                "required": ["selector", "text"]
            }
        }),
    ]
}

fn inject_native_tool_defs(line: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    let Some(tools) = value
        .get_mut("result")
        .and_then(|result| result.get_mut("tools"))
        .and_then(|tools| tools.as_array_mut())
    else {
        return line.to_string();
    };
    tools.extend(native_tool_defs());
    value.to_string()
}

fn native_tool_response(id: serde_json::Value, text: String, is_error: bool) -> Vec<String> {
    let mut result = serde_json::json!({
        "content": [{"type": "text", "text": text}]
    });
    if is_error {
        result["isError"] = serde_json::json!(true);
    }
    vec![
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        })
        .to_string(),
    ]
}

fn run_native_tool(
    profile: &Profile,
    sessions: &SharedSessions,
    session_id: Option<&str>,
    page: cdp::PageRef<'_>,
    name: &str,
    arguments: &serde_json::Value,
) -> Result<String, String> {
    let port = current_port(profile).ok_or_else(|| "missing DevTools port".to_string())?;
    match name {
        "goto" => {
            let url = arguments
                .get("url")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "goto requires 'url'".to_string())?;
            let selector = arguments
                .get("waitSelector")
                .and_then(|value| value.as_str());
            let expression = arguments
                .get("waitExpression")
                .and_then(|value| value.as_str());
            if selector.is_some() && expression.is_some() {
                return Err("goto accepts only one of waitSelector or waitExpression".into());
            }
            let wait_expression = match (selector, expression) {
                (Some(selector), None) => {
                    format!("document.querySelector({})", serde_json::json!(selector))
                }
                (None, Some(expression)) => expression.to_string(),
                (None, None) => "document.readyState === 'complete'".to_string(),
                _ => unreachable!(),
            };
            let timeout = arguments
                .get("timeout")
                .and_then(|value| value.as_u64())
                .unwrap_or(30_000)
                .clamp(100, 120_000);
            let interval = arguments
                .get("interval")
                .and_then(|value| value.as_u64())
                .unwrap_or(250)
                .clamp(50, 10_000);
            let (elapsed, final_url) = cdp::goto(
                port,
                page,
                url,
                &wait_expression,
                Duration::from_millis(timeout),
                Duration::from_millis(interval),
            )?;
            if let Some(session_id) = session_id {
                if let Some(page_id) = session_page(sessions, session_id) {
                    let _ = record_session_page(
                        sessions,
                        session_id,
                        page_id,
                        session_page_created_by_daemon(sessions, session_id),
                        Some(final_url.clone()),
                    );
                }
            }
            Ok(format!(
                "navigated to {final_url}; condition became truthy after {}ms",
                elapsed.as_millis()
            ))
        }
        "screenshot_quiet" => {
            let file_path = arguments
                .get("filePath")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "screenshot_quiet requires 'filePath'".to_string())?;
            let format = arguments
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("png");
            if !matches!(format, "png" | "jpeg" | "webp") {
                return Err("screenshot_quiet format must be png, jpeg, or webp".into());
            }
            let quality = arguments.get("quality").and_then(|value| value.as_u64());
            let output = cdp::screenshot_quiet(
                port,
                page,
                std::path::Path::new(file_path),
                format,
                quality,
            )?;
            Ok(format!("saved screenshot to {}", output.display()))
        }
        "wait_for_js" => {
            let expression = match (
                arguments.get("expression").and_then(|value| value.as_str()),
                arguments.get("selector").and_then(|value| value.as_str()),
            ) {
                (Some(expression), _) => expression.to_string(),
                (None, Some(selector)) => {
                    format!("document.querySelector({})", serde_json::json!(selector))
                }
                (None, None) => {
                    return Err("wait_for_js requires 'expression' or 'selector'".to_string());
                }
            };
            let timeout = arguments
                .get("timeout")
                .and_then(|value| value.as_u64())
                .unwrap_or(30_000)
                .clamp(100, 120_000);
            let interval = arguments
                .get("interval")
                .and_then(|value| value.as_u64())
                .unwrap_or(500)
                .clamp(50, 10_000);
            let elapsed = cdp::wait_for_js(
                port,
                page,
                &expression,
                Duration::from_millis(timeout),
                Duration::from_millis(interval),
            )?;
            Ok(format!(
                "condition became truthy after {}ms",
                elapsed.as_millis()
            ))
        }
        "click_at" => {
            let x = arguments
                .get("x")
                .and_then(|value| value.as_f64())
                .ok_or_else(|| "click_at requires numeric 'x'".to_string())?;
            let y = arguments
                .get("y")
                .and_then(|value| value.as_f64())
                .ok_or_else(|| "click_at requires numeric 'y'".to_string())?;
            cdp::click_at(port, page, x, y)?;
            Ok(format!("clicked at ({x}, {y})"))
        }
        "click_selector" => {
            let selector = arguments
                .get("selector")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "click_selector requires 'selector'".to_string())?;
            let (x, y) = cdp::click_selector(port, page, selector)?;
            Ok(format!("clicked {selector} at ({x:.0}, {y:.0})"))
        }
        "dispatch_key" => {
            let key = arguments
                .get("key")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "dispatch_key requires 'key'".to_string())?;
            let modifiers = arguments
                .get("modifiers")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            cdp::dispatch_key(port, page, key, modifiers)?;
            Ok(format!("dispatched key {key}"))
        }
        "set_file_input" => {
            let selector = arguments
                .get("selector")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "set_file_input requires 'selector'".to_string())?;
            let files = arguments
                .get("filePaths")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(|path| path.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if files.is_empty() {
                return Err("set_file_input requires a non-empty 'filePaths' array".to_string());
            }
            let files = files
                .iter()
                .map(|path| {
                    cdp::allowed_input_path(std::path::Path::new(path))
                        .map(|resolved| resolved.to_string_lossy().into_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            cdp::set_file_input(port, page, selector, &files)?;
            Ok(format!("attached {} file(s) to {selector}", files.len()))
        }
        "close_page_quiet" => {
            let target_id = page
                .target_id
                .ok_or_else(|| "close_page_quiet requires a session page".to_string())?;
            cdp::close_target(port, target_id)?;
            if let Some(session_id) = session_id {
                let (lock, _) = &**sessions;
                if let Ok(mut registry) = lock.lock() {
                    registry.clear_page(session_id);
                }
            }
            Ok(format!("closed page target {target_id}"))
        }
        "type_into" => {
            let selector = arguments
                .get("selector")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "type_into requires 'selector'".to_string())?;
            let text = arguments
                .get("text")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "type_into requires 'text'".to_string())?;
            let clear = arguments
                .get("clear")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            cdp::type_into(port, page, selector, text, clear)?;
            Ok(format!(
                "typed {} char(s) into {selector}",
                text.chars().count()
            ))
        }
        other => Err(format!("unknown native tool: {other}")),
    }
}

pub(crate) fn try_native_tool(
    profile: &Profile,
    sessions: &SharedSessions,
    session_id: Option<&str>,
    line: &str,
) -> Option<Vec<String>> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("method").and_then(|method| method.as_str()) != Some("tools/call") {
        return None;
    }
    let name = value
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(|name| name.as_str())?
        .to_string();
    if !is_native_tool(&name) {
        return None;
    }
    // A fresh session has no MCP page id yet. Let the router allocate and record
    // one once; after that, native tools can keep bypassing the MCP queue.
    let session_id = session_id?;
    session_page(sessions, session_id)?;
    let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let empty_args = serde_json::json!({});
    let arguments = value
        .get("params")
        .and_then(|params| params.get("arguments"))
        .unwrap_or(&empty_args);
    let page_url = session_page_url(sessions, session_id);
    // target id が未解決のセッション (daemon respawn 後など) は、URL が一意に定まる
    // ときだけ 1 回解決して固定する。以後は URL が変わっても同じタブを掴み続ける。
    let mut target_id = session_target_id(sessions, session_id);
    if target_id.is_none() {
        if let (Some(port), Some(url)) = (current_port(profile), page_url.as_deref()) {
            if let Ok(Some(resolved)) = cdp::resolve_unique_target_id(port, url) {
                set_session_target_id(sessions, session_id, Some(resolved.clone()));
                target_id = Some(resolved);
            }
        }
    }
    let page = cdp::PageRef::new(target_id.as_deref(), page_url.as_deref());
    Some(
        match run_native_tool(profile, sessions, Some(session_id), page, &name, arguments) {
            Ok(text) => native_tool_response(id, text, false),
            Err(message) => native_tool_response(id, format!("{name} failed: {message}"), true),
        },
    )
}

fn strip_page_id_schema(line: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    let Some(tools) = value
        .get_mut("result")
        .and_then(|result| result.get_mut("tools"))
        .and_then(|tools| tools.as_array_mut())
    else {
        return line.to_string();
    };
    for tool in tools.iter_mut() {
        let name = tool
            .get("name")
            .and_then(|name| name.as_str())
            .unwrap_or("")
            .to_string();
        if !is_page_scoped_tool(&name) {
            continue;
        }
        let Some(schema) = tool.get_mut("inputSchema").and_then(|s| s.as_object_mut()) else {
            continue;
        };
        if let Some(properties) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            properties.remove("pageId");
        }
        if let Some(required) = schema.get_mut("required").and_then(|r| r.as_array_mut()) {
            required.retain(|entry| entry.as_str() != Some("pageId"));
        }
    }
    value.to_string()
}

fn rewrite_selected_page(line: &str, page_id: u64) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    if let Some(pages) = value
        .get_mut("result")
        .and_then(|result| result.get_mut("structuredContent"))
        .and_then(|structured| structured.get_mut("pages"))
        .and_then(|pages| pages.as_array_mut())
    {
        for page in pages.iter_mut() {
            let is_target = page.get("id").and_then(|id| id.as_u64()) == Some(page_id);
            if let Some(object) = page.as_object_mut() {
                object.insert("selected".to_string(), serde_json::json!(is_target));
            }
        }
    }
    value.to_string()
}

fn collect_raw_uids(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == "uid" {
                    if let Some(raw_uid) = value.as_str() {
                        push_unique(out, raw_uid);
                    }
                } else {
                    collect_raw_uids(value, out);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_raw_uids(item, out);
            }
        }
        serde_json::Value::String(text) => {
            collect_text_uids(text, out);
        }
        _ => {}
    }
}

fn push_unique(out: &mut Vec<String>, value: &str) {
    if !out.iter().any(|item| item == value) {
        out.push(value.to_string());
    }
}

fn collect_text_uids(text: &str, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(index) = rest.find("uid=") {
        let after = &rest[index + 4..];
        let end = after
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, ')' | ']' | '}' | ',' | ';' | '"' | '\'')
            })
            .unwrap_or(after.len());
        if end > 0 {
            push_unique(out, &after[..end]);
        }
        rest = &after[end..];
    }
}

fn replace_raw_uids(value: &mut serde_json::Value, replacements: &HashMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == "uid" {
                    if let Some(raw_uid) = value.as_str() {
                        if let Some(token) = replacements.get(raw_uid) {
                            *value = serde_json::Value::String(token.clone());
                        }
                    }
                } else {
                    replace_raw_uids(value, replacements);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                replace_raw_uids(item, replacements);
            }
        }
        serde_json::Value::String(text) => {
            *text = replace_text_uids(text, replacements);
        }
        _ => {}
    }
}

fn replace_text_uids(text: &str, replacements: &HashMap<String, String>) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(index) = rest.find("uid=") {
        output.push_str(&rest[..index + 4]);
        let after = &rest[index + 4..];
        let end = after
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, ')' | ']' | '}' | ',' | ';' | '"' | '\'')
            })
            .unwrap_or(after.len());
        let raw_uid = &after[..end];
        if let Some(token) = replacements.get(raw_uid) {
            output.push_str(token);
        } else {
            output.push_str(raw_uid);
        }
        rest = &after[end..];
    }
    output.push_str(rest);
    output
}

fn rewrite_uid_response(
    line: &str,
    sessions: &SharedSessions,
    session_id: &str,
    page_id: u64,
) -> Result<String, String> {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(line.to_string());
    };
    let mut raw_uids = Vec::new();
    collect_raw_uids(&value, &mut raw_uids);
    if raw_uids.is_empty() {
        return Ok(line.to_string());
    }
    let replacements = {
        let (lock, _) = &**sessions;
        let mut registry = lock
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        registry.record_snapshot_uids(session_id, page_id, &raw_uids)?
    };
    replace_raw_uids(&mut value, &replacements);
    Ok(value.to_string())
}

fn rewrite_uid_responses(
    lines: Vec<String>,
    sessions: &SharedSessions,
    session_id: &str,
    page_id: u64,
) -> Result<Vec<String>, String> {
    lines
        .into_iter()
        .map(|line| rewrite_uid_response(&line, sessions, session_id, page_id))
        .collect()
}

fn translate_uid_fields(
    arguments: &mut serde_json::Value,
    sessions: &SharedSessions,
    session_id: &str,
    page_id: u64,
) -> Result<(), String> {
    let (lock, _) = &**sessions;
    let registry = lock
        .lock()
        .map_err(|_| "session registry poisoned".to_string())?;
    translate_uid_key(arguments, "uid", &registry, session_id, page_id)?;
    translate_uid_key(arguments, "from_uid", &registry, session_id, page_id)?;
    translate_uid_key(arguments, "to_uid", &registry, session_id, page_id)?;
    if let Some(elements) = arguments
        .get_mut("elements")
        .and_then(|elements| elements.as_array_mut())
    {
        for element in elements {
            translate_uid_key(element, "uid", &registry, session_id, page_id)?;
        }
    }
    if let Some(args) = arguments
        .get_mut("args")
        .and_then(|args| args.as_array_mut())
    {
        for arg in args {
            if let Some(token) = arg.as_str() {
                *arg = serde_json::Value::String(
                    registry.translate_uid_token(session_id, page_id, token)?,
                );
            }
        }
    }
    Ok(())
}

fn translate_uid_key(
    arguments: &mut serde_json::Value,
    key: &str,
    registry: &SessionRegistry,
    session_id: &str,
    page_id: u64,
) -> Result<(), String> {
    if let Some(value) = arguments.get_mut(key) {
        if let Some(token) = value.as_str() {
            *value = serde_json::Value::String(
                registry.translate_uid_token(session_id, page_id, token)?,
            );
        }
    }
    Ok(())
}

fn request_error_line(value: &serde_json::Value, message: &str) -> Vec<String> {
    value
        .get("id")
        .cloned()
        .map(|id| vec![jsonrpc_error_response(id, -32000, message)])
        .unwrap_or_default()
}

fn error_response_for_line(line: &str, message: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .map(|value| request_error_line(&value, message))
        .unwrap_or_default()
}

fn upload_file_needs_fallback(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let text = value
        .get("result")
        .and_then(|result| result.get("content"))
        .and_then(|content| content.as_array())
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(|text| text.as_str())
        .unwrap_or("");
    value
        .get("result")
        .and_then(|result| result.get("isError"))
        .and_then(|is_error| is_error.as_bool())
        == Some(true)
        && text.contains("could not accept the file directly")
        && text.contains("clicking it did not trigger a file chooser")
}

fn upload_file_paths(arguments: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    for key in ["filePath", "path"] {
        if let Some(path) = arguments.get(key).and_then(|value| value.as_str()) {
            paths.push(path.to_string());
        }
    }
    for key in ["filePaths", "files"] {
        if let Some(items) = arguments.get(key).and_then(|value| value.as_array()) {
            paths.extend(
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|item| item.to_string()),
            );
        }
    }
    paths
}

fn upload_file_fallback_response(line: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| value.get("id").cloned())
        .map(|id| {
            vec![
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": "uploaded file via CDP fallback"
                        }],
                        "structuredContent": {}
                    }
                })
                .to_string(),
            ]
        })
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn route_request(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    profile: &Profile,
    sessions: &SharedSessions,
    session_id: Option<&str>,
    line: &str,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<Vec<String>, String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return forward_line(mcp_stdin, mcp_reader, line, next_id, abandoned_ids);
    };
    let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
    if method == "tools/list" {
        let mut lines = forward_line(mcp_stdin, mcp_reader, line, next_id, abandoned_ids)?;
        if let Some(last) = lines.last_mut() {
            *last = strip_page_id_schema(last);
            *last = inject_native_tool_defs(last);
        }
        return Ok(lines);
    }
    if method != "tools/call" {
        return forward_line(mcp_stdin, mcp_reader, line, next_id, abandoned_ids);
    }
    let name = value
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(|name| name.as_str())
        .unwrap_or("")
        .to_string();
    match name.as_str() {
        "new_page" => route_new_page(
            mcp_stdin,
            mcp_reader,
            sessions,
            session_id,
            value,
            next_id,
            abandoned_ids,
        ),
        "list_pages" => {
            let lines = forward_line(mcp_stdin, mcp_reader, line, next_id, abandoned_ids)?;
            Ok(annotate_session_page(
                lines,
                session_id.and_then(|id| session_page(sessions, id)),
            ))
        }
        "select_page" => route_select_page(
            mcp_stdin,
            mcp_reader,
            sessions,
            session_id,
            &value,
            line,
            next_id,
            abandoned_ids,
        ),
        "close_page" => route_close_page(
            mcp_stdin,
            mcp_reader,
            sessions,
            session_id,
            &value,
            next_id,
            abandoned_ids,
        ),
        other if is_native_tool(other) => {
            let Some(session_id) = session_id else {
                return Err("native tool requires a bound session".to_string());
            };
            ensure_session_page(
                mcp_stdin,
                mcp_reader,
                sessions,
                session_id,
                current_port(profile),
                next_id,
                abandoned_ids,
            )?;
            try_native_tool(profile, sessions, Some(session_id), line)
                .ok_or_else(|| format!("failed to route native tool: {other}"))
        }
        other if is_page_scoped_tool(other) => {
            let Some(session_id) = session_id else {
                return Err("page-scoped tool requires a bound session".to_string());
            };
            let page_id = ensure_session_page(
                mcp_stdin,
                mcp_reader,
                sessions,
                session_id,
                current_port(profile),
                next_id,
                abandoned_ids,
            )?;
            let mut forwarded = value;
            let upload_paths = if other == "upload_file" {
                forwarded
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                    .map(upload_file_paths)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            inject_page_id(&mut forwarded, page_id);
            if let Some(arguments) = forwarded
                .get_mut("params")
                .and_then(|params| params.get_mut("arguments"))
            {
                if let Err(message) = translate_uid_fields(arguments, sessions, session_id, page_id)
                {
                    return Ok(request_error_line(&forwarded, &message));
                }
            }
            let navigated_url = if other == "navigate_page" {
                forwarded
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                    .and_then(|arguments| arguments.get("url"))
                    .and_then(|url| url.as_str())
                    .map(|url| url.to_string())
            } else {
                None
            };
            let lines = forward_line(
                mcp_stdin,
                mcp_reader,
                &forwarded.to_string(),
                next_id,
                abandoned_ids,
            )?;
            let lines = if other == "upload_file"
                && lines.last().map(|line| upload_file_needs_fallback(line)) == Some(true)
            {
                match current_port(profile)
                    .ok_or_else(|| "missing DevTools port".to_string())
                    .and_then(|port| {
                        // fallback も session のタブに固定する。target id が取れない
                        // ときは別タブへ渡さず失敗させる (projects メモの session 境界要件)。
                        let url = session_page_url(sessions, session_id);
                        let target_id = session_target_id(sessions, session_id);
                        cdp::set_file_input_files(
                            port,
                            cdp::PageRef::new(target_id.as_deref(), url.as_deref()),
                            &upload_paths,
                        )
                    }) {
                    Ok(()) => {
                        eprintln!("upload_file_fallback session={session_id} page={page_id}");
                        lines
                            .last()
                            .map(|line| upload_file_fallback_response(line))
                            .unwrap_or_default()
                    }
                    Err(error) => {
                        eprintln!(
                            "warning: upload_file_fallback_failed session={session_id} page={page_id} error={error}"
                        );
                        lines
                    }
                }
            } else {
                lines
            };
            if let Some(url) = navigated_url {
                let errored = lines
                    .last()
                    .and_then(|last| serde_json::from_str::<serde_json::Value>(last).ok())
                    .map(|response| {
                        response.get("error").is_some()
                            || response
                                .get("result")
                                .and_then(|result| result.get("isError"))
                                .and_then(|is_error| is_error.as_bool())
                                == Some(true)
                    })
                    .unwrap_or(false);
                if !errored {
                    let _ = record_session_page(
                        sessions,
                        session_id,
                        page_id,
                        session_page_created_by_daemon(sessions, session_id),
                        Some(url),
                    );
                }
            }
            rewrite_uid_responses(lines, sessions, session_id, page_id)
        }
        _ => forward_line(mcp_stdin, mcp_reader, line, next_id, abandoned_ids),
    }
}

fn annotate_session_page(mut lines: Vec<String>, page_id: Option<u64>) -> Vec<String> {
    if let (Some(last), Some(page_id)) = (lines.last_mut(), page_id) {
        *last = rewrite_selected_page(last, page_id);
    }
    lines
}

fn route_new_page(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    sessions: &SharedSessions,
    session_id: Option<&str>,
    value: serde_json::Value,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<Vec<String>, String> {
    let mut forwarded = value;
    let url = forwarded
        .get("params")
        .and_then(|params| params.get("arguments"))
        .and_then(|arguments| arguments.get("url"))
        .and_then(|url| url.as_str())
        .map(|url| url.to_string());
    if let Some(arguments) = forwarded
        .get_mut("params")
        .and_then(|params| params.as_object_mut())
        .map(|params| {
            params
                .entry("arguments")
                .or_insert_with(|| serde_json::json!({}))
        })
        .and_then(|arguments| arguments.as_object_mut())
    {
        arguments
            .entry("background".to_string())
            .or_insert_with(|| serde_json::json!(true));
    }
    let lines = forward_line(
        mcp_stdin,
        mcp_reader,
        &forwarded.to_string(),
        next_id,
        abandoned_ids,
    )?;
    if let (Some(session_id), Some(last)) = (session_id, lines.last()) {
        if let Ok(response) = serde_json::from_str::<serde_json::Value>(last) {
            if let Some(page_id) = selected_page_id(&response) {
                let _ = record_session_page(sessions, session_id, page_id, true, url);
            }
        }
    }
    Ok(lines)
}

#[allow(clippy::too_many_arguments)]
fn route_select_page(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    sessions: &SharedSessions,
    session_id: Option<&str>,
    value: &serde_json::Value,
    line: &str,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<Vec<String>, String> {
    let requested = value
        .get("params")
        .and_then(|params| params.get("arguments"))
        .and_then(|arguments| arguments.get("pageId"))
        .and_then(|page_id| page_id.as_u64());
    let lines = forward_line(mcp_stdin, mcp_reader, line, next_id, abandoned_ids)?;
    if let (Some(session_id), Some(page_id)) = (session_id, requested) {
        let errored = lines
            .last()
            .and_then(|last| serde_json::from_str::<serde_json::Value>(last).ok())
            .map(|response| response.get("error").is_some())
            .unwrap_or(false);
        if !errored {
            let _ = record_session_page(sessions, session_id, page_id, false, None);
        }
    }
    Ok(lines)
}

fn route_close_page(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    sessions: &SharedSessions,
    session_id: Option<&str>,
    value: &serde_json::Value,
    next_id: &mut u64,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<Vec<String>, String> {
    let mut forwarded = value.clone();
    let requested = forwarded
        .get("params")
        .and_then(|params| params.get("arguments"))
        .and_then(|arguments| arguments.get("pageId"))
        .and_then(|page_id| page_id.as_u64());
    let page_id = match (session_id, requested) {
        (_, Some(page_id)) => Some(page_id),
        (Some(session_id), None) => {
            let page_id = ensure_session_page(
                mcp_stdin,
                mcp_reader,
                sessions,
                session_id,
                None,
                next_id,
                abandoned_ids,
            )?;
            inject_page_id(&mut forwarded, page_id);
            Some(page_id)
        }
        _ => None,
    };
    let lines = forward_line(
        mcp_stdin,
        mcp_reader,
        &forwarded.to_string(),
        next_id,
        abandoned_ids,
    )?;
    if let (Some(session_id), Some(page_id)) = (session_id, page_id) {
        if session_page(sessions, session_id) == Some(page_id) {
            let (lock, _) = &**sessions;
            if let Ok(mut registry) = lock.lock() {
                registry.clear_page(session_id);
            }
        }
    }
    Ok(lines)
}

pub(crate) struct BoundSessionGuard<'a> {
    sessions: &'a SharedSessions,
    id: Option<String>,
    bound_at: Instant,
}

impl<'a> BoundSessionGuard<'a> {
    fn new(sessions: &'a SharedSessions) -> Self {
        Self {
            sessions,
            id: None,
            bound_at: Instant::now(),
        }
    }

    fn mark_bound(&mut self, id: String) {
        self.id = Some(id);
        self.bound_at = Instant::now();
    }
}

impl Drop for BoundSessionGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            eprintln!(
                "bind session={id} held_ms={}",
                self.bound_at.elapsed().as_millis()
            );
            let (lock, cvar) = &**self.sessions;
            if let Ok(mut registry) = lock.lock() {
                registry.unbind(&id);
                cvar.notify_all();
            }
        }
    }
}

fn client_disconnected(stream: &UnixStream) -> Result<bool, DaemonError> {
    let mut fd = PollFd {
        fd: stream.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    let ready = unsafe { poll(&mut fd, 1, 0) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        Err(DaemonError::Client(format!(
            "failed to inspect daemon client connection: {error}"
        )))
    } else {
        Ok(fd.revents & POLLHUP != 0)
    }
}

fn wait_forward_response(
    stream: &UnixStream,
    response: mpsc::Receiver<Result<Vec<String>, String>>,
) -> Result<Vec<String>, DaemonError> {
    loop {
        match response.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => return result.map_err(DaemonError::Fatal),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if client_disconnected(stream)? {
                    return Err(DaemonError::Client(
                        "daemon client disconnected during MCP forward".to_string(),
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(DaemonError::Fatal(
                    "failed to receive MCP response: channel disconnected".to_string(),
                ));
            }
        }
    }
}

pub(crate) fn handle_daemon_client(
    mut stream: UnixStream,
    router: RouterHandle,
    sessions: &SharedSessions,
    profile: &Profile,
) -> Result<bool, DaemonError> {
    let mut client_reader = BufReader::new(stream.try_clone().map_err(|error| {
        DaemonError::Client(format!("failed to clone daemon client stream: {error}"))
    })?);
    let mut line = String::new();
    let mut bound = BoundSessionGuard::new(sessions);

    loop {
        line.clear();
        let bytes = client_reader.read_line(&mut line).map_err(|error| {
            DaemonError::Client(format!("failed to read daemon client request: {error}"))
        })?;
        if bytes == 0 {
            return Ok(false);
        }

        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }

        if let Some(command) = line.strip_prefix("__chrome_devtools_daemon__:") {
            let started_at = Instant::now();
            let name = control_label(command);
            let outcome = handle_control_command(
                &mut stream,
                &router,
                sessions,
                &mut bound,
                command,
                profile,
            );
            router.record_control_latency(&name, started_at.elapsed());
            match outcome? {
                ControlOutcome::Continue => continue,
                ControlOutcome::CloseConnection => return Ok(false),
                ControlOutcome::StopDaemon => return Ok(true),
            }
        }

        if json_has_method(line, "initialize") {
            if let Some(id) = extract_jsonrpc_id_value(line) {
                let response = daemon_initialize_response(id);
                stream
                    .write_all(response.as_bytes())
                    .and_then(|_| stream.write_all(b"\n"))
                    .and_then(|_| stream.flush())
                    .map_err(|error| {
                        DaemonError::Client(format!(
                            "failed to write daemon initialize response: {error}"
                        ))
                    })?;
            }
            continue;
        }
        if json_has_method(line, "notifications/initialized") {
            continue;
        }

        let forwarded = sanitize_outgoing_request(line);
        if bound.id.is_none() && !json_has_method(&forwarded, "tools/list") {
            if let Some(id) = extract_jsonrpc_id_value(&forwarded) {
                let response =
                    jsonrpc_error_response(id, -32000, "session bind required for MCP forwarding");
                stream
                    .write_all(response.as_bytes())
                    .and_then(|_| stream.write_all(b"\n"))
                    .and_then(|_| stream.flush())
                    .map_err(|error| {
                        DaemonError::Client(format!(
                            "failed to write daemon client response: {error}"
                        ))
                    })?;
            }
            continue;
        }
        // daemon-native tool は MCP を経由せず接続スレッド上で CDP 直実行する。
        // 重い tool が MCP の直列キューを塞いでいても native tool は並行して動く。
        if let Some(lines) = try_native_tool(profile, sessions, bound.id.as_deref(), &forwarded) {
            for response_line in lines {
                stream
                    .write_all(response_line.as_bytes())
                    .and_then(|_| stream.write_all(b"\n"))
                    .and_then(|_| stream.flush())
                    .map_err(|error| {
                        DaemonError::Client(format!(
                            "failed to write daemon client response: {error}"
                        ))
                    })?;
            }
            if let Some(id) = bound.id.as_ref() {
                let (lock, _) = &**sessions;
                if let Ok(mut registry) = lock.lock() {
                    registry.touch(id);
                }
            }
            continue;
        }

        let response = router.enqueue_forward(bound.id.as_deref(), &forwarded)?;
        for response_line in wait_forward_response(&stream, response)? {
            stream
                .write_all(response_line.as_bytes())
                .and_then(|_| stream.write_all(b"\n"))
                .and_then(|_| stream.flush())
                .map_err(|error| {
                    DaemonError::Client(format!("failed to write daemon client response: {error}"))
                })?;
            if let Some(id) = bound.id.as_ref() {
                let (lock, _) = &**sessions;
                if let Ok(mut registry) = lock.lock() {
                    registry.touch(id);
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn read_mcp_response_line(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
) -> Result<String, DaemonError> {
    read_mcp_response_line_until(
        mcp_stdin,
        mcp_reader,
        Instant::now() + mcp_request_timeout(),
    )
}

pub(crate) fn read_mcp_response_line_until(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    deadline: Instant,
) -> Result<String, DaemonError> {
    // 応答行 (数 MB になりうる) は複数回の read に跨る。WouldBlock を跨いでも
    // 読みかけの部分行を失わないよう、バッファは retry ループの外で保持する。
    let mut response_line = String::new();
    loop {
        let bytes = match mcp_reader.read_line(&mut response_line) {
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(DaemonError::Fatal("MCP request timed out".to_string()));
                }
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => {
                return Err(DaemonError::Fatal(format!(
                    "failed to read MCP response: {error}"
                )));
            }
        };
        if bytes == 0 {
            return Err(DaemonError::Fatal(
                "chrome-devtools-mcp closed stdout before responding".to_string(),
            ));
        }
        if !response_line.ends_with('\n') {
            // 改行なしで Ok が返るのは EOF 直前のみ。次の read で EOF (bytes == 0) を検出する。
            continue;
        }
        let line = response_line.trim_end().to_string();
        response_line.clear();
        if json_has_method(&line, "roots/list") {
            if let Some(id) = extract_jsonrpc_id(&line) {
                write_json_line(mcp_stdin, &roots_list_response(id)).map_err(DaemonError::Fatal)?;
            }
            continue;
        }
        return Ok(line);
    }
}

fn read_mcp_response_line_until_ignoring(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    deadline: Instant,
    abandoned_ids: &mut HashMap<u64, Instant>,
) -> Result<String, DaemonError> {
    loop {
        let response_line = read_mcp_response_line_until(mcp_stdin, mcp_reader, deadline)?;
        if let Some(id) = extract_jsonrpc_id(&response_line) {
            if let Some(abandoned_at) = abandoned_ids.remove(&id) {
                eprintln!(
                    "abandoned_response internal_id={id} arrived_after_timeout_ms={} is_error={}",
                    abandoned_at.elapsed().as_millis(),
                    response_line.contains("\"isError\":true")
                        || response_line.contains("\"error\"")
                );
                continue;
            }
        }
        return Ok(response_line);
    }
}

#[cfg(test)]
pub(crate) fn drain_pending_mcp_response(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    pending_id: u64,
    already_read_line: &str,
) -> Result<(), DaemonError> {
    if extract_jsonrpc_id(already_read_line) == Some(pending_id) {
        return Ok(());
    }
    loop {
        let response_line = read_mcp_response_line(mcp_stdin, mcp_reader)?;
        if extract_jsonrpc_id(&response_line) == Some(pending_id) {
            return Ok(());
        }
    }
}

pub(crate) enum ControlOutcome {
    Continue,
    CloseConnection,
    StopDaemon,
}

fn control_label(command: &str) -> String {
    command.split_whitespace().next().unwrap_or("").to_string()
}

pub(crate) fn handle_control_command(
    stream: &mut UnixStream,
    router: &RouterHandle,
    sessions: &SharedSessions,
    bound: &mut BoundSessionGuard,
    command: &str,
    profile: &Profile,
) -> Result<ControlOutcome, DaemonError> {
    let (head, rest) = match command.split_once(' ') {
        Some((head, rest)) => (head, rest.trim()),
        None => (command, ""),
    };
    match head {
        "status" => {
            let snapshot = lock_sessions(sessions)?.list();
            let diagnostics = router.diagnostics_snapshot();
            let count = snapshot.len();
            let active = snapshot.iter().filter(|session| session.owned).count();
            let pages = snapshot
                .iter()
                .filter_map(|session| session.page_id.map(|page| page.to_string()))
                .collect::<Vec<_>>()
                .join(",");
            write_control_line(
                stream,
                &format!(
                    "daemon=ready version={} sessions={count} active_sessions={active} pages={pages} queued_mcp_requests={} max_control_latency_ms={} max_forward_latency_ms={} diagnostic_window_secs={} mcp_port={mcp_port}",
                    env!("CARGO_PKG_VERSION"),
                    router.queued_requests(),
                    diagnostics.max_control_latency_ms,
                    diagnostics.max_forward_latency_ms,
                    diagnostics.window_secs,
                    mcp_port = current_port(profile).unwrap_or_default()
                ),
            )?;
            Ok(ControlOutcome::CloseConnection)
        }
        "stop" => {
            if rest != "force" {
                let count = lock_sessions(sessions)?.list().len();
                if count > 0 {
                    write_control_line(
                        stream,
                        &format!(
                            "error={count} active session(s); other agents may be using this daemon, pass --force to stop anyway"
                        ),
                    )?;
                    return Ok(ControlOutcome::CloseConnection);
                }
            }
            write_control_line(stream, "daemon=stopping")?;
            Ok(ControlOutcome::StopDaemon)
        }
        "session_create" => {
            let state = lock_sessions(sessions)?.create();
            write_control_line(stream, &format_session_line(&state))?;
            Ok(ControlOutcome::CloseConnection)
        }
        "session_list" => {
            let snapshot = lock_sessions(sessions)?.list();
            for state in &snapshot {
                write_control_line(stream, &format_session_line(state))?;
            }
            Ok(ControlOutcome::CloseConnection)
        }
        "session_close" => {
            let id = parse_session_arg(rest).map_err(DaemonError::Client)?;
            let result = lock_sessions(sessions)?.close(&id);
            match result {
                Ok(cleanup) => {
                    if let Some(cleanup) = cleanup {
                        router.close_pages(vec![cleanup])?;
                    }
                    write_control_line(stream, &format!("closed={id}"))?;
                }
                Err(message) => write_control_line(stream, &format!("error={message}"))?,
            }
            Ok(ControlOutcome::CloseConnection)
        }
        "session_page" => {
            let id = parse_session_arg(rest).map_err(DaemonError::Client)?;
            let snapshot = lock_sessions(sessions)?;
            let page = snapshot
                .sessions
                .get(&id)
                .ok_or_else(|| DaemonError::Client(format!("unknown session: {id}")))?
                .page_id
                .map(|page_id| page_id.to_string())
                .unwrap_or_default();
            write_control_line(stream, &format!("session={id} page={page}"))?;
            Ok(ControlOutcome::CloseConnection)
        }
        "session_attach" => {
            let id = parse_session_arg(rest).map_err(DaemonError::Client)?;
            let page_id = parse_page_arg(rest).map_err(DaemonError::Client)?;
            let result = lock_sessions(sessions)?.set_page(&id, page_id, false, None);
            match result {
                Ok(()) => write_control_line(stream, &format!("session={id} page={page_id}"))?,
                Err(message) => write_control_line(stream, &format!("error={message}"))?,
            }
            Ok(ControlOutcome::CloseConnection)
        }
        "bind" => {
            let id = parse_session_arg(rest).map_err(DaemonError::Client)?;
            let result = bind_session_in_registry(sessions, &id);
            match result {
                Ok(()) => {
                    bound.mark_bound(id.clone());
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

pub(crate) fn lock_sessions<'a>(
    sessions: &'a SharedSessions,
) -> Result<std::sync::MutexGuard<'a, SessionRegistry>, DaemonError> {
    let (lock, _) = &**sessions;
    lock.lock()
        .map_err(|_| DaemonError::Fatal("session registry poisoned".to_string()))
}

pub(crate) fn bind_session_in_registry(sessions: &SharedSessions, id: &str) -> Result<(), String> {
    let (lock, _) = &**sessions;
    let mut registry = lock
        .lock()
        .map_err(|_| "session registry poisoned".to_string())?;
    registry.bind(id)
}

pub(crate) fn write_control_line(stream: &mut UnixStream, body: &str) -> Result<(), DaemonError> {
    stream
        .write_all(body.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|error| DaemonError::Client(format!("failed to write daemon response: {error}")))
}

pub(crate) fn format_session_line(state: &SessionState) -> String {
    let page = state
        .page_id
        .map(|page_id| page_id.to_string())
        .unwrap_or_default();
    let page_url = state.page_url.as_deref().unwrap_or("");
    let target_id = state.target_id.as_deref().unwrap_or("");
    format!(
        "session={} created={} last_used={} owned={} page={} page_created_by_daemon={} page_url={} target_id={} snapshot_epoch={}",
        state.id,
        unix_secs(state.created_at),
        unix_secs(state.last_used_at),
        state.owned,
        page,
        state.page_created_by_daemon,
        page_url,
        target_id,
        state.snapshot_epoch
    )
}

pub(crate) fn parse_session_arg(args: &str) -> Result<String, String> {
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

pub(crate) fn parse_page_arg(args: &str) -> Result<u64, String> {
    for part in args.split_whitespace() {
        if let Some(value) = part.strip_prefix("page=") {
            return value
                .parse::<u64>()
                .map_err(|_| "page id must be an integer".to_string());
        }
    }
    Err("missing page=<id> argument".to_string())
}

pub(crate) fn write_json_line(stdin: &mut impl Write, json: &str) -> Result<(), String> {
    stdin
        .write_all(json.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("failed to write MCP request: {error}"))
}

pub(crate) fn read_response(
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
                write_json_line(stdin, &roots_list_response(id))?;
            }
            continue;
        }

        if extract_jsonrpc_id(&line) == Some(target_id) {
            return Ok(line);
        }
    }
}

pub(crate) fn roots_list_response(id: impl Into<serde_json::Value>) -> String {
    let home = env::var("HOME").unwrap_or_default();
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.into(),
        "result": {
            "roots": [
                {
                    "uri": format!("file://{home}"),
                    "name": "home"
                },
                {
                    "uri": "file:///tmp",
                    "name": "tmp"
                }
            ]
        }
    })
    .to_string()
}

pub(crate) fn daemon_initialize_response(id: impl Into<serde_json::Value>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.into(),
        "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "serverInfo": {
                "name": "chrome-devtools-daemon",
                "version": "0.1.0"
            }
        }
    })
    .to_string()
}

pub(crate) fn jsonrpc_error_response(
    id: impl Into<serde_json::Value>,
    code: i64,
    message: &str,
) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.into(),
        "error": {
            "code": code,
            "message": message
        }
    })
    .to_string()
}

pub(crate) fn sanitize_outgoing_request(line: &str) -> String {
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

pub(crate) fn json_method_starts_with(line: &str, prefix: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(|found| found.as_str())
                .map(|found| found.starts_with(prefix))
        })
        .unwrap_or(false)
}

pub(crate) fn json_has_method(line: &str, method: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(|found| found.as_str())
                .map(|found| found == method)
        })
        .unwrap_or(false)
}

pub(crate) fn extract_jsonrpc_id(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("id")?
        .as_u64()
}

pub(crate) fn extract_jsonrpc_id_value(line: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("id")
        .cloned()
}

pub(crate) fn rewrite_jsonrpc_id(line: &str, id: serde_json::Value) -> Result<String, String> {
    let mut value = serde_json::from_str::<serde_json::Value>(line)
        .map_err(|error| format!("failed to parse JSON-RPC message: {error}"))?;
    let Some(object) = value.as_object_mut() else {
        return Err("JSON-RPC message must be an object".to_string());
    };
    object.insert("id".to_string(), id);
    Ok(value.to_string())
}
