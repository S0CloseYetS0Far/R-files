#!/usr/bin/env bash
# Installs RFiles for the current user (Ubuntu / Debian).
#   ./install.sh            build and install
#   ./install.sh uninstall  remove it again
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
cd "$(dirname "$0")"

if [[ "${1:-}" == "uninstall" ]]; then
    rm -f "$BIN_DIR/rfiles" "$APP_DIR/rfiles.desktop"
    echo "RFiles removed. (Your settings are kept in ~/.config/rfiles)"
    exit 0
fi

echo "==> Installing build dependencies (needs your password)"
sudo apt-get update -qq
sudo apt-get install -y libgtk-4-dev libpoppler-glib-dev build-essential pkg-config curl

if ! command -v cargo >/dev/null 2>&1; then
    if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    else
        echo "==> Installing Rust (rustup)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
fi

echo "==> Building RFiles (first build takes a few minutes)"
cargo build --release

echo "==> Installing"
mkdir -p "$BIN_DIR" "$APP_DIR"
install -m755 target/release/rfiles "$BIN_DIR/rfiles"
sed "s|^Exec=.*|Exec=$BIN_DIR/rfiles %U|" rfiles.desktop > "$APP_DIR/rfiles.desktop"
update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true

echo
echo "Done! Open \"RFiles\" from your app grid, or run: rfiles"
