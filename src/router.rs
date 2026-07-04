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
}

#[derive(Default)]
pub(crate) struct SessionRegistry {
    pub(crate) sessions: HashMap<String, SessionState>,
    pub(crate) bound_session: Option<String>,
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
        if self.bound_session.is_some() {
            return Err("daemon busy: another session is bound".to_string());
        }
        session.owned = true;
        session.last_used_at = SystemTime::now();
        self.bound_session = Some(id.to_string());
        Ok(())
    }

    pub(crate) fn unbind(&mut self, id: &str) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.owned = false;
            session.last_used_at = SystemTime::now();
        }
        if self.bound_session.as_deref() == Some(id) {
            self.bound_session = None;
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
    response: mpsc::Sender<Result<Vec<String>, String>>,
}

impl RouterHandle {
    pub(crate) fn start(
        mut mcp_stdin: impl Write + Send + 'static,
        mut mcp_reader: impl BufRead + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<RouterRequest>();
        thread::spawn(move || {
            let mut next_id: u64 = 10_000;
            for request in receiver {
                let result =
                    route_mcp_request(&mut mcp_stdin, &mut mcp_reader, &request.line, &mut next_id);
                let _ = request.response.send(result);
            }
        });
        Self { sender }
    }

    pub(crate) fn forward(&self, line: &str) -> Result<Vec<String>, DaemonError> {
        let (response_tx, response_rx) = mpsc::channel();
        self.sender
            .send(RouterRequest {
                line: line.to_string(),
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

pub(crate) fn route_mcp_request(
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
        for response_line in router.forward(&forwarded)? {
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
    timeout: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    let (lock, cvar) = &**sessions;
    let mut registry = lock
        .lock()
        .map_err(|_| "session registry poisoned".to_string())?;
    loop {
        match registry.bind(id) {
            Ok(()) => return Ok(()),
            Err(message) if message.starts_with("daemon busy") => {
                let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                    return Err(format!(
                        "daemon busy: bind deadline exceeded after {}s",
                        timeout.as_secs()
                    ));
                };
                let wait = cvar
                    .wait_timeout(registry, remaining)
                    .map_err(|_| "session registry poisoned".to_string())?;
                registry = wait.0;
                if wait.1.timed_out() {
                    return Err(format!(
                        "daemon busy: bind deadline exceeded after {}s",
                        timeout.as_secs()
                    ));
                }
            }
            Err(message) => return Err(message),
        }
    }
}

pub(crate) fn write_control_line(stream: &mut UnixStream, body: &str) -> Result<(), DaemonError> {
    stream
        .write_all(body.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|error| DaemonError::Client(format!("failed to write daemon response: {error}")))
}

pub(crate) fn format_session_line(state: &SessionState) -> String {
    format!(
        "session={} created={} last_used={} owned={}",
        state.id,
        unix_secs(state.created_at),
        unix_secs(state.last_used_at),
        state.owned
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
