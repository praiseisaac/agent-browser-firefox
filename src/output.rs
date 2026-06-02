//! Output formatting. Plain text by default; structured JSON with `--json`,
//! mirroring agent-browser's dual output style.

use crate::ipc::Response;

/// Print a daemon response and return the process exit code.
pub fn print(resp: &Response, json: bool) -> i32 {
    if json {
        let payload = serde_json::json!({
            "ok": resp.ok,
            "data": resp.data,
            "text": resp.text,
            "error": resp.error,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return if resp.ok { 0 } else { 1 };
    }

    if !resp.ok {
        eprintln!("error: {}", resp.error.as_deref().unwrap_or("unknown error"));
        return 1;
    }
    if let Some(text) = &resp.text {
        if !text.is_empty() {
            println!("{text}");
        }
    }
    0
}
