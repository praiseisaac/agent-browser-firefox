# agent-browser-firefox — flow breakdown

Open **[`flow.html`](./flow.html)** in a browser for the illustrated version (inline SVG
flowcharts). This file is the same breakdown in Markdown.

A Firefox-native browser automation CLI for AI agents. It drives Firefox over
**WebDriver BiDi** — the Firefox counterpart to agent-browser's Chrome/CDP attachment.

---

## 1. System architecture

Each CLI command is a short-lived process. The browser session lives in a long-running
**daemon**, so state (the open tab, `@refs`) survives across invocations.

```
 CLI invocations            daemon (per instance)              Firefox (headless)
 ┌───────────────┐  socket  ┌──────────────────────┐  BiDi    ┌────────────────────┐
 │ open …        │ ───────▶ │ Unix socket listener │ ───────▶ │ Remote agent       │
 │ snapshot      │  JSON    │ BiDi session (tab)    │  ws://   │ (--remote-debug…)  │
 │ click @e2     │          │ actions dispatch      │          │ Dedicated profile  │
 │ get title     │          └──────────────────────┘          │ Page / DOM (@refs) │
 │ close         │   spawns daemon on demand ↑                 └────────────────────┘
 └───────────────┘
```

- **CLI → daemon:** line-delimited JSON over a Unix domain socket (`abf-<id>.sock`).
- **daemon → Firefox:** WebDriver BiDi over a WebSocket.

## 2. Command flow & the “no-restart” rule

Every command calls `ensure_daemon`:

1. **Daemon alive & answers ping?** → **REUSE** the live session. *Same Firefox, no restart.*
2. **Daemon gone?** → the Firefox BiDi session is **orphaned** (Firefox binds its single
   allowed session to the now-closed socket and won't let anyone reuse or recreate it), so
   reap the stuck browser and clear the record.
3. Then, if the command is `open` → **cold start** a fresh instance; otherwise return
   `error: no session — run open first`.

> **Why this matters:** while the daemon lives, running `open → snapshot → click → close`
> as separate processes all reuse one browser. A restart can only happen *after* a daemon
> dies — that is a hard limitation of Firefox's one-session-per-connection BiDi model, not
> a bug.

## 3. BiDi handshake (cold start)

Discovery is **deterministic** — we choose the port, so the endpoint is
`ws://127.0.0.1:<port>/session`. We poll the TCP port instead of parsing stderr (on macOS
the Firefox launcher closes its inherited stderr right after startup, so an stderr EOF is
*not* a reliable "exited" signal).

```
daemon                                   Firefox remote agent
  │  spawn --remote-debugging-port P …          │
  │ ───────────────────────────────────────────▶│
  │  poll TCP 127.0.0.1:P  (until accepting)     │
  │ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─▶│
  │  WebSocket connect  ws://127.0.0.1:P/session │
  │ ───────────────────────────────────────────▶│
  │  → session.new { capabilities:{} }           │
  │ ───────────────────────────────────────────▶│
  │  ← success { sessionId }                     │
  │ ◀───────────────────────────────────────────│
  │  → browsingContext.getTree  (bind first tab) │
  │ ───────────────────────────────────────────▶│
  │  → navigate · script.evaluate · screenshot   │
  │ ───────────────────────────────────────────▶│
  │  ← results (matched to request by id)        │
  │ ◀───────────────────────────────────────────│
```

Requests carry an auto-incrementing `id`; the reader task matches each response back to the
waiting caller. Events (`type: "event"`) fan out on a broadcast channel.

## 4. Instance lifecycle

Instances are addressable by **ID** (`--session` name) and persisted to a registry on disk,
so they can be listed, resumed, and closed from any terminal.

| State | Meaning | Leaves via |
|-------|---------|-----------|
| **No instance** | no record on disk | `open` → cold start → *Running* |
| **Running** | daemon + Firefox alive | any command = reuse · `close` → cleaned up |
| **Orphaned** | daemon dead, session stuck | next command → reap + clear |

Registry record (one JSON file per instance) stores: `id`, `port`, `bidiUrl`,
`firefoxPid`, `daemonPid`, `socket`, `profileDir`, `headless`, `createdAt`, `lastUrl`.

## 5. Configuration precedence

Every setting has a default and can be overridden at four layers — later wins:

```
Built-in defaults  →  Config file  →  Environment  →  CLI flags
(port 9222,           agent-browser-   ABF_PORT,        --port,
 headless)            firefox.json     FIREFOX_BIN …    --headed …
```

- **Config file** lookup: `$ABF_CONFIG`, else `./agent-browser-firefox.json`, else
  `~/.config/agent-browser-firefox/config.json`.
- **Env vars:** `FIREFOX_BIN`, `ABF_PORT`, `ABF_HEADLESS`, `ABF_PROFILE_DIR`, `ABF_SESSION`.

## 6. Isolation from your personal Firefox

The automation browser never touches your daily Firefox:

- a **dedicated temp profile** per session,
- `--no-remote` + `MOZ_NO_REMOTE=1` (never adopt a running instance),
- `--new-instance`,
- **headless by default** (no window),
- a generated `user.js` that disables first-run, updates, default-browser nags, and telemetry.
