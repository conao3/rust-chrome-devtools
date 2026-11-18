//! Keep the user's foreground window when Chrome raises a window of its own.
//!
//! On X11 the daemon watches the root window's `_NET_ACTIVE_WINDOW` and `_NET_CLIENT_LIST`
//! through `xprop -spy`. When a window of this profile's Chrome becomes active within
//! `NEW_WINDOW_GRACE` of first appearing (Chrome launch, a page-opened popup, a foreground
//! `Target.createTarget`), or while an input tool is running (Chrome activates a window whose
//! active tab receives a click), the guard re-activates the window that was active before it with
//! `xdotool windowactivate`. Activating an existing Chrome window at any other time (the user
//! clicking it) is left alone.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{Profile, expand_home};

pub(crate) const NEW_WINDOW_GRACE: Duration = Duration::from_secs(5);

/// How long after an input tool returns a Chrome activation is still attributed to it.
pub(crate) const INPUT_GRACE_MS: u64 = 1_000;

static INPUT_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static INPUT_LAST_END_MS: AtomicU64 = AtomicU64::new(0);

/// Tools that send input to a page. Chrome activates the window when the session page is the
/// active tab of its window and receives a click, so activations during these calls are restored.
pub(crate) fn is_input_tool(name: &str) -> bool {
    matches!(
        name,
        "click_at"
            | "click_selector"
            | "click_text"
            | "dispatch_key"
            | "type_into"
            | "type_text"
            | "click"
            | "fill"
            | "fill_form"
            | "press_key"
            | "hover"
            | "drag"
            | "upload_file"
            | "handle_dialog"
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Marks an input tool call in flight for the guard.
pub(crate) struct InputScope;

impl InputScope {
    pub(crate) fn begin() -> Self {
        INPUT_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        InputScope
    }
}

impl Drop for InputScope {
    fn drop(&mut self) {
        INPUT_LAST_END_MS.store(now_ms(), Ordering::SeqCst);
        INPUT_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

fn input_active() -> bool {
    INPUT_IN_FLIGHT.load(Ordering::SeqCst) > 0
        || now_ms().saturating_sub(INPUT_LAST_END_MS.load(Ordering::SeqCst)) <= INPUT_GRACE_MS
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SpyEvent {
    Active(Option<u32>),
    ClientList(Vec<u32>),
}

pub(crate) fn parse_spy_line(line: &str) -> Option<SpyEvent> {
    let (name, rest) = line.split_once(':')?;
    let ids = rest
        .split_once('#')
        .map(|(_, ids)| ids)
        .unwrap_or("")
        .split(',')
        .filter_map(|token| parse_window_id(token.trim()))
        .collect::<Vec<_>>();
    match name.split('(').next()?.trim() {
        "_NET_ACTIVE_WINDOW" => Some(SpyEvent::Active(ids.first().copied().filter(|id| *id != 0))),
        "_NET_CLIENT_LIST" => Some(SpyEvent::ClientList(ids)),
        _ => None,
    }
}

fn parse_window_id(token: &str) -> Option<u32> {
    u32::from_str_radix(token.strip_prefix("0x")?, 16).ok()
}

#[derive(Default)]
pub(crate) struct GuardState {
    first_seen: HashMap<u32, Instant>,
    known: HashSet<u32>,
    initialized: bool,
    last_external: Option<u32>,
}

impl GuardState {
    /// Feed one event. Returns the window to re-activate, if any.
    pub(crate) fn on_event(
        &mut self,
        event: SpyEvent,
        now: Instant,
        agent_input: bool,
        is_profile_window: &mut dyn FnMut(u32) -> bool,
    ) -> Option<u32> {
        match event {
            SpyEvent::ClientList(ids) => {
                for id in &ids {
                    if self.known.insert(*id) && self.initialized {
                        self.first_seen.insert(*id, now);
                    }
                }
                self.initialized = true;
                let current: HashSet<u32> = ids.into_iter().collect();
                self.known.retain(|id| current.contains(id));
                self.first_seen.retain(|id, _| current.contains(id));
                None
            }
            SpyEvent::Active(None) => None,
            SpyEvent::Active(Some(window)) => {
                if !is_profile_window(window) {
                    self.last_external = Some(window);
                    return None;
                }
                let fresh = self
                    .first_seen
                    .get(&window)
                    .map(|seen| now.duration_since(*seen) <= NEW_WINDOW_GRACE)
                    // A window that is active before it shows up in _NET_CLIENT_LIST is new.
                    .unwrap_or(self.initialized && !self.known.contains(&window));
                if fresh || agent_input {
                    self.last_external
                } else {
                    None
                }
            }
        }
    }
}

pub(crate) fn is_profile_wm_class(wm_class: &str, user_data_dir: &str) -> bool {
    wm_class.contains(&format!("({user_data_dir})"))
}

fn window_wm_class(window: u32) -> Option<String> {
    let output = Command::new("xprop")
        .args(["-id", &format!("0x{window:x}"), "WM_CLASS"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Start the guard thread. Returns a description of why it is inactive when it cannot run.
pub(crate) fn start_focus_guard(profile: &Profile) -> Result<(), String> {
    if std::env::var_os("DISPLAY").is_none() {
        return Err("DISPLAY is not set".to_string());
    }
    for tool in ["xprop", "xdotool"] {
        let found = Command::new(tool)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if !found {
            return Err(format!("{tool} is not in PATH"));
        }
    }
    let user_data_dir = expand_home(&profile.user_data_dir)?.display().to_string();
    let mut child = Command::new("xprop")
        .args(["-root", "-spy", "_NET_CLIENT_LIST", "_NET_ACTIVE_WINDOW"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start xprop -spy: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "xprop -spy has no stdout".to_string())?;
    thread::spawn(move || {
        let mut state = GuardState::default();
        let mut classes: HashMap<u32, bool> = HashMap::new();
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Some(event) = parse_spy_line(&line) else {
                continue;
            };
            let mut is_profile_window = |window: u32| {
                *classes.entry(window).or_insert_with(|| {
                    window_wm_class(window)
                        .map(|class| is_profile_wm_class(&class, &user_data_dir))
                        .unwrap_or(false)
                })
            };
            if let Some(restore) = state.on_event(
                event,
                Instant::now(),
                input_active(),
                &mut is_profile_window,
            ) {
                let status = Command::new("xdotool")
                    .args(["windowactivate", &restore.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                eprintln!(
                    "focus guard: a window of this profile took the focus; re-activated 0x{restore:x} ({})",
                    match status {
                        Ok(status) if status.success() => "ok".to_string(),
                        Ok(status) => format!("xdotool exited with {status}"),
                        Err(error) => format!("xdotool failed: {error}"),
                    }
                );
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("focus guard: xprop -spy ended; the guard is inactive");
    });
    Ok(())
}
