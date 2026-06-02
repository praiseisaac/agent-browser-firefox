//! Line-delimited JSON IPC between the short-lived CLI process and the
//! long-lived daemon that owns the BiDi connection. One request, one response,
//! per connection.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A command for the daemon to run against its browser session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Action name, e.g. `navigate`, `click`, `eval`, `close`.
    pub action: String,
    /// Positional arguments for the action.
    #[serde(default)]
    pub args: Vec<String>,
    /// Named flags (e.g. `full=true`, `path=/tmp/x.png`).
    #[serde(default)]
    pub flags: HashMap<String, String>,
}

impl Request {
    pub fn new(action: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            action: action.into(),
            args,
            flags: HashMap::new(),
        }
    }

    /// Read a named flag (used by flag-bearing actions, e.g. screenshot `full`).
    #[allow(dead_code)]
    pub fn flag(&self, key: &str) -> Option<&str> {
        self.flags.get(key).map(String::as_str)
    }

    #[allow(dead_code)]
    pub fn flag_bool(&self, key: &str) -> bool {
        matches!(self.flags.get(key).map(String::as_str), Some("true") | Some("1"))
    }
}

/// The daemon's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    /// Human-readable line(s) for plain output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Structured payload for `--json` output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Error message when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok_text(text: impl Into<String>) -> Self {
        Self { ok: true, text: Some(text.into()), data: None, error: None }
    }
    pub fn ok_data(text: Option<String>, data: Value) -> Self {
        Self { ok: true, text, data: Some(data), error: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, text: None, data: None, error: Some(msg.into()) }
    }
}

/// Send one request to a daemon socket and read its response.
pub async fn send_request(socket: &std::path::Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to daemon socket {}", socket.display()))?;
    let (read_half, mut write_half) = stream.into_split();

    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();
    reader.read_line(&mut buf).await?;
    if buf.trim().is_empty() {
        return Err(anyhow!("daemon closed the connection without responding"));
    }
    serde_json::from_str(buf.trim()).context("parsing daemon response")
}
