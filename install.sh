#!/usr/bin/env sh
# agent-browser-firefox installer.
#
#   From a checkout:   ./install.sh
#   From anywhere:     curl -fsSL <raw-url>/install.sh | sh
#
# Steps: ensure Rust → get the source (current dir or clone) → build release →
# install the binary onto PATH → ensure Firefox is present.
#
# Env overrides:
#   ABF_REPO         git URL to clone when not run inside a checkout
#   ABF_INSTALL_DIR  where to put the binary   (default: ~/.local/bin)
set -eu

REPO_URL="${ABF_REPO:-https://github.com/praiseisaac/agent-browser-firefox.git}"
INSTALL_DIR="${ABF_INSTALL_DIR:-$HOME/.local/bin}"
BIN="agent-browser-firefox"

info() { printf '\033[1;34m•\033[0m %s\n' "$1"; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$1"; }
err()  { printf '\033[1;31m✗\033[0m %s\n' "$1" >&2; }

# 1. Rust toolchain ----------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  info "Rust not found — installing via rustup (non-interactive)…"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
ok "Rust: $(cargo --version)"

# 2. Source location ---------------------------------------------------------
CLEANUP=""
if [ -f "Cargo.toml" ] && grep -q 'name = "agent-browser-firefox"' Cargo.toml 2>/dev/null; then
  SRC="$(pwd)"
  info "Building from current checkout: $SRC"
else
  command -v git >/dev/null 2>&1 || { err "git is required to clone $REPO_URL"; exit 1; }
  SRC="$(mktemp -d)/agent-browser-firefox"
  CLEANUP="$SRC"
  info "Cloning $REPO_URL"
  git clone --depth 1 "$REPO_URL" "$SRC"
fi

# 3. Build -------------------------------------------------------------------
info "Building release binary (first build may take a few minutes)…"
cargo build --release --manifest-path "$SRC/Cargo.toml"

# 4. Install binary ----------------------------------------------------------
mkdir -p "$INSTALL_DIR"
cp "$SRC/target/release/$BIN" "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"
ok "Installed: $INSTALL_DIR/$BIN"
[ -n "$CLEANUP" ] && rm -rf "$(dirname "$CLEANUP")" 2>/dev/null || true

# 5. PATH hint ---------------------------------------------------------------
case ":$PATH:" in
  *":$INSTALL_DIR:"*) : ;;
  *) info "Add this to your shell profile:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

# 6. Ensure Firefox ----------------------------------------------------------
info "Ensuring Firefox is installed…"
"$INSTALL_DIR/$BIN" install || err "Firefox setup needs attention (see message above)."

ok "Done."
printf '\nQuick start:\n  %s open example.com\n  %s snapshot\n' "$BIN" "$BIN"
