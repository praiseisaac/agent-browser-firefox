//! `install` / setup flow: make sure Firefox is present, installing it via the
//! platform package manager when it isn't. Mirrors agent-browser's
//! `agent-browser install` (which fetches Chrome for Testing) — our analogue
//! ensures a usable Firefox.

use std::process::Command;

/// Run the setup flow. Returns a process exit code.
pub fn run(with_deps: bool) -> i32 {
    println!("agent-browser-firefox · setup");

    match crate::firefox::find_firefox() {
        Some(path) => {
            ok(&format!("Firefox found: {path}"));
        }
        None => {
            info("Firefox not found — attempting to install it…");
            match install_firefox(with_deps) {
                Ok(()) => match crate::firefox::find_firefox() {
                    Some(path) => ok(&format!("Firefox installed: {path}")),
                    None => {
                        err("Firefox installed but could not be located.");
                        hint("Set $FIREFOX_BIN to its path, or pass --firefox-bin.");
                        return 1;
                    }
                },
                Err(e) => {
                    err(&format!("could not install Firefox automatically: {e}"));
                    hint("Install it manually from https://www.mozilla.org/firefox/ then re-run.");
                    return 1;
                }
            }
        }
    }

    ok("Ready.");
    println!("\nTry it:\n  agent-browser-firefox open example.com\n  agent-browser-firefox snapshot");
    0
}

/// Install Firefox using whatever package manager the platform provides.
fn install_firefox(with_deps: bool) -> Result<(), String> {
    match std::env::consts::OS {
        "macos" => {
            if which("brew") {
                run_cmd("brew", &["install", "--cask", "firefox"])
            } else {
                Err("Homebrew not found — install it from https://brew.sh, or download Firefox manually.".into())
            }
        }
        "linux" => install_firefox_linux(with_deps),
        other => Err(format!("automatic install isn't supported on '{other}'")),
    }
}

fn install_firefox_linux(with_deps: bool) -> Result<(), String> {
    let _ = with_deps; // reserved: e.g. extra X/gtk libs for headed mode
    if which("apt-get") {
        let _ = sudo("apt-get", &["update"]);
        // Debian/Ubuntu ship either `firefox` or `firefox-esr`.
        sudo("apt-get", &["install", "-y", "firefox"])
            .or_else(|_| sudo("apt-get", &["install", "-y", "firefox-esr"]))
    } else if which("dnf") {
        sudo("dnf", &["install", "-y", "firefox"])
    } else if which("pacman") {
        sudo("pacman", &["-S", "--noconfirm", "firefox"])
    } else if which("zypper") {
        sudo("zypper", &["--non-interactive", "install", "firefox"])
    } else {
        Err("no supported package manager found (apt/dnf/pacman/zypper)".into())
    }
}

// ---- shell helpers ---------------------------------------------------------

/// Is `bin` on PATH?
fn which(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a command, streaming its output; error on non-zero exit.
fn run_cmd(bin: &str, args: &[&str]) -> Result<(), String> {
    info(&format!("$ {bin} {}", args.join(" ")));
    let status = Command::new(bin)
        .args(args)
        .status()
        .map_err(|e| format!("failed to run {bin}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{bin} exited with {status}"))
    }
}

/// Like `run_cmd`, but prefixes `sudo` when not already root and sudo exists.
fn sudo(bin: &str, args: &[&str]) -> Result<(), String> {
    if is_root() || !which("sudo") {
        run_cmd(bin, args)
    } else {
        let mut full = vec![bin];
        full.extend_from_slice(args);
        run_cmd("sudo", &full)
    }
}

fn is_root() -> bool {
    // geteuid() == 0
    extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() == 0 }
}

// ---- pretty output ---------------------------------------------------------

fn info(msg: &str) {
    println!("\x1b[1;34m•\x1b[0m {msg}");
}
fn ok(msg: &str) {
    println!("\x1b[1;32m✓\x1b[0m {msg}");
}
fn err(msg: &str) {
    eprintln!("\x1b[1;31m✗\x1b[0m {msg}");
}
fn hint(msg: &str) {
    println!("  \x1b[2m{msg}\x1b[0m");
}
