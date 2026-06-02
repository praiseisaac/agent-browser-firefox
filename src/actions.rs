//! Maps IPC [`Request`]s to operations on a [`BidiSession`]. This is the
//! backend-agnostic action layer; today it speaks BiDi, but the surface mirrors
//! agent-browser so a CLI user sees the same commands.

use crate::bidi::BidiSession;
use crate::ipc::{Request, Response};
use serde_json::{json, Value};

/// Run one action against the session.
pub async fn handle(session: &BidiSession, req: &Request) -> Response {
    match req.action.as_str() {
        "navigate" | "open" | "goto" => navigate(session, req).await,
        "click" => click(session, req).await,
        "fill" => fill(session, req).await,
        "type" => type_text(session, req).await,
        "press" => press(session, req).await,
        "keydown" => key_hold(session, req, true).await,
        "keyup" => key_hold(session, req, false).await,
        "keyboard" => keyboard(session, req).await,
        "hover" => hover(session, req).await,
        "dblclick" => dblclick(session, req).await,
        "drag" => drag(session, req).await,
        "scroll" => scroll(session, req).await,
        "wait" => wait(session, req).await,
        "get" => get(session, req).await,
        "eval" => eval(session, req).await,
        "screenshot" => screenshot(session, req).await,
        "snapshot" => snapshot(session).await,
        "status" => status(session).await,
        // `close` is handled by the daemon loop (it must also tear down).
        other => Response::err(format!("unknown action '{other}'")),
    }
}

async fn navigate(session: &BidiSession, req: &Request) -> Response {
    let Some(raw) = req.args.first() else {
        // open with no URL: stay on the current/blank page.
        return match session.get_url().await {
            Ok(u) => Response::ok_text(u),
            Err(e) => Response::err(e),
        };
    };
    let url = normalize_url(raw);
    match session.navigate(&url).await {
        Ok(final_url) => {
            let title = session.get_title().await.unwrap_or_default();
            Response::ok_data(
                Some(final_url.clone()),
                json!({ "url": final_url, "title": title }),
            )
        }
        Err(e) => Response::err(e),
    }
}

async fn click(session: &BidiSession, req: &Request) -> Response {
    let Some(sel) = req.args.first() else {
        return Response::err("usage: click <selector|@ref>");
    };
    match session.click(&resolve_selector(sel)).await {
        Ok(()) => Response::ok_text(format!("clicked {sel}")),
        Err(e) => Response::err(e),
    }
}

async fn fill(session: &BidiSession, req: &Request) -> Response {
    let (Some(sel), Some(val)) = (req.args.first(), req.args.get(1)) else {
        return Response::err("usage: fill <selector|@ref> <text>");
    };
    match session.fill(&resolve_selector(sel), val).await {
        Ok(()) => Response::ok_text(format!("filled {sel}")),
        Err(e) => Response::err(e),
    }
}

async fn type_text(session: &BidiSession, req: &Request) -> Response {
    let (Some(sel), Some(val)) = (req.args.first(), req.args.get(1)) else {
        return Response::err("usage: type <selector|@ref> <text>");
    };
    let css = resolve_selector(sel);
    let expr = format!(
        "(() => {{ const el = document.querySelector({s}); if(!el) throw new Error('no element matches'); el.focus(); if('value' in el){{ el.value += {v}; }} else {{ el.textContent += {v}; }} el.dispatchEvent(new Event('input',{{bubbles:true}})); return true; }})()",
        s = js_str(&css),
        v = js_str(val)
    );
    match session.evaluate(&expr).await {
        Ok(_) => Response::ok_text(format!("typed into {sel}")),
        Err(e) => Response::err(e),
    }
}

async fn get(session: &BidiSession, req: &Request) -> Response {
    let kind = req.args.first().map(String::as_str).unwrap_or("");
    match kind {
        "url" => match session.get_url().await {
            Ok(v) => Response::ok_data(Some(v.clone()), json!({ "url": v })),
            Err(e) => Response::err(e),
        },
        "title" => match session.get_title().await {
            Ok(v) => Response::ok_data(Some(v.clone()), json!({ "title": v })),
            Err(e) => Response::err(e),
        },
        "html" => {
            let sel = req.args.get(1);
            let expr = match sel {
                Some(s) => format!(
                    "(() => {{ const el = document.querySelector({s}); return el ? el.innerHTML : null; }})()",
                    s = js_str(&resolve_selector(s))
                ),
                None => "document.documentElement.outerHTML".to_string(),
            };
            text_result(session, &expr).await
        }
        "text" => {
            let sel = req.args.get(1);
            let expr = match sel {
                Some(s) => format!(
                    "(() => {{ const el = document.querySelector({s}); return el ? el.innerText : null; }})()",
                    s = js_str(&resolve_selector(s))
                ),
                None => "document.body ? document.body.innerText : ''".to_string(),
            };
            text_result(session, &expr).await
        }
        "value" => {
            let Some(s) = req.args.get(1) else {
                return Response::err("usage: get value <selector|@ref>");
            };
            let expr = format!(
                "(() => {{ const el = document.querySelector({s}); return el ? el.value : null; }})()",
                s = js_str(&resolve_selector(s))
            );
            text_result(session, &expr).await
        }
        "attr" => {
            let (Some(s), Some(a)) = (req.args.get(1), req.args.get(2)) else {
                return Response::err("usage: get attr <selector|@ref> <attribute>");
            };
            let expr = format!(
                "(() => {{ const el = document.querySelector({s}); return el ? el.getAttribute({a}) : null; }})()",
                s = js_str(&resolve_selector(s)),
                a = js_str(a)
            );
            text_result(session, &expr).await
        }
        other => Response::err(format!(
            "unknown get target '{other}' (try: url, title, text, html, value, attr)"
        )),
    }
}

async fn eval(session: &BidiSession, req: &Request) -> Response {
    let Some(js) = req.args.first() else {
        return Response::err("usage: eval <javascript>");
    };
    match session.evaluate(js).await {
        Ok(v) => {
            let text = match &v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Response::ok_data(Some(text), v)
        }
        Err(e) => Response::err(e),
    }
}

async fn screenshot(session: &BidiSession, _req: &Request) -> Response {
    match session.screenshot().await {
        // Daemon returns base64; the CLI decodes and writes the file.
        Ok(b64) => Response::ok_data(None, json!({ "base64": b64 })),
        Err(e) => Response::err(e),
    }
}

async fn status(session: &BidiSession) -> Response {
    let url = session.get_url().await.unwrap_or_default();
    let title = session.get_title().await.unwrap_or_default();
    Response::ok_data(
        Some(format!("{title}\n{url}")),
        json!({ "url": url, "title": title, "context": session.cached_context(), "session": session.session_id }),
    )
}

/// Accessibility-ish DOM snapshot: assigns `@eN` refs (stored as
/// `data-abf-ref` attributes) to visible interactive elements and returns a
/// compact tree. Later `click @e3` resolves via the persisted attribute.
async fn snapshot(session: &BidiSession) -> Response {
    let js = r##"(() => {
  document.querySelectorAll('[data-abf-ref]').forEach(e => e.removeAttribute('data-abf-ref'));
  const sels = 'a,button,input,select,textarea,[role],[onclick],summary,[contenteditable="true"],h1,h2,h3';
  const visible = (el) => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'; };
  const lines = [];
  let n = 0;
  document.querySelectorAll(sels).forEach(el => {
    if (!visible(el)) return;
    n++;
    const ref = 'e' + n;
    el.setAttribute('data-abf-ref', ref);
    const role = el.getAttribute('role') || el.tagName.toLowerCase();
    let name = (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.value || el.innerText || el.getAttribute('alt') || el.getAttribute('title') || '').trim().replace(/\s+/g, ' ').slice(0, 80);
    lines.push('[@' + ref + '] ' + role + (name ? ' "' + name + '"' : ''));
  });
  return lines.join('\n');
})()"##;
    match session.evaluate(js).await {
        Ok(v) => {
            let text = v.as_str().unwrap_or("").to_string();
            Response::ok_data(Some(text.clone()), json!({ "snapshot": text }))
        }
        Err(e) => Response::err(e),
    }
}

async fn text_result(session: &BidiSession, expr: &str) -> Response {
    match session.evaluate(expr).await {
        Ok(Value::Null) => Response::err("no element matched"),
        Ok(v) => {
            let text = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
            Response::ok_data(Some(text), v)
        }
        Err(e) => Response::err(e),
    }
}

// ---- real input ------------------------------------------------------------

async fn press(session: &BidiSession, req: &Request) -> Response {
    let Some(key) = req.args.first() else {
        return Response::err("usage: press <key>  (e.g. Enter, Tab, Control+a)");
    };
    match session.press(key).await {
        Ok(()) => Response::ok_text(format!("pressed {key}")),
        Err(e) => Response::err(e),
    }
}

async fn key_hold(session: &BidiSession, req: &Request, down: bool) -> Response {
    let Some(key) = req.args.first() else {
        return Response::err("usage: keydown/keyup <key>");
    };
    match session.key_hold(key, down).await {
        Ok(()) => Response::ok_text(format!("{} {key}", if down { "keydown" } else { "keyup" })),
        Err(e) => Response::err(e),
    }
}

async fn keyboard(session: &BidiSession, req: &Request) -> Response {
    let sub = req.args.first().map(String::as_str).unwrap_or("");
    let text = req.args.get(1..).map(|s| s.join(" ")).unwrap_or_default();
    match sub {
        "type" => match session.type_keys(&text).await {
            Ok(()) => Response::ok_text("typed"),
            Err(e) => Response::err(e),
        },
        "inserttext" => match session.insert_text(&text).await {
            Ok(()) => Response::ok_text("inserted"),
            Err(e) => Response::err(e),
        },
        _ => Response::err("usage: keyboard <type|inserttext> <text>"),
    }
}

async fn hover(session: &BidiSession, req: &Request) -> Response {
    let Some(sel) = req.args.first() else {
        return Response::err("usage: hover <selector|@ref>");
    };
    match session.hover(&resolve_selector(sel)).await {
        Ok(()) => Response::ok_text(format!("hovered {sel}")),
        Err(e) => Response::err(e),
    }
}

async fn dblclick(session: &BidiSession, req: &Request) -> Response {
    let Some(sel) = req.args.first() else {
        return Response::err("usage: dblclick <selector|@ref>");
    };
    match session.dblclick(&resolve_selector(sel)).await {
        Ok(()) => Response::ok_text(format!("double-clicked {sel}")),
        Err(e) => Response::err(e),
    }
}

async fn drag(session: &BidiSession, req: &Request) -> Response {
    let (Some(src), Some(tgt)) = (req.args.first(), req.args.get(1)) else {
        return Response::err("usage: drag <source> <target>");
    };
    match session.drag(&resolve_selector(src), &resolve_selector(tgt)).await {
        Ok(()) => Response::ok_text(format!("dragged {src} → {tgt}")),
        Err(e) => Response::err(e),
    }
}

async fn scroll(session: &BidiSession, req: &Request) -> Response {
    let dir = req.args.first().map(String::as_str).unwrap_or("down");
    let amount = req
        .args
        .get(1)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(400);
    let selector = req.flag("selector").map(resolve_selector);
    match session.scroll(dir, amount, selector.as_deref()).await {
        Ok(()) => Response::ok_text(format!("scrolled {dir} {amount}")),
        Err(e) => Response::err(e),
    }
}

// ---- waiting ---------------------------------------------------------------

async fn wait(session: &BidiSession, req: &Request) -> Response {
    let timeout = std::time::Duration::from_millis(
        req.flag("timeout").and_then(|s| s.parse().ok()).unwrap_or(30_000),
    );

    // wait <ms>
    if let Some(arg) = req.args.first() {
        if let Ok(ms) = arg.parse::<u64>() {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            return Response::ok_text(format!("waited {ms}ms"));
        }
    }

    // wait --url <glob>
    if let Some(pat) = req.flag("url") {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let url = session.get_url().await.unwrap_or_default();
            if wildcard_match(pat, &url) {
                return Response::ok_text(format!("url matched: {url}"));
            }
            if tokio::time::Instant::now() >= deadline {
                return Response::err(format!("timed out waiting for url ~ {pat}"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    }

    // condition-based waits (poll a JS boolean)
    let (expr, label) = if let Some(text) = req.flag("text") {
        (
            format!(
                "!!(document.body && document.body.innerText.includes({}))",
                js_str(text)
            ),
            format!("text \"{text}\""),
        )
    } else if let Some(f) = req.flag("fn") {
        (format!("!!({f})"), "condition".to_string())
    } else if let Some(load) = req.flag("load") {
        let e = match load {
            "domcontentloaded" => "['interactive','complete'].includes(document.readyState)",
            _ => "document.readyState === 'complete'",
        };
        (e.to_string(), format!("load:{load}"))
    } else if let Some(sel) = req.args.first() {
        let css = resolve_selector(sel);
        let visible = format!(
            "(() => {{ const el = document.querySelector({s}); if (!el) return false; const r = el.getBoundingClientRect(); const st = getComputedStyle(el); return r.width>0 && r.height>0 && st.visibility!=='hidden' && st.display!=='none'; }})()",
            s = js_str(&css)
        );
        if req.flag("state") == Some("hidden") {
            (format!("!({visible})"), format!("{sel} hidden"))
        } else {
            (visible, format!("{sel} visible"))
        }
    } else {
        return Response::err("usage: wait <ms|selector> | --text|--url|--fn|--load <…>");
    };

    match session.wait_for_js(&expr, timeout).await {
        Ok(true) => Response::ok_text(format!("ready: {label}")),
        Ok(false) => Response::err(format!("timed out waiting for {label}")),
        Err(e) => Response::err(e),
    }
}

/// Minimal glob matcher supporting `*` (any run) and `?` (one char).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn m(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => m(&p[1..], t) || (!t.is_empty() && m(p, &t[1..])),
            Some(b'?') => !t.is_empty() && m(&p[1..], &t[1..]),
            Some(&c) => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
        }
    }
    // A bare pattern with no wildcard is treated as a substring match.
    if !pattern.contains('*') && !pattern.contains('?') {
        return text.contains(pattern);
    }
    m(pattern.as_bytes(), text.as_bytes())
}

/// `example.com` → `https://example.com`; leaves explicit schemes and
/// `about:`/`file:` URLs untouched.
pub fn normalize_url(raw: &str) -> String {
    let r = raw.trim();
    if r.contains("://") || r.starts_with("about:") || r.starts_with("data:") || r.starts_with("file:") {
        r.to_string()
    } else {
        format!("https://{r}")
    }
}

/// Resolve an `@ref` to its persisted attribute selector; pass CSS through.
fn resolve_selector(sel: &str) -> String {
    if let Some(rest) = sel.strip_prefix('@') {
        format!("[data-abf-ref=\"{rest}\"]")
    } else {
        sel.to_string()
    }
}

/// JSON-encode a string for embedding in JS source.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}
