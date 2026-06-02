//! High-level BiDi session: establishes a session over a [`BidiClient`], tracks
//! the active browsing context (tab), and exposes the page operations the CLI
//! needs. Conversion helpers turn BiDi `RemoteValue`s back into plain JSON.

use super::client::BidiClient;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default timeout for a single BiDi command, so a hung page (e.g. an app that
/// never reaches the load event) can't block the daemon forever.
const NAV_TIMEOUT: Duration = Duration::from_secs(45);
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

pub struct BidiSession {
    client: Arc<BidiClient>,
    pub session_id: String,
    /// Last-known top-level browsing context (tab). Always re-resolved before an
    /// action; cached only for display and as a hint.
    context: Mutex<String>,
}

impl BidiSession {
    /// Open a BiDi session and bind to the current top-level browsing context.
    ///
    /// Firefox permits only one BiDi session per browser. When resuming a
    /// still-running Firefox whose previous session lingers, `session.new`
    /// reports the limit; we then reuse the existing session rather than fail.
    pub async fn establish(client: Arc<BidiClient>) -> Result<Self, String> {
        let session_id = match client.send("session.new", json!({ "capabilities": {} })).await {
            Ok(res) => res
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Err(e) if e.contains("Maximum number of active sessions") => {
                // Resume: a session already exists for this browser.
                String::new()
            }
            Err(e) => return Err(e),
        };

        // Subscribe to navigation/load events so navigate() can wait reliably.
        let _ = client
            .send(
                "session.subscribe",
                json!({ "events": ["browsingContext.load", "browsingContext.domContentLoaded"] }),
            )
            .await;

        let context = Self::resolve_top_context(&client).await?;
        Ok(Self {
            client,
            session_id,
            context: Mutex::new(context),
        })
    }

    /// Find the current top-level browsing context, creating a tab if the window
    /// has none.
    async fn resolve_top_context(client: &Arc<BidiClient>) -> Result<String, String> {
        let tree = client.send("browsingContext.getTree", json!({})).await?;
        if let Some(ctx) = tree
            .get("contexts")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("context"))
            .and_then(Value::as_str)
        {
            return Ok(ctx.to_string());
        }
        let created = client
            .send("browsingContext.create", json!({ "type": "tab" }))
            .await?;
        created
            .get("context")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "could not obtain a browsing context".to_string())
    }

    /// Re-resolve and cache the active context before each action, so a stale id
    /// (a startup-tab swap, a COOP/cross-origin navigation, or the user closing
    /// the tab in headed mode) can't break the next command.
    async fn active_context(&self) -> Result<String, String> {
        let ctx = Self::resolve_top_context(&self.client).await?;
        if let Ok(mut guard) = self.context.lock() {
            *guard = ctx.clone();
        }
        Ok(ctx)
    }

    /// Last-known context id, for status display only.
    pub fn cached_context(&self) -> String {
        self.context.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Send a command with a timeout, mapping a timeout to a readable error.
    async fn send_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        match tokio::time::timeout(timeout, self.client.send(method, params)).await {
            Ok(res) => res,
            Err(_) => Err(format!("{method} timed out after {}s", timeout.as_secs())),
        }
    }

    /// Navigate the active context and wait for the document to be complete.
    pub async fn navigate(&self, url: &str) -> Result<String, String> {
        let ctx = self.active_context().await?;
        let res = self
            .send_timeout(
                "browsingContext.navigate",
                json!({ "context": ctx, "url": url, "wait": "complete" }),
                NAV_TIMEOUT,
            )
            .await?;
        Ok(res
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or(url)
            .to_string())
    }

    /// Evaluate a JS expression in the active context and return its value as JSON.
    pub async fn evaluate(&self, expression: &str) -> Result<Value, String> {
        let ctx = self.active_context().await?;
        let res = self
            .send_timeout(
                "script.evaluate",
                json!({
                    "expression": expression,
                    "target": { "context": ctx },
                    "awaitPromise": true,
                    "resultOwnership": "none",
                }),
                CMD_TIMEOUT,
            )
            .await?;

        match res.get("type").and_then(Value::as_str) {
            Some("success") => Ok(remote_to_json(res.get("result").unwrap_or(&Value::Null))),
            Some("exception") => {
                let text = res
                    .get("exceptionDetails")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("script raised an exception");
                Err(text.to_string())
            }
            _ => Ok(Value::Null),
        }
    }

    pub async fn get_url(&self) -> Result<String, String> {
        Ok(self
            .evaluate("window.location.href")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    pub async fn get_title(&self) -> Result<String, String> {
        Ok(self
            .evaluate("document.title")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    #[allow(dead_code)]
    pub async fn get_content(&self) -> Result<String, String> {
        Ok(self
            .evaluate("document.documentElement.outerHTML")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    /// Capture a screenshot of the active context, returned as base64 PNG.
    pub async fn screenshot(&self) -> Result<String, String> {
        let ctx = self.active_context().await?;
        let res = self
            .send_timeout(
                "browsingContext.captureScreenshot",
                json!({ "context": ctx }),
                CMD_TIMEOUT,
            )
            .await?;
        res.get("data")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "screenshot returned no data".to_string())
    }

    /// Click the first element matching a CSS selector (via injected JS for MVP).
    pub async fn click(&self, selector: &str) -> Result<(), String> {
        let expr = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) throw new Error('no element matches {sel_disp}'); el.scrollIntoView({{block:'center'}}); el.click(); return true; }})()",
            sel = js_string(selector),
            sel_disp = selector.replace('\'', "")
        );
        self.evaluate(&expr).await.map(|_| ())
    }

    /// Clear and fill an input matching a CSS selector.
    pub async fn fill(&self, selector: &str, value: &str) -> Result<(), String> {
        let expr = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) throw new Error('no element matches'); el.focus(); el.value = {val}; el.dispatchEvent(new Event('input', {{bubbles:true}})); el.dispatchEvent(new Event('change', {{bubbles:true}})); return true; }})()",
            sel = js_string(selector),
            val = js_string(value)
        );
        self.evaluate(&expr).await.map(|_| ())
    }

    pub async fn close(&self) -> Result<(), String> {
        let _ = self.client.send("session.end", json!({})).await;
        Ok(())
    }
}

/// JSON-encode a string for safe embedding in a JS source literal.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Convert a BiDi `RemoteValue` into plain JSON for primitive and simple
/// container results. Non-serializable handles collapse to their type name.
fn remote_to_json(rv: &Value) -> Value {
    match rv.get("type").and_then(Value::as_str) {
        Some("string") | Some("number") | Some("boolean") => {
            rv.get("value").cloned().unwrap_or(Value::Null)
        }
        Some("null") | Some("undefined") => Value::Null,
        Some("bigint") => rv.get("value").cloned().unwrap_or(Value::Null),
        Some("array") | Some("set") => {
            let items = rv
                .get("value")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().map(remote_to_json).collect())
                .unwrap_or_default();
            Value::Array(items)
        }
        Some("object") | Some("map") => {
            // value is an array of [key, value] pairs.
            let mut obj = serde_json::Map::new();
            if let Some(pairs) = rv.get("value").and_then(Value::as_array) {
                for pair in pairs {
                    if let Some(p) = pair.as_array() {
                        if p.len() == 2 {
                            let key = p[0].as_str().map(str::to_string).unwrap_or_else(|| {
                                remote_to_json(&p[0]).as_str().unwrap_or("").to_string()
                            });
                            obj.insert(key, remote_to_json(&p[1]));
                        }
                    }
                }
            }
            Value::Object(obj)
        }
        _ => rv.get("value").cloned().unwrap_or(Value::Null),
    }
}
