//! High-level BiDi session: establishes a session over a [`BidiClient`], tracks
//! the active browsing context (tab), and exposes the page operations the CLI
//! needs. Conversion helpers turn BiDi `RemoteValue`s back into plain JSON.

use super::client::BidiClient;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

    /// Click the first element matching a CSS selector. Focuses the element
    /// first so a following `keyboard type` / `press` lands on it.
    pub async fn click(&self, selector: &str) -> Result<(), String> {
        let expr = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) throw new Error('no element matches {sel_disp}'); el.scrollIntoView({{block:'center'}}); if (el.focus) try {{ el.focus(); }} catch (e) {{}} el.click(); return true; }})()",
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

    // ---- Real input via input.performActions -------------------------------

    /// Send a list of BiDi input source-actions against the active context.
    async fn perform_actions(&self, sources: Value) -> Result<(), String> {
        let ctx = self.active_context().await?;
        self.send_timeout(
            "input.performActions",
            json!({ "context": ctx, "actions": sources }),
            CMD_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    /// Resolve an element's viewport-center coordinates, scrolling it into view.
    async fn element_center(&self, selector: &str) -> Result<(i64, i64), String> {
        let expr = format!(
            "(() => {{ const el = document.querySelector({s}); if (!el) return null; el.scrollIntoView({{block:'center',inline:'center'}}); const r = el.getBoundingClientRect(); return {{ x: Math.round(r.left + r.width/2), y: Math.round(r.top + r.height/2) }}; }})()",
            s = js_string(selector)
        );
        let v = self.evaluate(&expr).await?;
        if v.is_null() {
            return Err(format!("no element matches {selector}"));
        }
        let x = v.get("x").and_then(Value::as_i64).ok_or("element has no position")?;
        let y = v.get("y").and_then(Value::as_i64).ok_or("element has no position")?;
        Ok((x, y))
    }

    /// Press a key chord like `Enter`, `Tab`, or `Control+a` at the current focus.
    pub async fn press(&self, combo: &str) -> Result<(), String> {
        let keys = parse_combo(combo)?;
        let mut actions = Vec::new();
        for k in &keys {
            actions.push(json!({ "type": "keyDown", "value": k }));
        }
        for k in keys.iter().rev() {
            actions.push(json!({ "type": "keyUp", "value": k }));
        }
        self.perform_actions(json!([{ "type": "key", "id": "kb", "actions": actions }]))
            .await
    }

    /// Hold a key down (or release it) without the matching up/down.
    pub async fn key_hold(&self, key: &str, down: bool) -> Result<(), String> {
        let v = key_value(key).ok_or_else(|| format!("unknown key '{key}'"))?;
        let kind = if down { "keyDown" } else { "keyUp" };
        self.perform_actions(json!([{ "type": "key", "id": "kb", "actions": [{ "type": kind, "value": v }] }]))
            .await
    }

    /// Type text as real keystrokes at the current focus.
    pub async fn type_keys(&self, text: &str) -> Result<(), String> {
        let mut actions = Vec::new();
        for ch in text.chars() {
            let s = ch.to_string();
            actions.push(json!({ "type": "keyDown", "value": s }));
            actions.push(json!({ "type": "keyUp", "value": s }));
        }
        self.perform_actions(json!([{ "type": "key", "id": "kb", "actions": actions }]))
            .await
    }

    /// Insert text into the focused element without synthesizing key events.
    pub async fn insert_text(&self, text: &str) -> Result<(), String> {
        let expr = format!(
            "(() => {{ const el = document.activeElement; if (!el) throw new Error('no focused element'); if ('value' in el) {{ const s=el.selectionStart??el.value.length, e=el.selectionEnd??el.value.length; el.value = el.value.slice(0,s) + {t} + el.value.slice(e); }} else {{ el.textContent += {t}; }} el.dispatchEvent(new Event('input', {{bubbles:true}})); return true; }})()",
            t = js_string(text)
        );
        self.evaluate(&expr).await.map(|_| ())
    }

    /// Move the mouse over an element.
    pub async fn hover(&self, selector: &str) -> Result<(), String> {
        let (x, y) = self.element_center(selector).await?;
        self.perform_actions(json!([{
            "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" },
            "actions": [{ "type": "pointerMove", "x": x, "y": y }]
        }]))
        .await
    }

    /// Double-click an element with real pointer events.
    pub async fn dblclick(&self, selector: &str) -> Result<(), String> {
        let (x, y) = self.element_center(selector).await?;
        self.perform_actions(json!([{
            "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" },
            "actions": [
                { "type": "pointerMove", "x": x, "y": y },
                { "type": "pointerDown", "button": 0 }, { "type": "pointerUp", "button": 0 },
                { "type": "pointerDown", "button": 0 }, { "type": "pointerUp", "button": 0 }
            ]
        }]))
        .await
    }

    /// Drag from one element to another with real pointer events.
    pub async fn drag(&self, source: &str, target: &str) -> Result<(), String> {
        let (x1, y1) = self.element_center(source).await?;
        let (x2, y2) = self.element_center(target).await?;
        self.perform_actions(json!([{
            "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" },
            "actions": [
                { "type": "pointerMove", "x": x1, "y": y1 },
                { "type": "pointerDown", "button": 0 },
                { "type": "pointerMove", "x": x2, "y": y2, "duration": 200 },
                { "type": "pointerUp", "button": 0 }
            ]
        }]))
        .await
    }

    /// Scroll the page (or over a specific element) by a wheel delta.
    pub async fn scroll(&self, dir: &str, amount: i64, selector: Option<&str>) -> Result<(), String> {
        let (ox, oy) = match selector {
            Some(sel) => self.element_center(sel).await?,
            None => (20, 20),
        };
        let (dx, dy) = match dir {
            "up" => (0, -amount),
            "down" => (0, amount),
            "left" => (-amount, 0),
            "right" => (amount, 0),
            other => return Err(format!("unknown scroll direction '{other}' (up/down/left/right)")),
        };
        self.perform_actions(json!([{
            "type": "wheel", "id": "wheel",
            "actions": [{ "type": "scroll", "x": ox, "y": oy, "deltaX": dx, "deltaY": dy }]
        }]))
        .await
    }

    // ---- Waiting -----------------------------------------------------------

    /// Poll a boolean JS expression until it is truthy or the timeout elapses.
    pub async fn wait_for_js(&self, bool_expr: &str, timeout: Duration) -> Result<bool, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let v = self.evaluate(bool_expr).await.unwrap_or(Value::Bool(false));
            let truthy = v.as_bool().unwrap_or(false)
                || v.as_i64().map(|n| n != 0).unwrap_or(false)
                || v.as_str().map(|s| !s.is_empty()).unwrap_or(false);
            if truthy {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    }

    pub async fn close(&self) -> Result<(), String> {
        let _ = self.client.send("session.end", json!({})).await;
        Ok(())
    }
}

/// Map a key name to its WebDriver key value (the `\uE0xx` codepoints for
/// special keys; the character itself otherwise).
fn key_value(name: &str) -> Option<String> {
    let k = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => '\u{E007}',
        "tab" => '\u{E004}',
        "escape" | "esc" => '\u{E00C}',
        "backspace" => '\u{E003}',
        "delete" | "del" => '\u{E017}',
        "space" => ' ',
        "up" | "arrowup" => '\u{E013}',
        "down" | "arrowdown" => '\u{E015}',
        "left" | "arrowleft" => '\u{E012}',
        "right" | "arrowright" => '\u{E014}',
        "home" => '\u{E011}',
        "end" => '\u{E010}',
        "pageup" => '\u{E00E}',
        "pagedown" => '\u{E00F}',
        "insert" => '\u{E016}',
        "shift" => '\u{E008}',
        "control" | "ctrl" => '\u{E009}',
        "alt" | "option" => '\u{E00A}',
        "meta" | "cmd" | "command" | "super" => '\u{E03D}',
        "f1" => '\u{E031}', "f2" => '\u{E032}', "f3" => '\u{E033}', "f4" => '\u{E034}',
        "f5" => '\u{E035}', "f6" => '\u{E036}', "f7" => '\u{E037}', "f8" => '\u{E038}',
        "f9" => '\u{E039}', "f10" => '\u{E03A}', "f11" => '\u{E03B}', "f12" => '\u{E03C}',
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c, // single character
                _ => return None,
            }
        }
    };
    Some(k.to_string())
}

/// Parse a chord like `Control+Shift+a` into ordered key values.
fn parse_combo(combo: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for part in combo.split('+') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(key_value(part).ok_or_else(|| format!("unknown key '{part}'"))?);
    }
    if out.is_empty() {
        return Err("no key given".into());
    }
    Ok(out)
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
