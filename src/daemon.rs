//! The daemon: a long-lived process that owns one Firefox instance and its BiDi
//! session, and serves CLI commands over a unix socket. It is spawned on demand
//! by `open` and lives until `close` (or until killed).

use crate::actions;
use crate::bidi::{BidiClient, BidiSession};
use crate::config::Config;
use crate::firefox;
use crate::instance::{self, InstanceRecord};
use crate::ipc::{Request, Response};
use anyhow::{anyhow, Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Launch the browser, establish a session, register the instance, and serve
/// requests until `close`. Runs in the foreground of the spawned daemon process.
pub async fn run(cfg: Config) -> Result<()> {
    let profile = cfg.effective_profile_dir();
    let ff = firefox::launch(cfg.port, cfg.headless, profile.clone())
        .await
        .context("launching Firefox")?;
    let firefox_pid = ff.child.id().unwrap_or(0);

    let endpoint = format!("{}/session", ff.bidi_url.trim_end_matches('/'));
    let client = BidiClient::connect(&endpoint)
        .await
        .context("connecting BiDi")?;
    let session = BidiSession::establish(client)
        .await
        .map_err(|e| anyhow!("establishing BiDi session: {e}"))?;

    let socket = instance::socket_path(&cfg.session);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("binding daemon socket {}", socket.display()))?;

    let mut record = InstanceRecord {
        id: cfg.session.clone(),
        port: cfg.port,
        bidi_url: ff.bidi_url.clone(),
        firefox_pid,
        daemon_pid: std::process::id(),
        socket: socket.to_string_lossy().to_string(),
        profile_dir: profile.to_string_lossy().to_string(),
        headless: cfg.headless,
        created_at: now_secs(),
        last_url: None,
    };
    instance::save(&record).context("writing instance record")?;

    let mut ff = ff;
    if let Err(e) = serve(&listener, &session, &mut record).await {
        eprintln!("daemon serve loop ended with error: {e}");
    }

    // Teardown.
    let _ = session.close().await;
    let _ = ff.child.kill().await;
    instance::remove(&cfg.session);
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Accept and process connections one at a time until a `close` request.
async fn serve(
    listener: &UnixListener,
    session: &BidiSession,
    record: &mut InstanceRecord,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        if handle_connection(stream, session, record).await? {
            return Ok(()); // close requested
        }
    }
}

/// Handle one request/response. Returns `true` if the daemon should shut down.
async fn handle_connection(
    stream: UnixStream,
    session: &BidiSession,
    record: &mut InstanceRecord,
) -> Result<bool> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(false); // client hung up
    }

    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            write_response(&mut write_half, &Response::err(format!("bad request: {e}"))).await?;
            return Ok(false);
        }
    };

    let (resp, shutdown) = match req.action.as_str() {
        "ping" => (Response::ok_text("pong"), false),
        "close" => (Response::ok_text("closed"), true),
        _ => (actions::handle(session, &req).await, false),
    };

    // Best-effort: keep last_url fresh for the registry.
    if let Some(url) = resp.data.as_ref().and_then(|d| d.get("url")).and_then(|u| u.as_str()) {
        record.last_url = Some(url.to_string());
        let _ = instance::save(record);
    }

    write_response(&mut write_half, &resp).await?;
    Ok(shutdown)
}

async fn write_response(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &Response,
) -> Result<()> {
    let mut out = serde_json::to_string(resp)?;
    out.push('\n');
    write_half.write_all(out.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
