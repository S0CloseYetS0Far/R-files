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
mkdir -p "$BIN_DIR" "$APP_DIR" "$(dirname "$ICON_FILE")"
install -m755 target/release/rfiles "$BIN_DIR/rfiles"
install -m644 data/io.github.rfiles.Files.svg "$ICON_FILE"
sed "s|^Exec=.*|Exec=$BIN_DIR/rfiles %U|" rfiles.desktop > "$APP_DIR/rfiles.desktop"
update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true

echo
echo "Done! Open \"RFiles\" from your app grid, or run: rfiles"
echo "To make it your default file manager: ./install.sh set-default"
