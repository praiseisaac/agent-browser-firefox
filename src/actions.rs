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
