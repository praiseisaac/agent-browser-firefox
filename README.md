# agent-browser-firefox

> **AI Use Disclosure:** I didn't build this. It was bootstrapped and built using Claude Code because I needed something quick for my testing.

> Browser automation CLI for AI agents — **Firefox edition**.

A fast, native **Rust** CLI that drives Firefox over **WebDriver BiDi** — the Firefox
counterpart to [agent-browser](https://github.com/vercel-labs/agent-browser)'s Chrome/CDP
attachment. Its command surface mirrors agent-browser (`open` → `snapshot` → `click @e2` →
`close`), so the same muscle memory works against Firefox.

```bash
agent-browser-firefox open example.com
agent-browser-firefox snapshot      # [@e1] h1 "Example Domain"  /  [@e2] a "Learn more"
agent-browser-firefox click @e2     # → navigates to iana.org
```

## Contents

- [Why BiDi](#why-bidi)
- [Features](#features)
- [Install](#install)
- [Quick start](#quick-start)
- [How it works](#how-it-works)
- [Commands](#commands)
- [Examples](#examples)
- [Configuration](#configuration)
- [Isolation](#isolation)
- [Troubleshooting](#troubleshooting)
- [Roadmap](#roadmap)

## Why BiDi

Firefox's legacy CDP shim is being removed by Mozilla. **WebDriver BiDi** is the modern,
bidirectional WebSocket protocol Firefox fully supports — `id`/`method`/`params` requests and
`type: success | error | event` responses, structurally very close to CDP, but future-proof
and standardized.

## Features

- **Mirrors agent-browser** — `open`, `snapshot`, `click`, `fill`, `get`, `eval`, `screenshot`, …
- **Snapshot with `@refs`** — accessibility-style DOM tree an agent can act on by reference.
- **Stateful across invocations** — a background **daemon** holds the session, so separate
  commands reuse the *same* browser (no restarts).
- **Many browsers at once** — each addressable by `--session` id, fully isolated, with `list`
  and `close --all`.
- **Headless or headed** — `--headed` for a visible window.
- **Dual output** — human-readable by default, `--json` for machines.
- **Layered config** — sane defaults, overridable by config file, env vars, and flags.
- **One-command setup** — installs Firefox for you if it's missing.
- **Strong isolation** — never touches your personal Firefox profile.

## Install

### One-liner (recommended)

Ensures Rust, builds the binary onto your PATH, and installs Firefox if missing:

```bash
curl -fsSL https://raw.githubusercontent.com/praiseisaac/agent-browser-firefox/main/install.sh | sh
```

### From a checkout

```bash
git clone https://github.com/praiseisaac/agent-browser-firefox
cd agent-browser-firefox
./install.sh
```

The binary lands in `~/.local/bin` (override with `ABF_INSTALL_DIR`). If that's not on your
PATH yet: `export PATH="$HOME/.local/bin:$PATH"`.

### Manual build

```bash
cargo build --release             # → ./target/release/agent-browser-firefox
agent-browser-firefox install     # ensure Firefox is present
```

### Requirements

- **Rust** to build (the installer sets this up): https://rustup.rs
- **Firefox** — Nightly, Developer Edition, or stable. Auto-detected; override with
  `--firefox-bin` or `$FIREFOX_BIN`. `agent-browser-firefox install` will fetch it via your
  package manager (Homebrew on macOS; apt/dnf/pacman/zypper on Linux) if it's missing.

## Quick start

```bash
agent-browser-firefox open example.com      # launch Firefox + navigate
agent-browser-firefox snapshot              # DOM tree with @refs
agent-browser-firefox click @e2             # click by ref
agent-browser-firefox fill "#email" "a@b.c" # fill by selector
agent-browser-firefox get title             # read info
agent-browser-firefox eval "document.title" # run JavaScript
agent-browser-firefox screenshot page.png   # PNG screenshot
agent-browser-firefox close                 # shut down
```

## How it works

Each CLI command is a short-lived process. The browser session lives in a long-running
**daemon** (one per instance), so state — the open tab, snapshot `@refs` — survives across
invocations.

```
 CLI command  ──JSON over Unix socket──▶  daemon  ──WebDriver BiDi (ws://)──▶  Firefox
 (ephemeral)                              (holds the session)                  (headless)
```

- While the daemon is alive, every command **reuses the same Firefox** — it is never
  restarted.
- A restart only happens if the daemon itself dies, because Firefox binds its single allowed
  BiDi session to the daemon's WebSocket and orphans it on disconnect — so the tool reaps the
  stuck browser and cold-starts cleanly.

See **[docs/flow.html](docs/flow.html)** for illustrated architecture and flow diagrams (or
[docs/README.md](docs/README.md) for the Markdown version).

## Commands

| Command | Description |
|---------|-------------|
| `open [url]` | Launch/reuse instance; navigate if URL given (aliases: `goto`, `navigate`) |
| `snapshot` | Accessibility-style DOM tree with `@eN` refs |
| `click <sel\|@ref>` | Click element (focuses it first) |
| `fill <sel> <text>` | Clear and fill input |
| `type <sel> <text>` | Append text to element |
| `press <key>` | Press a key chord at focus (`Enter`, `Tab`, `Control+a`) (alias: `key`) |
| `keydown <key>` / `keyup <key>` | Hold / release a key |
| `keyboard <type\|inserttext> <text>` | Real keystrokes / insert text at focus |
| `hover <sel>` | Move the mouse over an element |
| `dblclick <sel>` | Double-click an element |
| `drag <src> <tgt>` | Drag one element onto another |
| `scroll <up\|down\|left\|right> [px]` | Scroll page (`--selector` to scroll over an element) |
| `wait <ms\|sel>` | Wait: `--text`, `--url <glob>`, `--fn <js>`, `--load`, `--state hidden`, `--timeout` |
| `find <role\|text\|label\|placeholder\|alt\|title\|testid\|first\|last\|nth> <value> <action> [value]` | Locate by meaning + act (`--name`, `--exact`) |
| `get <url\|title\|text\|html\|value\|attr> [sel]` | Read page/element info |
| `eval <js>` | Evaluate JavaScript, returns the value |
| `screenshot [path]` | Capture PNG (defaults to a temp path) |
| `status` | Current title + URL for the instance |
| `list` | List all instances + liveness (alias: `ls`) |
| `close [--all]` | Close this instance (or every instance) (aliases: `quit`, `exit`) |
| `config` | Show resolved configuration |
| `install [--with-deps]` | Ensure Firefox is installed |

**Global flags:** `-s/--session <id>`, `-p/--port <n>`, `--firefox-bin <path>`,
`--profile <dir>`, `--headless`/`--headed`, `--json`.

> Place global flags **before** the subcommand (e.g. `--json get title`), since `get`/`eval`
> capture trailing arguments.

## Examples

### A real workflow (search → click result → read)

```bash
agent-browser-firefox open https://en.wikipedia.org
agent-browser-firefox fill "#searchInput" "WebDriver BiDi"
agent-browser-firefox eval "document.querySelector('#searchInput').form.submit()"
agent-browser-firefox click ".mw-search-result-heading a"
agent-browser-firefox get text h1            # → "Headless browser"
agent-browser-firefox screenshot result.png
```

### Multiple concurrent browsers

```bash
agent-browser-firefox -s research -p 9301 open https://arxiv.org
agent-browser-firefox -s shopping -p 9302 open https://example.com
agent-browser-firefox -s research click @e5  # routed to the research daemon only
agent-browser-firefox list                   # show all instances
agent-browser-firefox close --all            # close every instance
```

### Structured output for scripts/agents

```bash
agent-browser-firefox --json status
# { "ok": true, "data": { "url": "...", "title": "...", "context": "...", "session": "..." } }
```

## Configuration

Settings resolve across four layers (**later wins**): defaults → config file → env → CLI flags.

Config file lookup order:
1. `$ABF_CONFIG`
2. `./agent-browser-firefox.json`
3. `~/.config/agent-browser-firefox/config.json`

Example ([`agent-browser-firefox.example.json`](agent-browser-firefox.example.json)):

```json
{
  "firefoxBin": "/Applications/Firefox Nightly.app/Contents/MacOS/firefox",
  "port": 9222,
  "headless": true,
  "session": "default"
}
```

Environment variables: `FIREFOX_BIN`, `ABF_PORT`, `ABF_HEADLESS`, `ABF_PROFILE_DIR`,
`ABF_SESSION`, `ABF_CONFIG`. Run `agent-browser-firefox config` to see what resolved.

## Isolation

The automation browser never disturbs your personal Firefox:

- a **dedicated temp profile** per session,
- `--no-remote` + `MOZ_NO_REMOTE` (never adopts a running instance),
- `--new-instance`,
- **headless by default** (no window),
- a generated `user.js` that disables first-run, updates, default-browser nags, and telemetry.

## Troubleshooting

- **"daemon did not become ready"** — Firefox couldn't start the remote agent. Check the
  per-session log at `$TMPDIR/abf-daemon-<session>.log`, and that the `--port` isn't already
  in use.
- **Two sessions collide** — give each a distinct `--port` (default is `9222`).
- **Firefox not found** — run `agent-browser-firefox install`, or set `$FIREFOX_BIN` /
  `--firefox-bin`.
- **A command hangs** — navigations time out after 45s and other commands after 30s; a hung
  page returns a timeout error rather than blocking forever.

## Roadmap

Working today: launch/attach, navigate, click/fill/type, get, eval, snapshot + `@refs`,
screenshot, multi-instance, config, one-command install.

Natural next steps on the BiDi foundation:

- **Real input** via `input.performActions` — `press`, `hover`, `drag`, true keystrokes.
- **Waits** — for selector/text/URL/load-state/JS condition.
- **Semantic locators** — `find role/text/label/placeholder/testid`.
- **Network** — intercept/block/mock requests, HAR capture, console/error logs.
- **AI chat** — natural-language control wiring snapshot → Claude API → actions.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
