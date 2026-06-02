//! Layered configuration so the tool is easy to set up and override.
//!
//! Precedence (low → high): built-in defaults, a config file, environment
//! variables, then CLI flags. The config file is looked up at:
//!   1. `$ABF_CONFIG` (explicit path), else
//!   2. `./agent-browser-firefox.json` (project-local), else
//!   3. `~/.config/agent-browser-firefox/config.json` (user-global).
//!
//! Every field is optional in the file so users only set what they care about.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 9222;
pub const DEFAULT_SESSION: &str = "default";

/// Fully-resolved configuration used at runtime.
#[derive(Debug, Clone)]
pub struct Config {
    /// Explicit Firefox binary path (`None` = auto-detect well-known locations).
    pub firefox_bin: Option<String>,
    /// Remote-agent debugging port.
    pub port: u16,
    /// Run Firefox without a visible window.
    pub headless: bool,
    /// Profile directory (`None` = a temp profile derived from the session name).
    pub profile_dir: Option<PathBuf>,
    /// Named session, so multiple independent instances can coexist.
    pub session: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            firefox_bin: None,
            port: DEFAULT_PORT,
            headless: true,
            profile_dir: None,
            session: DEFAULT_SESSION.to_string(),
        }
    }
}

/// On-disk config file shape: all fields optional.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileConfig {
    pub firefox_bin: Option<String>,
    pub port: Option<u16>,
    pub headless: Option<bool>,
    pub profile_dir: Option<String>,
    pub session: Option<String>,
}

/// CLI flags that override file/env config. `None` means "not specified".
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub firefox_bin: Option<String>,
    pub port: Option<u16>,
    pub headless: Option<bool>,
    pub profile_dir: Option<PathBuf>,
    pub session: Option<String>,
}

impl Config {
    /// Resolve effective config from file + env + CLI overrides.
    pub fn resolve(overrides: &Overrides) -> Self {
        let mut cfg = Config::default();

        // 1. Config file.
        if let Some(file) = load_file() {
            if let Some(v) = file.firefox_bin {
                cfg.firefox_bin = Some(v);
            }
            if let Some(v) = file.port {
                cfg.port = v;
            }
            if let Some(v) = file.headless {
                cfg.headless = v;
            }
            if let Some(v) = file.profile_dir {
                cfg.profile_dir = Some(PathBuf::from(v));
            }
            if let Some(v) = file.session {
                cfg.session = v;
            }
        }

        // 2. Environment variables.
        if let Ok(v) = std::env::var("FIREFOX_BIN") {
            if !v.is_empty() {
                cfg.firefox_bin = Some(v);
            }
        }
        if let Ok(v) = std::env::var("ABF_PORT") {
            if let Ok(p) = v.parse() {
                cfg.port = p;
            }
        }
        if let Ok(v) = std::env::var("ABF_HEADLESS") {
            cfg.headless = parse_bool(&v).unwrap_or(cfg.headless);
        }
        if let Ok(v) = std::env::var("ABF_PROFILE_DIR") {
            if !v.is_empty() {
                cfg.profile_dir = Some(PathBuf::from(v));
            }
        }
        if let Ok(v) = std::env::var("ABF_SESSION") {
            if !v.is_empty() {
                cfg.session = v;
            }
        }

        // 3. CLI overrides (highest priority).
        if let Some(v) = &overrides.firefox_bin {
            cfg.firefox_bin = Some(v.clone());
        }
        if let Some(v) = overrides.port {
            cfg.port = v;
        }
        if let Some(v) = overrides.headless {
            cfg.headless = v;
        }
        if let Some(v) = &overrides.profile_dir {
            cfg.profile_dir = Some(v.clone());
        }
        if let Some(v) = &overrides.session {
            cfg.session = v.clone();
        }

        cfg
    }

    /// Effective profile directory for this session.
    pub fn effective_profile_dir(&self) -> PathBuf {
        self.profile_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("agent-browser-firefox-{}", self.session))
        })
    }
}

/// Locate and parse the config file, if any.
fn load_file() -> Option<FileConfig> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("warning: ignoring invalid config at {}: {e}", path.display());
            None
        }
    }
}

/// Resolve which config file path to read.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("ABF_CONFIG") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    let local = PathBuf::from("agent-browser-firefox.json");
    if local.exists() {
        return Some(local);
    }
    let global = dirs::config_dir()?
        .join("agent-browser-firefox")
        .join("config.json");
    if global.exists() {
        return Some(global);
    }
    None
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
