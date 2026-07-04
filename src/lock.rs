use crate::config::*;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::Instant;

pub(crate) struct ProfileLock {
    path: PathBuf,
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn acquire_profile_lock(profile: &Profile) -> Result<ProfileLock, String> {
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

pub(crate) fn lock_timeout() -> Duration {
    env::var("CHROME_DEVTOOLS_LOCK_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(300))
}

pub(crate) fn remove_stale_lock(path: &Path) -> Result<bool, String> {
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

pub(crate) fn parse_lock_pid(content: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        line.strip_prefix("pid=")
            .and_then(|value| value.trim().parse::<u32>().ok())
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn process_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_exists(pid: u32) -> bool {
    match Command::new("kill").args(["-0", &pid.to_string()]).output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => !String::from_utf8_lossy(&output.stderr).contains("No such process"),
        Err(_) => true,
    }
}

pub(crate) fn safe_lock_name(name: &str) -> String {
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
