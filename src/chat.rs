//! AI chat: natural-language browser control.
//!
//! Drives the browser by looping the Anthropic Messages API with tool use:
//! snapshot the page → let Claude choose tools → execute them against the daemon
//! → feed results back → repeat until Claude answers without a tool call.
//!
//! Requires `ANTHROPIC_API_KEY`. Model defaults to `claude-sonnet-4-6`
//! (override with `$ABF_MODEL`).

use crate::config::Config;
use crate::instance::InstanceRecord;
use crate::ipc::{self, Request};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const MAX_STEPS: usize = 24;

const SYSTEM_PROMPT: &str = "\
You are a browser-automation agent driving Firefox. Accomplish the user's goal using the tools.

Workflow:
- Call `snapshot` to see the page as a list of elements, each with an @ref (e.g. @e5).
- Act using those refs (click @e5) or CSS selectors.
- Use `navigate` for URLs. After anything that changes the page, snapshot again before acting.
- Prefer refs from the latest snapshot; they change when the page changes.
- When the goal is met, reply with a short summary and NO tool call.

Be efficient — don't snapshot redundantly. If a step fails, read the error and adapt.";

/// Entry point for the `chat` command.
pub async fn run(cfg: &Config, instruction: Option<String>) -> i32 {
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("error: set ANTHROPIC_API_KEY to use chat");
            return 1;
        }
    };
    let rec = match crate::ensure_daemon(cfg, true).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let model = std::env::var("ABF_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let http = reqwest::Client::new();

    let mut agent = Agent {
        http,
        api_key,
        model,
        rec,
        messages: Vec::new(),
    };

    match instruction {
        Some(text) => agent.turn(&text).await,
        None => agent.repl().await,
    }
}

struct Agent {
    http: reqwest::Client,
    api_key: String,
    model: String,
    rec: InstanceRecord,
    messages: Vec<Value>,
}

impl Agent {
    /// Interactive REPL: each line is a new instruction; context carries over.
    async fn repl(&mut self) -> i32 {
        println!("agent-browser-firefox chat — type a goal, or 'exit' to quit.");
        let stdin = std::io::stdin();
        loop {
            print!("\x1b[1;36m›\x1b[0m ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if stdin.read_line(&mut line).unwrap_or(0) == 0 {
                break; // EOF
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if matches!(line, "exit" | "quit" | ":q") {
                break;
            }
            let code = self.turn(line).await;
            if code != 0 {
                return code;
            }
        }
        0
    }

    /// Run one instruction to completion (the tool-use loop).
    async fn turn(&mut self, instruction: &str) -> i32 {
        self.messages.push(json!({ "role": "user", "content": instruction }));

        for _ in 0..MAX_STEPS {
            let resp = match self.call_api().await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("error: {e}");
                    return 1;
                }
            };
            if let Some(err) = resp.get("error") {
                eprintln!("api error: {err}");
                return 1;
            }

            let content = resp.get("content").cloned().unwrap_or_else(|| json!([]));
            let blocks = content.as_array().cloned().unwrap_or_default();

            // Print any assistant text.
            for b in &blocks {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !t.trim().is_empty() {
                            println!("{t}");
                        }
                    }
                }
            }

            // Record the assistant turn verbatim.
            self.messages.push(json!({ "role": "assistant", "content": content }));

            if resp.get("stop_reason").and_then(Value::as_str) != Some("tool_use") {
                return 0; // done
            }

            // Execute each requested tool and collect results.
            let mut results = Vec::new();
            for b in &blocks {
                if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                let input = b.get("input").cloned().unwrap_or(Value::Null);
                let out = self.exec_tool(name, &input).await;
                println!("  \x1b[2m↳ {name}({}) → {}\x1b[0m", brief(&input), brief_str(&out));
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": out,
                }));
            }
            self.messages.push(json!({ "role": "user", "content": results }));
        }

        eprintln!("(stopped after {MAX_STEPS} steps)");
        0
    }

    async fn call_api(&self) -> Result<Value, String> {
        let body = json!({
            "model": self.model,
            "max_tokens": 2048,
            "system": SYSTEM_PROMPT,
            "tools": tools(),
            "messages": self.messages,
        });
        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        resp.json::<Value>()
            .await
            .map_err(|e| format!("bad API response: {e}"))
    }

    /// Map a tool call to a daemon action and return a textual result.
    async fn exec_tool(&self, name: &str, input: &Value) -> String {
        let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let req = match name {
            "navigate" => Request::new("navigate", vec![s("url")]),
            "snapshot" => Request::new("snapshot", vec![]),
            "click" => Request::new("click", vec![s("target")]),
            "fill" => Request::new("fill", vec![s("target"), s("text")]),
            "type" => Request::new("type", vec![s("target"), s("text")]),
            "press" => Request::new("press", vec![s("key")]),
            "get_text" => {
                let mut a = vec!["text".to_string()];
                if !s("selector").is_empty() {
                    a.push(s("selector"));
                }
                Request::new("get", a)
            }
            "get_url" => Request::new("get", vec!["url".to_string()]),
            "eval" => Request::new("eval", vec![s("js")]),
            "scroll" => {
                let mut a = vec![if s("direction").is_empty() { "down".into() } else { s("direction") }];
                if let Some(n) = input.get("amount").and_then(Value::as_i64) {
                    a.push(n.to_string());
                }
                Request::new("scroll", a)
            }
            other => return format!("unknown tool '{other}'"),
        };
        match ipc::send_request(&PathBuf::from(&self.rec.socket), &req).await {
            Ok(r) if r.ok => r.text.unwrap_or_else(|| "ok".to_string()),
            Ok(r) => format!("error: {}", r.error.unwrap_or_default()),
            Err(e) => format!("error: {e}"),
        }
    }
}

/// Tool schema advertised to the model.
fn tools() -> Value {
    json!([
        { "name": "navigate", "description": "Navigate to a URL.",
          "input_schema": { "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] } },
        { "name": "snapshot", "description": "Get the page as a list of elements with @refs. Do this before acting.",
          "input_schema": { "type": "object", "properties": {} } },
        { "name": "click", "description": "Click an element by @ref or CSS selector.",
          "input_schema": { "type": "object", "properties": { "target": { "type": "string" } }, "required": ["target"] } },
        { "name": "fill", "description": "Clear and fill an input by @ref or CSS selector.",
          "input_schema": { "type": "object", "properties": { "target": { "type": "string" }, "text": { "type": "string" } }, "required": ["target", "text"] } },
        { "name": "type", "description": "Append text into an element.",
          "input_schema": { "type": "object", "properties": { "target": { "type": "string" }, "text": { "type": "string" } }, "required": ["target", "text"] } },
        { "name": "press", "description": "Press a key chord at the current focus (Enter, Tab, Control+a).",
          "input_schema": { "type": "object", "properties": { "key": { "type": "string" } }, "required": ["key"] } },
        { "name": "get_text", "description": "Get visible text of the page, or of a selector if given.",
          "input_schema": { "type": "object", "properties": { "selector": { "type": "string" } } } },
        { "name": "get_url", "description": "Get the current page URL.",
          "input_schema": { "type": "object", "properties": {} } },
        { "name": "eval", "description": "Evaluate a JavaScript expression in the page.",
          "input_schema": { "type": "object", "properties": { "js": { "type": "string" } }, "required": ["js"] } },
        { "name": "scroll", "description": "Scroll the page up/down/left/right by an optional pixel amount.",
          "input_schema": { "type": "object", "properties": { "direction": { "type": "string" }, "amount": { "type": "integer" } }, "required": ["direction"] } }
    ])
}

/// Compact one-line rendering of a tool input for progress display.
fn brief(input: &Value) -> String {
    match input {
        Value::Object(m) if m.is_empty() => String::new(),
        Value::Object(m) => m
            .iter()
            .map(|(k, v)| format!("{k}={}", brief_str(&v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))))
            .collect::<Vec<_>>()
            .join(", "),
        other => brief_str(&other.to_string()),
    }
}

fn brief_str(s: &str) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() > 70 {
        format!("{}…", one.chars().take(70).collect::<String>())
    } else {
        one
    }
}
