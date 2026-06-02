//! Instance registry.
//!
//! Every launched browser is an *instance* identified by an ID (the session
//! name, e.g. `default`). Its metadata is persisted to disk so the instance can
//! be listed, resumed, and re-attached across separate CLI invocations.
//!
//! Three lifecycle states are supported by this record:
//!   1. **daemon alive**  → CLI talks to the daemon over `socket`.
//!   2. **daemon dead, Firefox alive** → re-attach a fresh BiDi session to
//!      `bidi_url`/`port`; the open tabs survive in the running browser.
//!   3. **both dead** → cold start a new instance.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Persisted metadata describing one browser instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    /// Stable instance ID == session name.
    pub id: String,
    /// Remote-agent debugging port.
    pub port: u16,
    /// Base BiDi WebSocket URL announced by Firefox (no `/session` suffix).
    pub bidi_url: String,
    /// Firefox process PID.
    pub firefox_pid: u32,
    /// Daemon process PID (the long-lived holder of the BiDi connection).
    pub daemon_pid: u32,
    /// Unix domain socket the daemon listens on for CLI requests.
    pub socket: String,
    /// Profile directory in use.
    pub profile_dir: String,
    pub headless: bool,
    /// Unix epoch seconds when the instance was created.
    pub created_at: u64,
    /// Last URL navigated to (best-effort, updated by the daemon).
    #[serde(default)]
    pub last_url: Option<String>,
}

/// Directory holding per-instance JSON records.
pub fn registry_dir() -> PathBuf {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("agent-browser-firefox").join("instances")
}

fn record_path(id: &str) -> PathBuf {
    registry_dir().join(format!("{}.json", sanitize(id)))
}

/// Conventional unix socket path for an instance (kept short for the macOS
/// `sun_path` length limit).
pub fn socket_path(id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("abf-{}.sock", sanitize(id)))
}

/// Persist (create or overwrite) an instance record.
pub fn save(rec: &InstanceRecord) -> std::io::Result<()> {
    let dir = registry_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(rec).expect("serialize instance record");
    std::fs::write(record_path(&rec.id), json)
}

/// Load a single instance record by ID.
pub fn load(id: &str) -> Option<InstanceRecord> {
    let text = std::fs::read_to_string(record_path(id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Remove an instance record from the registry.
pub fn remove(id: &str) {
    let _ = std::fs::remove_file(record_path(id));
}

/// List every known instance record.
pub fn list() -> Vec<InstanceRecord> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(registry_dir()) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    if let Ok(rec) = serde_json::from_str::<InstanceRecord>(&text) {
                        out.push(rec);
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Whether a process with the given PID is currently alive.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // signal 0 = existence/permission check, no signal delivered.
    unsafe { kill(pid as i32, 0) == 0 }
}

/// Send SIGTERM to a PID (best effort).
pub fn kill_pid(pid: u32) {
    if pid != 0 {
        unsafe {
            kill(pid as i32, 15); // SIGTERM
        }
    }
}

// Minimal libc::kill shim to avoid pulling in the full `libc` crate.
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Keep a filename safe and predictable.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
