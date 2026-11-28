#!/usr/bin/env bash
# Installs RFiles for the current user (Ubuntu / Debian).
#   ./install.sh            build and install
#   ./install.sh uninstall      remove it again
#   ./install.sh set-default    make RFiles your default file manager
#   ./install.sh unset-default  go back to GNOME Files
#   ./install.sh update         get the newest RFiles from GitHub and install it (if there is a newer one)
#   ./install.sh auto-update on|off   run "update" every hour in the background
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
ICON_FILE="$HOME/.local/share/icons/hicolor/scalable/apps/io.github.rfiles.Files.svg"
DBUS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services"
DBUS_FILE="$DBUS_DIR/org.freedesktop.FileManager1.service"
LOCALE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/locale"
# Starts `rfiles --keep-file-manager` at login, which keeps "Show in folder" in RFiles even when Nautilus runs
AUTOSTART_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/autostart/rfiles-file-manager.desktop"
# "update" builds in its own clone, so a work-in-progress checkout is never touched
UPDATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/rfiles"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
# RFiles as the Save / Open dialog: a D-Bus service, the portal backend file (system-wide, needs sudo) and
# the user's portal config choosing it. The portal only reads backend files from /usr/share.
CHOOSER_DBUS_FILE="$DBUS_DIR/io.github.rfiles.FileChooser.service"
PORTAL_FILE=/usr/share/xdg-desktop-portal/portals/rfiles.portal
PORTAL_CONF="${XDG_CONFIG_HOME:-$HOME/.config}/xdg-desktop-portal/gnome-portals.conf"
PORTAL_MARK="# Written by RFiles install.sh"
cd "$(dirname "$0")"

reload_dbus() {
    gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.ReloadConfig >/dev/null 2>&1 || true
}

stop_keeper() {
    pkill -u "$USER" -f -- '/rfiles --keep-file-manager$' 2>/dev/null || true
}

unset_default() {
    rm -f "$DBUS_FILE" "$AUTOSTART_FILE" "$CHOOSER_DBUS_FILE"
    stop_keeper
    reload_dbus
    # Only a config this script wrote
    if grep -qxF "$PORTAL_MARK" "$PORTAL_CONF" 2>/dev/null; then
        rm -f "$PORTAL_CONF"
        systemctl --user try-restart xdg-desktop-portal.service >/dev/null 2>&1 || true
    fi
    if [[ "$(xdg-mime query default inode/directory 2>/dev/null)" == "rfiles.desktop" ]]; then
        xdg-mime default org.gnome.Nautilus.desktop inode/directory
    fi
}

# Whether D-Bus name $1 is owned on the session bus (asking does not start anything).
bus_name_owned() {
    [[ "$(gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.NameHasOwner "$1" 2>/dev/null)" == *true* ]]
}

# A running RFiles keeps running the old binary, and every new launch goes to it.
# Asks it to exit if it has no windows (it is just lingering); otherwise tells the user.
# It is found by its unique bus name, so nothing else can be hit.
switch_running_rfiles() {
    local name=io.github.rfiles.Files
    bus_name_owned "$name" || return 0
    gdbus call --session --dest "$name" --object-path /io/github/rfiles/Files \
        --method org.gtk.Actions.Activate quit-if-idle '[]' '{}' >/dev/null 2>&1 || true
    for _ in {1..30}; do
        bus_name_owned "$name" || return 0
        sleep 0.1
    done
    echo "RFiles is open right now, and its windows still run the old version."
    echo "Close all RFiles windows (or press Ctrl+Q in one) to switch to the new version."
}

auto_update_off() {
    systemctl --user disable --now rfiles-update.timer >/dev/null 2>&1 || true
    rm -f "$UNIT_DIR/rfiles-update.service" "$UNIT_DIR/rfiles-update.timer"
    systemctl --user daemon-reload >/dev/null 2>&1 || true
}

SKIP_DEPS=no
case "${1:-}" in
    --no-deps)
        # Used by "update": the build dependencies are already there
        SKIP_DEPS=yes
        ;;
    update)
        mkdir -p "$UPDATE_DIR"
        src="$UPDATE_DIR/src"
        if [[ ! -d "$src/.git" ]]; then
            git clone --quiet "$(git remote get-url origin)" "$src"
        fi
        git -C "$src" fetch --quiet origin main
        new="$(git -C "$src" rev-parse origin/main)"
        if [[ "$new" == "$(cat "$UPDATE_DIR/installed-rev" 2>/dev/null)" && -x "$BIN_DIR/rfiles" ]]; then
            echo "RFiles is up to date."
            exit 0
        fi
        # Its own clone: nothing local to keep
        git -C "$src" reset --quiet --hard "$new"
        "$src/install.sh" --no-deps
        echo "$new" > "$UPDATE_DIR/installed-rev"
        notify-send -i io.github.rfiles.Files "RFiles was updated" \
            "$(git -C "$src" log -1 --format=%s)" >/dev/null 2>&1 || true
        exit 0
        ;;
    auto-update)
        case "${2:-}" in
            on)
                mkdir -p "$UNIT_DIR" "$UPDATE_DIR"
                # The timer runs the script in RFiles' own clone, so it keeps working whatever happens to this one
                if [[ ! -d "$UPDATE_DIR/src/.git" ]]; then
                    git clone --quiet "$(git remote get-url origin)" "$UPDATE_DIR/src"
                fi
                printf '[Unit]\nDescription=Update RFiles from GitHub\nAfter=network-online.target\n\n[Service]\nType=oneshot\nEnvironment=PATH=%s/.cargo/bin:/usr/local/bin:/usr/bin:/bin\nExecStart=%s update\nNice=10\n' \
                    "$HOME" "$UPDATE_DIR/src/install.sh" > "$UNIT_DIR/rfiles-update.service"
                printf '[Unit]\nDescription=Check for a newer RFiles every hour\n\n[Timer]\nOnStartupSec=2min\nOnUnitActiveSec=1h\n\n[Install]\nWantedBy=timers.target\n' \
                    > "$UNIT_DIR/rfiles-update.timer"
                systemctl --user daemon-reload
                systemctl --user enable --now rfiles-update.timer
                echo "RFiles now checks GitHub for updates every hour and installs them by itself."
                echo "See what it did: journalctl --user -u rfiles-update"
                ;;
            off)
                auto_update_off
                echo "Automatic updates are off."
                ;;
            *)
                echo "Usage: ./install.sh auto-update on|off"
                exit 1
                ;;
        esac
        exit 0
        ;;
    uninstall)
        unset_default
        auto_update_off
        rm -f "$BIN_DIR/rfiles" "$APP_DIR/rfiles.desktop" "$ICON_FILE"
        echo "RFiles removed. (Your settings are kept in ~/.config/rfiles)"
        exit 0
        ;;
    set-default)
        # The script install lives in ~/.local/bin, the .deb in /usr/bin
        if [[ -x "$BIN_DIR/rfiles" ]]; then
            BIN="$BIN_DIR/rfiles"
        elif [[ -x /usr/bin/rfiles ]]; then
            BIN=/usr/bin/rfiles
        else
            echo "Install RFiles first: the .deb from Releases, or ./install.sh"
            exit 1
        fi
        # Folders open in RFiles
        xdg-mime default rfiles.desktop inode/directory
        # "Show in folder" from browsers, editors and the desktop portal
        mkdir -p "$DBUS_DIR"
        sed "s|@BIN@|$BIN|" rfiles-filemanager1.service > "$DBUS_FILE"
        reload_dbus
        # Holds the "Show in folder" name from login on, so Nautilus (started by the desktop icons) can't take it
        mkdir -p "$(dirname "$AUTOSTART_FILE")"
        printf '[Desktop Entry]\nType=Application\nName=RFiles (Show in folder)\nExec=%s --keep-file-manager\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n' "$BIN" > "$AUTOSTART_FILE"
        stop_keeper
        setsid "$BIN" --keep-file-manager </dev/null >/dev/null 2>&1 &
        # Save / Open dialogs (Brave's "Save as", uploads, ...) as RFiles windows instead of GNOME Files
        printf '[D-BUS Service]\nName=io.github.rfiles.FileChooser\nExec=%s --file-chooser\n' "$BIN" > "$CHOOSER_DBUS_FILE"
        reload_dbus
        if [[ ! -f "$PORTAL_FILE" ]]; then
            echo "To make Save / Open dialogs RFiles too, one file goes in /usr/share (asks for your password):"
            printf '[portal]\nDBusName=io.github.rfiles.FileChooser\nInterfaces=org.freedesktop.impl.portal.FileChooser\n' \
                | sudo tee "$PORTAL_FILE" >/dev/null || true
        fi
        if [[ ! -f "$PORTAL_FILE" ]]; then
            echo "Save / Open dialogs stay GNOME Files."
        elif [[ -f "$PORTAL_CONF" ]] && ! grep -qxF "$PORTAL_MARK" "$PORTAL_CONF"; then
            echo "You have your own $PORTAL_CONF, so it was left alone. For RFiles Save / Open dialogs, add to it:"
            echo "  org.freedesktop.impl.portal.FileChooser=rfiles;gnome;"
        else
            mkdir -p "$(dirname "$PORTAL_CONF")"
            # The system's GNOME choices, plus RFiles for file dialogs (GNOME's if RFiles can't answer)
            {
                echo "$PORTAL_MARK"
                cat /usr/share/xdg-desktop-portal/gnome-portals.conf 2>/dev/null || printf '[preferred]\ndefault=gnome;gtk;\n'
                echo 'org.freedesktop.impl.portal.FileChooser=rfiles;gnome;'
            } > "$PORTAL_CONF"
            systemctl --user try-restart xdg-desktop-portal.service >/dev/null 2>&1 || true
        fi
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

if [[ "$SKIP_DEPS" == no ]]; then
    echo "==> Installing build dependencies (needs your password)"
    sudo apt-get update -qq
    sudo apt-get install -y libgtk-4-dev libpoppler-glib-dev build-essential pkg-config curl
fi

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
# Compiled translations: po/<language>.mo (none yet) go to the user's locale folder
for mo in po/*.mo; do
    [[ -f "$mo" ]] || continue
    lang="$(basename "$mo" .mo)"
    mkdir -p "$LOCALE_DIR/$lang/LC_MESSAGES"
    install -m644 "$mo" "$LOCALE_DIR/$lang/LC_MESSAGES/rfiles.mo"
done
switch_running_rfiles

echo
echo "Done! Open \"RFiles\" from your app grid, or run: rfiles"
echo "To make it your default file manager: ./install.sh set-default"
echo "To get updates from GitHub by itself: ./install.sh auto-update on"
