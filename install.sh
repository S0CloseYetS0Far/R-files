#!/usr/bin/env bash
# Installs RFiles for the current user (Ubuntu / Debian).
#   ./install.sh            build and install
#   ./install.sh uninstall      remove it again
#   ./install.sh set-default    make RFiles your default file manager
#   ./install.sh unset-default  go back to GNOME Files
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
ICON_FILE="$HOME/.local/share/icons/hicolor/scalable/apps/io.github.rfiles.Files.svg"
DBUS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services"
DBUS_FILE="$DBUS_DIR/org.freedesktop.FileManager1.service"
cd "$(dirname "$0")"

reload_dbus() {
    gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.ReloadConfig >/dev/null 2>&1 || true
}

unset_default() {
    rm -f "$DBUS_FILE"
    reload_dbus
    if [[ "$(xdg-mime query default inode/directory 2>/dev/null)" == "rfiles.desktop" ]]; then
        xdg-mime default org.gnome.Nautilus.desktop inode/directory
    fi
}

case "${1:-}" in
    uninstall)
        unset_default
        rm -f "$BIN_DIR/rfiles" "$APP_DIR/rfiles.desktop" "$ICON_FILE"
        echo "RFiles removed. (Your settings are kept in ~/.config/rfiles)"
        exit 0
        ;;
    set-default)
        [[ -x "$BIN_DIR/rfiles" ]] || { echo "Install RFiles first: ./install.sh"; exit 1; }
        # Folders open in RFiles
        xdg-mime default rfiles.desktop inode/directory
        # "Show in folder" from browsers, editors and the desktop portal
        mkdir -p "$DBUS_DIR"
        sed "s|@BIN@|$BIN_DIR/rfiles|" rfiles-filemanager1.service > "$DBUS_FILE"
        reload_dbus
        echo "RFiles is now your default file manager."
        echo "If GNOME Files is open right now, close it (or log out and back in) so \"Show in folder\" switches over."
        exit 0
        ;;
    unset-default)
        unset_default
        echo "GNOME Files is the default file manager again."
        exit 0
        ;;
esac

echo "==> Installing build dependencies (needs your password)"
sudo apt-get update -qq
sudo apt-get install -y libgtk-4-dev libpoppler-glib-dev build-essential pkg-config curl

# Puts rustup's cargo (in ~/.cargo/bin) first on PATH, installing rustup if needed.
use_rustup_cargo() {
    if [[ ! -x "$HOME/.cargo/bin/cargo" ]]; then
        echo "==> Installing Rust (rustup)"
        # Skips the installer's warning about a distribution Rust in /usr/bin.
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | RUSTUP_INIT_SKIP_PATH_CHECK=yes sh -s -- -y
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
}

# version_at_least HAVE NEED: succeeds when version HAVE >= NEED.
# Compares major.minor.patch numerically; missing parts count as 0 and
# suffixes such as "-nightly" are ignored.
version_at_least() {
    local -a have need
    IFS=. read -r -a have <<< "${1%%[!0-9.]*}"
    IFS=. read -r -a need <<< "${2%%[!0-9.]*}"
    local i a b
    for i in 0 1 2; do
        a=$((10#${have[i]:-0})) b=$((10#${need[i]:-0}))
        if ((a > b)); then return 0; fi
        if ((a < b)); then return 1; fi
    done
    return 0
}

# The installed Rust version, e.g. "1.75.0" (empty if it can't be read).
rust_version() {
    local out
    out="$(rustc --version 2>/dev/null || cargo --version 2>/dev/null || true)"
    if [[ "$out" =~ ([0-9]+\.[0-9]+\.[0-9]+) ]]; then
        echo "${BASH_REMATCH[1]}"
    fi
}

if ! command -v cargo >/dev/null 2>&1; then
    use_rustup_cargo
fi

# The minimum Rust version lives in Cargo.toml (rust-version = "...").
MIN_RUST="$(sed -n 's/^rust-version *= *"\([0-9.]*\)".*/\1/p' Cargo.toml)"
HAVE_RUST="$(rust_version)"
if [[ -n "$MIN_RUST" && -n "$HAVE_RUST" ]] && ! version_at_least "$HAVE_RUST" "$MIN_RUST"; then
    if ! command -v rustup >/dev/null 2>&1; then
        echo "==> RFiles needs Rust $MIN_RUST or newer, but the cargo found is Rust $HAVE_RUST (likely the distribution package)."
        echo "    Using a current Rust from rustup in ~/.cargo instead; the system package is left alone."
        use_rustup_cargo
        HAVE_RUST="$(rust_version)"
    fi
    if [[ -n "$HAVE_RUST" ]] && ! version_at_least "$HAVE_RUST" "$MIN_RUST"; then
        echo "RFiles needs Rust $MIN_RUST or newer, but this computer has Rust $HAVE_RUST."
        echo "Update it with \"rustup update stable\", then run ./install.sh again."
        exit 1
    fi
fi

echo "==> Building RFiles (first build takes a few minutes)"
cargo build --release

echo "==> Installing"
mkdir -p "$BIN_DIR" "$APP_DIR" "$(dirname "$ICON_FILE")"
install -m755 target/release/rfiles "$BIN_DIR/rfiles"
install -m644 data/io.github.rfiles.Files.svg "$ICON_FILE"
# Two Exec lines: the main entry (with %U) and the "New Window" action (without)
sed -e "s|^Exec=rfiles %U\$|Exec=$BIN_DIR/rfiles %U|" -e "s|^Exec=rfiles\$|Exec=$BIN_DIR/rfiles|" rfiles.desktop > "$APP_DIR/rfiles.desktop"
update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true

echo
echo "Done! Open \"RFiles\" from your app grid, or run: rfiles"
echo "To make it your default file manager: ./install.sh set-default"
