use std::collections::HashMap;
use std::env;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(crate) const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

pub(crate) const SESSION_REAPER_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub(crate) struct SessionState {
    pub(crate) id: String,
    pub(crate) created_at: SystemTime,
    pub(crate) last_used_at: SystemTime,
    pub(crate) owned: bool,
    pub(crate) page_id: Option<u64>,
    pub(crate) page_created_by_daemon: bool,
    pub(crate) page_url: Option<String>,
    pub(crate) snapshot_epoch: u64,
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
            snapshot_epoch: 0,
        };
        self.sessions.insert(id.clone(), state.clone());
        state
    }

    pub(crate) fn list(&self) -> Vec<SessionState> {
        let mut sessions = self.sessions.values().cloned().collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.created_at);
        sessions
    }

    pub(crate) fn close(&mut self, id: &str) -> Result<(), String> {
        let Some(session) = self.sessions.get(id) else {
            return Err(format!("unknown session: {id}"));
        };
        if session.owned {
            return Err(format!("session in use: {id}"));
        }
        self.sessions.remove(id);
        Ok(())
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

    pub(crate) fn reap_expired(&mut self) {
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
        session.page_id = Some(page_id);
        session.page_created_by_daemon = page_created_by_daemon;
        session.page_url = page_url;
        session.last_used_at = SystemTime::now();
        Ok(())
    }

    pub(crate) fn clear_page(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.page_id = None;
            session.page_created_by_daemon = false;
            session.page_url = None;
            session.last_used_at = SystemTime::now();
        }
    }
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

#[derive(Debug)]
pub(crate) enum DaemonError {
    Client(String),
    Fatal(String),
}

pub(crate) type SharedSessions = Arc<(Mutex<SessionRegistry>, Condvar)>;

#[derive(Clone)]
pub(crate) struct RouterHandle {
    sender: mpsc::Sender<RouterRequest>,
}

pub(crate) struct RouterRequest {
    line: String,
    session_id: Option<String>,
    response: mpsc::Sender<Result<Vec<String>, String>>,
}

impl RouterHandle {
    pub(crate) fn start(
        mut mcp_stdin: impl Write + Send + 'static,
        mut mcp_reader: impl BufRead + Send + 'static,
        sessions: SharedSessions,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<RouterRequest>();
        thread::spawn(move || {
            let mut next_id: u64 = 10_000;
            for request in receiver {
                let result = route_request(
                    &mut mcp_stdin,
                    &mut mcp_reader,
                    &sessions,
                    request.session_id.as_deref(),
                    &request.line,
                    &mut next_id,
                );
                let _ = request.response.send(result);
            }
        });
        Self { sender }
    }

    pub(crate) fn forward(
        &self,
        session_id: Option<&str>,
        line: &str,
    ) -> Result<Vec<String>, DaemonError> {
        let (response_tx, response_rx) = mpsc::channel();
        self.sender
            .send(RouterRequest {
                line: line.to_string(),
                session_id: session_id.map(|id| id.to_string()),
                response: response_tx,
            })
            .map_err(|error| DaemonError::Fatal(format!("failed to queue MCP request: {error}")))?;
        response_rx
            .recv()
            .map_err(|error| {
                DaemonError::Fatal(format!("failed to receive MCP response: {error}"))
            })?
            .map_err(DaemonError::Fatal)
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
    loop {
        let response_line =
            read_mcp_response_line(mcp_stdin, mcp_reader).map_err(|error| format!("{error:?}"))?;
        if extract_jsonrpc_id(&response_line) == Some(internal_id) {
            lines.push(rewrite_jsonrpc_id(&response_line, original_id.clone())?);
            return Ok(lines);
        }
        lines.push(response_line);
    }
}

fn mcp_call(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    next_id: &mut u64,
    mut request: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let internal_id = *next_id;
    *next_id = next_id.wrapping_add(1);
    request["jsonrpc"] = serde_json::json!("2.0");
    request["id"] = serde_json::json!(internal_id);
    write_json_line(mcp_stdin, &request.to_string())?;
    loop {
        let response_line =
            read_mcp_response_line(mcp_stdin, mcp_reader).map_err(|error| format!("{error:?}"))?;
        if extract_jsonrpc_id(&response_line) == Some(internal_id) {
            return serde_json::from_str(&response_line)
                .map_err(|error| format!("failed to parse MCP response: {error}"));
        }
    }
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

fn ensure_session_page(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    sessions: &SharedSessions,
    session_id: &str,
    next_id: &mut u64,
) -> Result<u64, String> {
    {
        let (lock, _) = &**sessions;
        let registry = lock
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        if let Some(page_id) = registry.page_id(session_id) {
            return Ok(page_id);
        }
    }
    let response = mcp_call(
        mcp_stdin,
        mcp_reader,
        next_id,
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
    let (lock, _) = &**sessions;
    let mut registry = lock
        .lock()
        .map_err(|_| "session registry poisoned".to_string())?;
    registry.set_page(session_id, page_id, true, Some("about:blank".to_string()))?;
    Ok(page_id)
}

fn session_page(sessions: &SharedSessions, session_id: &str) -> Option<u64> {
    let (lock, _) = &**sessions;
    lock.lock()
        .ok()
        .and_then(|registry| registry.page_id(session_id))
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

#[allow(clippy::too_many_arguments)]
fn route_request(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    sessions: &SharedSessions,
    session_id: Option<&str>,
    line: &str,
    next_id: &mut u64,
) -> Result<Vec<String>, String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return forward_line(mcp_stdin, mcp_reader, line, next_id);
    };
    let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
    if method == "tools/list" {
        let mut lines = forward_line(mcp_stdin, mcp_reader, line, next_id)?;
        if let Some(last) = lines.last_mut() {
            *last = strip_page_id_schema(last);
        }
        return Ok(lines);
    }
    if method != "tools/call" {
        return forward_line(mcp_stdin, mcp_reader, line, next_id);
    }
    let name = value
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(|name| name.as_str())
        .unwrap_or("")
        .to_string();
    match name.as_str() {
        "new_page" => route_new_page(mcp_stdin, mcp_reader, sessions, session_id, value, next_id),
        "list_pages" => {
            let lines = forward_line(mcp_stdin, mcp_reader, line, next_id)?;
            Ok(annotate_session_page(
                lines,
                session_id.and_then(|id| session_page(sessions, id)),
            ))
        }
        "select_page" => route_select_page(
            mcp_stdin, mcp_reader, sessions, session_id, &value, line, next_id,
        ),
        "close_page" => {
            route_close_page(mcp_stdin, mcp_reader, sessions, session_id, &value, next_id)
        }
        other if is_page_scoped_tool(other) => {
            let Some(session_id) = session_id else {
                return Err("page-scoped tool requires a bound session".to_string());
            };
            let page_id =
                ensure_session_page(mcp_stdin, mcp_reader, sessions, session_id, next_id)?;
            let mut forwarded = value;
            inject_page_id(&mut forwarded, page_id);
            forward_line(mcp_stdin, mcp_reader, &forwarded.to_string(), next_id)
        }
        _ => forward_line(mcp_stdin, mcp_reader, line, next_id),
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
    let lines = forward_line(mcp_stdin, mcp_reader, &forwarded.to_string(), next_id)?;
    if let (Some(session_id), Some(last)) = (session_id, lines.last()) {
        if let Ok(response) = serde_json::from_str::<serde_json::Value>(last) {
            if let Some(page_id) = selected_page_id(&response) {
                let _ = record_session_page(sessions, session_id, page_id, true, url);
            }
        }
    }
    Ok(lines)
}

fn route_select_page(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
    sessions: &SharedSessions,
    session_id: Option<&str>,
    value: &serde_json::Value,
    line: &str,
    next_id: &mut u64,
) -> Result<Vec<String>, String> {
    let requested = value
        .get("params")
        .and_then(|params| params.get("arguments"))
        .and_then(|arguments| arguments.get("pageId"))
        .and_then(|page_id| page_id.as_u64());
    let lines = forward_line(mcp_stdin, mcp_reader, line, next_id)?;
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
            let page_id =
                ensure_session_page(mcp_stdin, mcp_reader, sessions, session_id, next_id)?;
            inject_page_id(&mut forwarded, page_id);
            Some(page_id)
        }
        _ => None,
    };
    let lines = forward_line(mcp_stdin, mcp_reader, &forwarded.to_string(), next_id)?;
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

pub(crate) fn handle_daemon_client(
    mut stream: UnixStream,
    router: RouterHandle,
    sessions: &SharedSessions,
    mcp_port: u16,
    bind_timeout: Duration,
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
            match handle_control_command(
                &mut stream,
                sessions,
                &mut bound,
                command,
                mcp_port,
                bind_timeout,
            )? {
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
        for response_line in router.forward(bound.id.as_deref(), &forwarded)? {
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

pub(crate) fn read_mcp_response_line(
    mcp_stdin: &mut impl Write,
    mcp_reader: &mut impl BufRead,
) -> Result<String, DaemonError> {
    loop {
        let mut response_line = String::new();
        let bytes = mcp_reader
            .read_line(&mut response_line)
            .map_err(|error| DaemonError::Fatal(format!("failed to read MCP response: {error}")))?;
        if bytes == 0 {
            return Err(DaemonError::Fatal(
                "chrome-devtools-mcp closed stdout before responding".to_string(),
            ));
        }
        let response_line = response_line.trim_end().to_string();
        if json_has_method(&response_line, "roots/list") {
            if let Some(id) = extract_jsonrpc_id(&response_line) {
                write_json_line(mcp_stdin, &roots_list_response(id)).map_err(DaemonError::Fatal)?;
            }
            continue;
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

pub(crate) fn handle_control_command(
    stream: &mut UnixStream,
    sessions: &SharedSessions,
    bound: &mut BoundSessionGuard,
    command: &str,
    mcp_port: u16,
    bind_timeout: Duration,
) -> Result<ControlOutcome, DaemonError> {
    let (head, rest) = match command.split_once(' ') {
        Some((head, rest)) => (head, rest.trim()),
        None => (command, ""),
    };
    match head {
        "status" => {
            let count = lock_sessions(sessions)?.list().len();
            write_control_line(
                stream,
                &format!(
                    "daemon=ready version={} sessions={count} mcp_port={mcp_port}",
                    env!("CARGO_PKG_VERSION")
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
                Ok(()) => write_control_line(stream, &format!("closed={id}"))?,
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
            let result = bind_session_in_registry(sessions, &id, bind_timeout);
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

pub(crate) fn bind_session_in_registry(
    sessions: &SharedSessions,
    id: &str,
    _timeout: Duration,
) -> Result<(), String> {
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
    format!(
        "session={} created={} last_used={} owned={} page={} page_created_by_daemon={} page_url={} snapshot_epoch={}",
        state.id,
        unix_secs(state.created_at),
        unix_secs(state.last_used_at),
        state.owned,
        page,
        state.page_created_by_daemon,
        page_url,
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
            "roots": [{
                "uri": format!("file://{home}"),
                "name": "home"
            }]
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
