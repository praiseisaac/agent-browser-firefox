//! agent-browser-firefox: a Firefox-native browser automation CLI for AI agents.
//!
//! Architecture mirrors agent-browser's stateful UX (`open` → `click` → `close`
//! across separate invocations) but drives Firefox over WebDriver BiDi instead
//! of Chrome over CDP. State lives in an on-demand background daemon, addressable
//! by instance ID (session name) so instances can be listed, resumed, and closed.

mod actions;
mod bidi;
mod config;
mod daemon;
mod firefox;
mod install;
mod instance;
mod ipc;
mod output;

use clap::{Parser, Subcommand};
use config::{Config, Overrides};
use instance::InstanceRecord;
use ipc::{Request, Response};
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "agent-browser-firefox",
    version,
    about = "Firefox browser automation CLI for AI agents (WebDriver BiDi)"
)]
struct Cli {
    /// Instance ID / session name (lets multiple browsers run concurrently).
    #[arg(short = 's', long, global = true)]
    session: Option<String>,
    /// Remote-agent debugging port.
    #[arg(short = 'p', long, global = true)]
    port: Option<u16>,
    /// Explicit Firefox binary path.
    #[arg(long, global = true)]
    firefox_bin: Option<String>,
    /// Profile directory to use.
    #[arg(long, global = true)]
    profile: Option<PathBuf>,
    /// Run Firefox without a visible window (default).
    #[arg(long, global = true)]
    headless: bool,
    /// Run Firefox with a visible window.
    #[arg(long, global = true)]
    headed: bool,
    /// Emit structured JSON output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Launch the browser (optionally navigate to URL).
    #[command(visible_aliases = ["goto", "navigate"])]
    Open { url: Option<String> },
    /// Click an element by CSS selector or @ref.
    Click { selector: String },
    /// Clear and fill an input.
    Fill { selector: String, text: String },
    /// Type text into an element (appends).
    Type { selector: String, text: String },
    /// Press a key chord at the current focus (e.g. Enter, Tab, Control+a).
    #[command(visible_alias = "key")]
    Press { key: String },
    /// Hold a key down.
    Keydown { key: String },
    /// Release a held key.
    Keyup { key: String },
    /// Real keyboard input at the current focus: keyboard <type|inserttext> <text>.
    Keyboard {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Hover the mouse over an element.
    Hover { selector: String },
    /// Double-click an element.
    Dblclick { selector: String },
    /// Drag one element onto another.
    Drag { source: String, target: String },
    /// Scroll the page (up/down/left/right) by an optional pixel amount.
    Scroll {
        direction: String,
        amount: Option<i64>,
        /// Scroll while hovering this element.
        #[arg(long)]
        selector: Option<String>,
    },
    /// Wait for a condition: wait <ms|selector> [--text|--url|--fn|--load|--state|--timeout].
    Wait {
        target: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long = "url")]
        url: Option<String>,
        #[arg(long = "fn")]
        func: Option<String>,
        #[arg(long = "load")]
        load: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Read info: get <url|title|text|html|value|attr> [selector] [attr].
    Get {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Evaluate JavaScript in the page.
    Eval {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        js: Vec<String>,
    },
    /// Capture a screenshot (PNG).
    Screenshot {
        path: Option<PathBuf>,
        #[arg(long)]
        full: bool,
    },
    /// Accessibility-style DOM snapshot with @refs.
    Snapshot,
    /// Show the current page status for this instance.
    Status,
    /// Close the browser. `--all` closes every instance.
    #[command(visible_aliases = ["quit", "exit"])]
    Close {
        #[arg(long)]
        all: bool,
    },
    /// List all known instances and their liveness.
    #[command(visible_alias = "ls")]
    List,
    /// Show resolved configuration and config-file path.
    Config,
    /// Ensure Firefox is installed (sets up the browser dependency).
    Install {
        /// Also install extra system dependencies (Linux, for headed mode).
        #[arg(long)]
        with_deps: bool,
    },
    /// Internal: run the background daemon (not for direct use).
    #[command(name = "__daemon", hide = true)]
    Daemon,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let headless = if cli.headed {
        Some(false)
    } else if cli.headless {
        Some(true)
    } else {
        None
    };
    let overrides = Overrides {
        firefox_bin: cli.firefox_bin.clone(),
        port: cli.port,
        headless,
        profile_dir: cli.profile.clone(),
        session: cli.session.clone(),
    };
    let cfg = Config::resolve(&overrides);
    let json = cli.json;

    let code = match cli.command {
        Command::Daemon => run_daemon(cfg).await,
        Command::Open { url } => cmd_open(&cfg, url, json).await,
        Command::Click { selector } => {
            send_action(&cfg, Request::new("click", vec![selector]), json).await
        }
        Command::Fill { selector, text } => {
            send_action(&cfg, Request::new("fill", vec![selector, text]), json).await
        }
        Command::Type { selector, text } => {
            send_action(&cfg, Request::new("type", vec![selector, text]), json).await
        }
        Command::Press { key } => send_action(&cfg, Request::new("press", vec![key]), json).await,
        Command::Keydown { key } => {
            send_action(&cfg, Request::new("keydown", vec![key]), json).await
        }
        Command::Keyup { key } => send_action(&cfg, Request::new("keyup", vec![key]), json).await,
        Command::Keyboard { args } => {
            send_action(&cfg, Request::new("keyboard", args), json).await
        }
        Command::Hover { selector } => {
            send_action(&cfg, Request::new("hover", vec![selector]), json).await
        }
        Command::Dblclick { selector } => {
            send_action(&cfg, Request::new("dblclick", vec![selector]), json).await
        }
        Command::Drag { source, target } => {
            send_action(&cfg, Request::new("drag", vec![source, target]), json).await
        }
        Command::Scroll {
            direction,
            amount,
            selector,
        } => {
            let mut args = vec![direction];
            if let Some(a) = amount {
                args.push(a.to_string());
            }
            let mut req = Request::new("scroll", args);
            if let Some(sel) = selector {
                req.flags.insert("selector".into(), sel);
            }
            send_action(&cfg, req, json).await
        }
        Command::Wait {
            target,
            text,
            url,
            func,
            load,
            state,
            timeout,
        } => {
            let mut req = Request::new("wait", target.into_iter().collect());
            for (k, v) in [
                ("text", text),
                ("url", url),
                ("fn", func),
                ("load", load),
                ("state", state),
            ] {
                if let Some(val) = v {
                    req.flags.insert(k.into(), val);
                }
            }
            if let Some(t) = timeout {
                req.flags.insert("timeout".into(), t.to_string());
            }
            send_action(&cfg, req, json).await
        }
        Command::Get { args } => send_action(&cfg, Request::new("get", args), json).await,
        Command::Eval { js } => {
            send_action(&cfg, Request::new("eval", vec![js.join(" ")]), json).await
        }
        Command::Screenshot { path, full } => cmd_screenshot(&cfg, path, full, json).await,
        Command::Snapshot => send_action(&cfg, Request::new("snapshot", vec![]), json).await,
        Command::Status => cmd_status(&cfg, json).await,
        Command::Close { all } => cmd_close(&cfg, all, json).await,
        Command::List => cmd_list(json),
        Command::Config => cmd_config(&cfg, json),
        Command::Install { with_deps } => install::run(with_deps),
    };
    std::process::exit(code);
}

// ---- daemon entrypoint -----------------------------------------------------

async fn run_daemon(cfg: Config) -> i32 {
    if let Err(e) = daemon::run(cfg).await {
        eprintln!("daemon error: {e:#}");
        return 1;
    }
    0
}

// ---- commands --------------------------------------------------------------

async fn cmd_open(cfg: &Config, url: Option<String>, json: bool) -> i32 {
    let rec = match ensure_daemon(cfg, true).await {
        Ok(r) => r,
        Err(e) => return fail(&e, json),
    };
    match url {
        Some(u) => {
            let req = Request::new("navigate", vec![u]);
            dispatch(&rec, req, json).await
        }
        None => {
            let msg = format!("browser '{}' ready", cfg.session);
            output::print(&Response::ok_text(msg), json)
        }
    }
}

async fn send_action(cfg: &Config, req: Request, json: bool) -> i32 {
    let rec = match require_daemon(cfg).await {
        Ok(r) => r,
        Err(e) => return fail(&e, json),
    };
    dispatch(&rec, req, json).await
}

async fn cmd_status(cfg: &Config, json: bool) -> i32 {
    let rec = match require_daemon(cfg).await {
        Ok(r) => r,
        Err(e) => return fail(&e, json),
    };
    dispatch(&rec, Request::new("status", vec![]), json).await
}

async fn cmd_screenshot(cfg: &Config, path: Option<PathBuf>, full: bool, json: bool) -> i32 {
    use base64::Engine;
    let rec = match require_daemon(cfg).await {
        Ok(r) => r,
        Err(e) => return fail(&e, json),
    };
    let mut req = Request::new("screenshot", vec![]);
    if full {
        req.flags.insert("full".into(), "true".into());
    }
    let resp = match ipc::send_request(&PathBuf::from(&rec.socket), &req).await {
        Ok(r) => r,
        Err(e) => return fail(&e.to_string(), json),
    };
    if !resp.ok {
        return output::print(&resp, json);
    }
    let b64 = resp
        .data
        .as_ref()
        .and_then(|d| d.get("base64"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(b) => b,
        Err(e) => return fail(&format!("decoding screenshot: {e}"), json),
    };
    let out = path.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("abf-screenshot-{}.png", cfg.session))
    });
    if let Err(e) = std::fs::write(&out, &bytes) {
        return fail(&format!("writing {}: {e}", out.display()), json);
    }
    let display = out.display().to_string();
    output::print(
        &Response::ok_data(Some(display.clone()), serde_json::json!({ "path": display })),
        json,
    )
}

async fn cmd_close(cfg: &Config, all: bool, json: bool) -> i32 {
    if all {
        let mut closed = 0;
        for rec in instance::list() {
            close_one(&rec).await;
            closed += 1;
        }
        return output::print(&Response::ok_text(format!("closed {closed} instance(s)")), json);
    }
    match instance::load(&cfg.session) {
        Some(rec) => {
            close_one(&rec).await;
            output::print(&Response::ok_text(format!("closed '{}'", cfg.session)), json)
        }
        None => output::print(
            &Response::ok_text(format!("no instance '{}' to close", cfg.session)),
            json,
        ),
    }
}

async fn close_one(rec: &InstanceRecord) {
    let socket = PathBuf::from(&rec.socket);
    if instance::pid_alive(rec.daemon_pid) {
        let _ = ipc::send_request(&socket, &Request::new("close", vec![])).await;
    } else {
        if instance::pid_alive(rec.firefox_pid) {
            instance::kill_pid(rec.firefox_pid);
        }
        instance::remove(&rec.id);
        let _ = std::fs::remove_file(&socket);
    }
}

fn cmd_list(json: bool) -> i32 {
    let records = instance::list();
    if json {
        let arr: Vec<_> = records
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "port": r.port,
                    "headless": r.headless,
                    "daemonAlive": instance::pid_alive(r.daemon_pid),
                    "firefoxAlive": instance::pid_alive(r.firefox_pid),
                    "lastUrl": r.last_url,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return 0;
    }
    if records.is_empty() {
        println!("no instances");
        return 0;
    }
    println!("{:<16} {:<6} {:<8} {:<8} {}", "ID", "PORT", "DAEMON", "FIREFOX", "LAST URL");
    for r in records {
        println!(
            "{:<16} {:<6} {:<8} {:<8} {}",
            r.id,
            r.port,
            yn(instance::pid_alive(r.daemon_pid)),
            yn(instance::pid_alive(r.firefox_pid)),
            r.last_url.as_deref().unwrap_or("-")
        );
    }
    0
}

fn cmd_config(cfg: &Config, json: bool) -> i32 {
    let path = config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".into());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "configPath": path,
                "session": cfg.session,
                "port": cfg.port,
                "headless": cfg.headless,
                "firefoxBin": cfg.firefox_bin,
                "profileDir": cfg.effective_profile_dir().display().to_string(),
            }))
            .unwrap_or_default()
        );
        return 0;
    }
    println!("config file: {path}");
    println!("session:     {}", cfg.session);
    println!("port:        {}", cfg.port);
    println!("headless:    {}", cfg.headless);
    println!(
        "firefox:     {}",
        cfg.firefox_bin
            .clone()
            .or_else(firefox::find_firefox)
            .unwrap_or_else(|| "(not found)".into())
    );
    println!("profile:     {}", cfg.effective_profile_dir().display());
    0
}

// ---- daemon lifecycle helpers ---------------------------------------------

/// Ensure a live daemon exists for this instance, returning its record.
///
/// While the daemon is alive, every CLI invocation resumes the *same* browser
/// session over its socket — no restart. If the daemon has died, its Firefox
/// BiDi session is orphaned (Firefox binds the single allowed session to the
/// now-closed socket and won't let anyone reuse or recreate it), so we clean up
/// the stuck browser and, when permitted, cold-start a fresh instance.
async fn ensure_daemon(cfg: &Config, allow_launch: bool) -> Result<InstanceRecord, String> {
    if let Some(rec) = instance::load(&cfg.session) {
        if instance::pid_alive(rec.daemon_pid) && ping(&rec).await {
            return Ok(rec); // resume the live session
        }
        // Daemon gone: the browser (if any) is orphaned and unusable. Reap it.
        if instance::pid_alive(rec.firefox_pid) {
            instance::kill_pid(rec.firefox_pid);
            wait_port_free(rec.port, Duration::from_secs(5)).await;
        }
        instance::remove(&cfg.session);
        let _ = std::fs::remove_file(&rec.socket);
    }
    if !allow_launch {
        return Err(format!(
            "no browser session '{}'. Run: agent-browser-firefox open <url>",
            cfg.session
        ));
    }
    spawn_daemon(cfg)?;
    wait_ready(cfg).await
}

/// Wait until a TCP port is no longer accepting connections (browser shut down).
async fn wait_port_free(port: u16, max: Duration) {
    let deadline = tokio::time::Instant::now() + max;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn require_daemon(cfg: &Config) -> Result<InstanceRecord, String> {
    ensure_daemon(cfg, false).await
}

/// Spawn the daemon as a detached background process.
fn spawn_daemon(cfg: &Config) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("locating self: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__daemon")
        .arg("-s")
        .arg(&cfg.session)
        .arg("-p")
        .arg(cfg.port.to_string());
    if cfg.headless {
        cmd.arg("--headless");
    } else {
        cmd.arg("--headed");
    }
    if let Some(bin) = &cfg.firefox_bin {
        cmd.arg("--firefox-bin").arg(bin);
    }
    if let Some(profile) = &cfg.profile_dir {
        cmd.arg("--profile").arg(profile);
    }
    // Route daemon diagnostics to a per-session log file for troubleshooting.
    let log_path = std::env::temp_dir().join(format!("abf-daemon-{}.log", cfg.session));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("opening daemon log {}: {e}", log_path.display()))?;
    let log_err = log.try_clone().map_err(|e| format!("cloning log handle: {e}"))?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    // Detach into its own process group so CLI exit/Ctrl-C doesn't kill it.
    cmd.process_group(0);
    cmd.spawn().map_err(|e| format!("spawning daemon: {e}"))?;
    Ok(())
}

/// Poll until the daemon answers a ping (or time out).
async fn wait_ready(cfg: &Config) -> Result<InstanceRecord, String> {
    for _ in 0..200 {
        if let Some(rec) = instance::load(&cfg.session) {
            if instance::pid_alive(rec.daemon_pid) && ping(&rec).await {
                return Ok(rec);
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(format!(
        "daemon for '{}' did not become ready (is Firefox installed?)",
        cfg.session
    ))
}

async fn ping(rec: &InstanceRecord) -> bool {
    let socket = PathBuf::from(&rec.socket);
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            ipc::send_request(&socket, &Request::new("ping", vec![])),
        )
        .await,
        Ok(Ok(r)) if r.ok
    )
}

async fn dispatch(rec: &InstanceRecord, req: Request, json: bool) -> i32 {
    match ipc::send_request(&PathBuf::from(&rec.socket), &req).await {
        Ok(resp) => output::print(&resp, json),
        Err(e) => fail(&e.to_string(), json),
    }
}

// ---- small helpers ---------------------------------------------------------

fn fail(msg: &str, json: bool) -> i32 {
    output::print(&Response::err(msg.to_string()), json)
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// Build a flag map (kept for symmetry with future flag-bearing actions).
#[allow(dead_code)]
fn flags(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}
