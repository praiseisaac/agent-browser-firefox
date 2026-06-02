//! Launch Firefox with the remote agent enabled and discover its WebDriver BiDi
//! WebSocket endpoint. This is the Firefox analogue of attaching to Chrome's CDP
//! endpoint: we start the browser with `--remote-debugging-port`, then read the
//! `WebDriver BiDi listening on ws://...` line the remote agent prints to stderr.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout, Duration};

/// Well-known macOS install locations, most-preferred first.
pub const DEFAULT_FIREFOX_PATHS: &[&str] = &[
    "/Applications/Firefox Nightly.app/Contents/MacOS/firefox",
    "/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox",
    "/Applications/Firefox.app/Contents/MacOS/firefox",
];

/// A running Firefox instance plus the BiDi endpoint to talk to it.
pub struct LaunchedFirefox {
    pub child: Child,
    pub bidi_url: String,
    #[allow(dead_code)]
    pub profile_dir: PathBuf,
}

/// Resolve the Firefox binary: `$FIREFOX_BIN` wins, then the well-known paths,
/// then a bare `firefox` on `$PATH`.
pub fn find_firefox() -> Option<String> {
    if let Ok(p) = std::env::var("FIREFOX_BIN") {
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    for p in DEFAULT_FIREFOX_PATHS {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    // Fall back to PATH lookup.
    if let Ok(out) = std::process::Command::new("which").arg("firefox").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Launch Firefox with the remote agent on `port` and a fresh profile, then wait
/// until the remote agent is accepting connections on that port.
///
/// Discovery is deterministic — `ws://127.0.0.1:<port>/session` — rather than
/// parsed from stderr. On macOS the `firefox` launcher closes its inherited
/// stderr shortly after startup (the browser keeps running), so an stderr EOF is
/// not a reliable "exited" signal. We poll the TCP port instead, only treating an
/// actual process exit as failure.
pub async fn launch(port: u16, headless: bool, profile_dir: PathBuf) -> Result<LaunchedFirefox> {
    let bin = find_firefox()
        .ok_or_else(|| anyhow!("Firefox not found. Set $FIREFOX_BIN or install Firefox."))?;

    std::fs::create_dir_all(&profile_dir)
        .with_context(|| format!("creating profile dir {}", profile_dir.display()))?;
    write_isolation_prefs(&profile_dir);

    let mut cmd = Command::new(&bin);
    // Belt-and-suspenders isolation: never adopt or steal the user's running
    // Firefox; always behave as a brand-new, fully separate instance.
    cmd.env("MOZ_NO_REMOTE", "1");
    cmd.arg("--remote-debugging-port")
        .arg(port.to_string())
        // Allow the local WebSocket client regardless of Host/Origin checks.
        .arg("--remote-allow-hosts")
        .arg("localhost,127.0.0.1")
        .arg("--profile")
        .arg(&profile_dir)
        .arg("--no-remote")
        .arg("--new-instance");
    if headless {
        cmd.arg("--headless");
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    cmd.kill_on_drop(false);

    let mut child = cmd.spawn().context("failed to spawn Firefox")?;

    // Drain stderr in the background purely for diagnostics; never block on it.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(_)) = lines.next_line().await {}
        });
    }

    // Wait for the remote agent to accept connections, or for Firefox to die.
    let ready = timeout(Duration::from_secs(30), async {
        loop {
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return Ok::<(), anyhow::Error>(());
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(anyhow!("Firefox exited ({status}) before the remote agent came up"));
            }
            sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .context("timed out waiting for the Firefox remote agent")??;
    let _ = ready;

    Ok(LaunchedFirefox {
        child,
        bidi_url: format!("ws://127.0.0.1:{port}"),
        profile_dir,
    })
}

/// Write a `user.js` into the dedicated profile that suppresses first-run flows,
/// update prompts, default-browser nags, and telemetry. This keeps the
/// automation instance quiet and self-contained, fully separate from any
/// personal Firefox profile.
fn write_isolation_prefs(profile_dir: &std::path::Path) {
    let prefs = r#"// Managed by agent-browser-firefox — dedicated automation profile.
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.startup.page", 0);
user_pref("startup.homepage_welcome_url", "about:blank");
user_pref("startup.homepage_welcome_url.additional", "");
user_pref("browser.aboutwelcome.enabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("datareporting.healthreport.uploadEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("app.update.enabled", false);
user_pref("app.update.auto", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("browser.sessionstore.resume_from_crash", false);
"#;
    let _ = std::fs::write(profile_dir.join("user.js"), prefs);
}
