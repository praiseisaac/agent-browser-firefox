//! AI chat: natural-language browser control across multiple LLM providers.
//!
//! Drives the browser by looping a provider's chat API with tool use: snapshot
//! the page → the model chooses tools → execute them against the daemon → feed
//! results back → repeat until the model answers without a tool call.
//!
//! Providers: Anthropic, OpenAI (and OpenAI-compatible), Gemini. The agent keeps
//! one **normalized** history; each provider translates it into its own wire
//! format, so the loop logic is shared.
//!
//! Selection: `--provider`/`$ABF_PROVIDER`, else auto-detected from whichever
//! API key is set. Model: `--model`/`$ABF_MODEL`, else a per-provider default.

use crate::config::Config;
use crate::instance::InstanceRecord;
use crate::ipc::{self, Request};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;

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

// ---- normalized conversation types -----------------------------------------

#[derive(Clone)]
struct ToolCall {
    id: String,
    name: String,
    input: Value,
}

struct ToolResult {
    id: String,
    name: String,
    content: String,
}

enum Turn {
    User(String),
    Assistant { text: String, calls: Vec<ToolCall> },
    ToolResults(Vec<ToolResult>),
}

struct Step {
    text: String,
    calls: Vec<ToolCall>,
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    /// JSON Schema object: `{ "type": "object", "properties": {…}, "required": […] }`.
    schema: Value,
}

// ---- provider abstraction --------------------------------------------------

enum Provider {
    Anthropic { key: String, model: String },
    OpenAI { key: String, model: String, base: String },
    Gemini { key: String, model: String },
}

impl Provider {
    fn label(&self) -> String {
        match self {
            Provider::Anthropic { model, .. } => format!("anthropic/{model}"),
            Provider::OpenAI { model, .. } => format!("openai/{model}"),
            Provider::Gemini { model, .. } => format!("gemini/{model}"),
        }
    }

    /// One turn of the model: send the history, get back text + tool calls.
    async fn complete(
        &self,
        http: &reqwest::Client,
        tools: &[ToolDef],
        history: &[Turn],
    ) -> Result<Step, String> {
        match self {
            Provider::Anthropic { key, model } => {
                let body = json!({
                    "model": model,
                    "max_tokens": 2048,
                    "system": SYSTEM_PROMPT,
                    "tools": tools.iter().map(|t| json!({
                        "name": t.name, "description": t.description, "input_schema": t.schema,
                    })).collect::<Vec<_>>(),
                    "messages": anthropic_messages(history),
                });
                let v = post(http
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body))
                .await?;
                parse_anthropic(&v)
            }
            Provider::OpenAI { key, model, base } => {
                let body = json!({
                    "model": model,
                    "messages": openai_messages(history),
                    "tools": tools.iter().map(|t| json!({
                        "type": "function",
                        "function": { "name": t.name, "description": t.description, "parameters": t.schema },
                    })).collect::<Vec<_>>(),
                    "tool_choice": "auto",
                });
                let v = post(http
                    .post(format!("{}/chat/completions", base.trim_end_matches('/')))
                    .header("authorization", format!("Bearer {key}"))
                    .json(&body))
                .await?;
                parse_openai(&v)
            }
            Provider::Gemini { key, model } => {
                let url = format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
                );
                let body = json!({
                    "systemInstruction": { "parts": [{ "text": SYSTEM_PROMPT }] },
                    "tools": [{ "functionDeclarations": tools.iter().map(gemini_decl).collect::<Vec<_>>() }],
                    "contents": gemini_contents(history),
                });
                let v = post(http
                    .post(url)
                    .header("x-goog-api-key", key)
                    .json(&body))
                .await?;
                parse_gemini(&v)
            }
        }
    }
}

/// Send a request, decode JSON, and surface API-level errors.
async fn post(rb: reqwest::RequestBuilder) -> Result<Value, String> {
    let resp = rb
        .header("content-type", "application/json")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let v: Value = resp.json().await.map_err(|e| format!("bad response: {e}"))?;
    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(if msg.is_empty() { err.to_string() } else { msg.to_string() });
    }
    Ok(v)
}

// ---- Anthropic translation -------------------------------------------------

fn anthropic_messages(history: &[Turn]) -> Vec<Value> {
    let mut out = Vec::new();
    for turn in history {
        match turn {
            Turn::User(s) => out.push(json!({ "role": "user", "content": s })),
            Turn::Assistant { text, calls } => {
                let mut content = Vec::new();
                if !text.trim().is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
                for c in calls {
                    content.push(json!({ "type": "tool_use", "id": c.id, "name": c.name, "input": c.input }));
                }
                out.push(json!({ "role": "assistant", "content": content }));
            }
            Turn::ToolResults(results) => {
                let content: Vec<Value> = results
                    .iter()
                    .map(|r| json!({ "type": "tool_result", "tool_use_id": r.id, "content": r.content }))
                    .collect();
                out.push(json!({ "role": "user", "content": content }));
            }
        }
    }
    out
}

fn parse_anthropic(v: &Value) -> Result<Step, String> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for b in v.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => calls.push(ToolCall {
                id: b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                name: b.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                input: b.get("input").cloned().unwrap_or(json!({})),
            }),
            _ => {}
        }
    }
    Ok(Step { text, calls })
}

// ---- OpenAI translation ----------------------------------------------------

fn openai_messages(history: &[Turn]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": SYSTEM_PROMPT })];
    for turn in history {
        match turn {
            Turn::User(s) => out.push(json!({ "role": "user", "content": s })),
            Turn::Assistant { text, calls } => {
                let tool_calls: Vec<Value> = calls
                    .iter()
                    .map(|c| json!({
                        "id": c.id, "type": "function",
                        "function": { "name": c.name, "arguments": c.input.to_string() },
                    }))
                    .collect();
                let mut msg = json!({ "role": "assistant" });
                msg["content"] = if text.trim().is_empty() { Value::Null } else { json!(text) };
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            Turn::ToolResults(results) => {
                for r in results {
                    out.push(json!({ "role": "tool", "tool_call_id": r.id, "content": r.content }));
                }
            }
        }
    }
    out
}

fn parse_openai(v: &Value) -> Result<Step, String> {
    let msg = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .cloned()
        .ok_or("no choices in response")?;
    let text = msg.get("content").and_then(Value::as_str).unwrap_or("").to_string();
    let mut calls = Vec::new();
    for tc in msg.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default() {
        let f = tc.get("function").cloned().unwrap_or(Value::Null);
        let args = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
        calls.push(ToolCall {
            id: tc.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: f.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            input: serde_json::from_str(args).unwrap_or(json!({})),
        });
    }
    Ok(Step { text, calls })
}

// ---- Gemini translation ----------------------------------------------------

fn gemini_decl(t: &ToolDef) -> Value {
    let props = t.schema.get("properties").and_then(Value::as_object);
    // Gemini rejects function declarations with empty parameters; omit them.
    if props.map(|p| p.is_empty()).unwrap_or(true) {
        json!({ "name": t.name, "description": t.description })
    } else {
        json!({ "name": t.name, "description": t.description, "parameters": t.schema })
    }
}

fn gemini_contents(history: &[Turn]) -> Vec<Value> {
    let mut out = Vec::new();
    for turn in history {
        match turn {
            Turn::User(s) => out.push(json!({ "role": "user", "parts": [{ "text": s }] })),
            Turn::Assistant { text, calls } => {
                let mut parts = Vec::new();
                if !text.trim().is_empty() {
                    parts.push(json!({ "text": text }));
                }
                for c in calls {
                    parts.push(json!({ "functionCall": { "name": c.name, "args": c.input } }));
                }
                out.push(json!({ "role": "model", "parts": parts }));
            }
            Turn::ToolResults(results) => {
                let parts: Vec<Value> = results
                    .iter()
                    .map(|r| json!({ "functionResponse": { "name": r.name, "response": { "result": r.content } } }))
                    .collect();
                out.push(json!({ "role": "user", "parts": parts }));
            }
        }
    }
    out
}

fn parse_gemini(v: &Value) -> Result<Step, String> {
    let parts = v
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text = String::new();
    let mut calls = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if let Some(t) = p.get("text").and_then(Value::as_str) {
            text.push_str(t);
        }
        if let Some(fc) = p.get("functionCall") {
            let name = fc.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            calls.push(ToolCall {
                id: format!("{name}-{i}"), // Gemini has no call id; synthesize one.
                name,
                input: fc.get("args").cloned().unwrap_or(json!({})),
            });
        }
    }
    Ok(Step { text, calls })
}

// ---- provider selection ----------------------------------------------------

fn resolve_provider(flag_provider: Option<String>, flag_model: Option<String>) -> Result<Provider, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let model = flag_model.or_else(|| env("ABF_MODEL"));
    let choice = flag_provider
        .or_else(|| env("ABF_PROVIDER"))
        .or_else(|| {
            if env("ANTHROPIC_API_KEY").is_some() {
                Some("anthropic".into())
            } else if env("OPENAI_API_KEY").is_some() {
                Some("openai".into())
            } else if env("GEMINI_API_KEY").or_else(|| env("GOOGLE_API_KEY")).is_some() {
                Some("gemini".into())
            } else {
                None
            }
        })
        .ok_or("no provider configured — set ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY")?;

    match choice.to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Ok(Provider::Anthropic {
            key: env("ANTHROPIC_API_KEY").ok_or("set ANTHROPIC_API_KEY")?,
            model: model.unwrap_or_else(|| "claude-sonnet-4-6".into()),
        }),
        "openai" | "gpt" => Ok(Provider::OpenAI {
            key: env("OPENAI_API_KEY").ok_or("set OPENAI_API_KEY")?,
            model: model.unwrap_or_else(|| "gpt-4o".into()),
            base: env("ABF_OPENAI_BASE").unwrap_or_else(|| "https://api.openai.com/v1".into()),
        }),
        "gemini" | "google" => Ok(Provider::Gemini {
            key: env("GEMINI_API_KEY")
                .or_else(|| env("GOOGLE_API_KEY"))
                .ok_or("set GEMINI_API_KEY")?,
            model: model.unwrap_or_else(|| "gemini-2.0-flash".into()),
        }),
        other => Err(format!("unknown provider '{other}' (anthropic/openai/gemini)")),
    }
}

// ---- entry point + agent loop ----------------------------------------------

/// Entry point for the `chat` command.
pub async fn run(
    cfg: &Config,
    instruction: Option<String>,
    provider: Option<String>,
    model: Option<String>,
) -> i32 {
    let provider = match resolve_provider(provider, model) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
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
    eprintln!("\x1b[2musing {}\x1b[0m", provider.label());

    let mut agent = Agent {
        http: reqwest::Client::new(),
        provider,
        rec,
        history: Vec::new(),
    };

    match instruction {
        Some(text) => agent.turn(&text).await,
        None => agent.repl().await,
    }
}

struct Agent {
    http: reqwest::Client,
    provider: Provider,
    rec: InstanceRecord,
    history: Vec<Turn>,
}

impl Agent {
    async fn repl(&mut self) -> i32 {
        println!("agent-browser-firefox chat — type a goal, or 'exit' to quit.");
        let stdin = std::io::stdin();
        loop {
            print!("\x1b[1;36m›\x1b[0m ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if stdin.read_line(&mut line).unwrap_or(0) == 0 {
                break;
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

    async fn turn(&mut self, instruction: &str) -> i32 {
        self.history.push(Turn::User(instruction.to_string()));
        let tools = tools();

        for _ in 0..MAX_STEPS {
            let step = match self.provider.complete(&self.http, &tools, &self.history).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    return 1;
                }
            };
            if !step.text.trim().is_empty() {
                println!("{}", step.text);
            }
            if step.calls.is_empty() {
                self.history.push(Turn::Assistant { text: step.text, calls: vec![] });
                return 0;
            }
            self.history.push(Turn::Assistant {
                text: step.text.clone(),
                calls: step.calls.clone(),
            });

            let mut results = Vec::new();
            for call in &step.calls {
                let out = self.exec_tool(&call.name, &call.input).await;
                println!("  \x1b[2m↳ {}({}) → {}\x1b[0m", call.name, brief(&call.input), brief_str(&out));
                results.push(ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    content: out,
                });
            }
            self.history.push(Turn::ToolResults(results));
        }

        eprintln!("(stopped after {MAX_STEPS} steps)");
        0
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

/// Tool set advertised to the model (provider-neutral JSON Schema).
fn tools() -> Vec<ToolDef> {
    let str_prop = |req: &[&str], props: Value| json!({ "type": "object", "properties": props, "required": req });
    vec![
        ToolDef { name: "navigate", description: "Navigate to a URL.",
            schema: str_prop(&["url"], json!({ "url": { "type": "string" } })) },
        ToolDef { name: "snapshot", description: "Get the page as a list of elements with @refs. Do this before acting.",
            schema: json!({ "type": "object", "properties": {} }) },
        ToolDef { name: "click", description: "Click an element by @ref or CSS selector.",
            schema: str_prop(&["target"], json!({ "target": { "type": "string" } })) },
        ToolDef { name: "fill", description: "Clear and fill an input by @ref or CSS selector.",
            schema: str_prop(&["target", "text"], json!({ "target": { "type": "string" }, "text": { "type": "string" } })) },
        ToolDef { name: "type", description: "Append text into an element.",
            schema: str_prop(&["target", "text"], json!({ "target": { "type": "string" }, "text": { "type": "string" } })) },
        ToolDef { name: "press", description: "Press a key chord at the current focus (Enter, Tab, Control+a).",
            schema: str_prop(&["key"], json!({ "key": { "type": "string" } })) },
        ToolDef { name: "get_text", description: "Get visible text of the page, or of a selector if given.",
            schema: json!({ "type": "object", "properties": { "selector": { "type": "string" } } }) },
        ToolDef { name: "get_url", description: "Get the current page URL.",
            schema: json!({ "type": "object", "properties": {} }) },
        ToolDef { name: "eval", description: "Evaluate a JavaScript expression in the page.",
            schema: str_prop(&["js"], json!({ "js": { "type": "string" } })) },
        ToolDef { name: "scroll", description: "Scroll the page up/down/left/right by an optional pixel amount.",
            schema: str_prop(&["direction"], json!({ "direction": { "type": "string" }, "amount": { "type": "integer" } })) },
    ]
}

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
