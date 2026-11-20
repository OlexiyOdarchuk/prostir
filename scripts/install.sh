#!/usr/bin/env bash
# Installs the Prostir PC program for the current user, without root:
#   ~/.local/bin/prostir, its icon and its application menu entry.
#
#   ./install.sh                       install (or update: run it again)
#   ./install.sh --autostart           also start Prostir when you log in
#   ./install.sh --uninstall           remove everything this script installed
#   ./install.sh --uninstall --purge   also remove settings and pairings (~/.config/prostir)
#
# In a source checkout it builds the program first (cargo build --release).
set -euo pipefail

usage() { sed -n '2,10s/^# \{0,1\}//p' "$0"; }

autostart=0 uninstall=0 purge=0
for arg in "$@"; do
    case "$arg" in
        --autostart) autostart=1 ;;
        --uninstall) uninstall=1 ;;
        --purge) purge=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done
if (( purge && !uninstall )); then
    echo "--purge only works together with --uninstall." >&2
    exit 2
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
# Release archive: install.sh sits next to the binary. Source tree: in scripts/.
if [[ -x "$HERE/prostir" ]]; then ROOT="$HERE"; else ROOT="$(cd "$HERE/.." && pwd)"; fi
BIN="$HOME/.local/bin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
ICON="$DATA/icons/hicolor/scalable/apps/prostir.svg"
DESKTOP="$DATA/applications/prostir.desktop"
AUTOSTART="$CONFIG/autostart/prostir.desktop"
DOCS="$DATA/doc/prostir"
SETTINGS="$CONFIG/prostir"

refresh_menu() {
    if command -v update-desktop-database >/dev/null; then
        update-desktop-database "$DATA/applications" 2>/dev/null || true
    fi
}

hyprland_lines() {
    echo "   On Hyprland, start it with the session by adding one line to your config:"
    echo "     hyprland.lua:   hl.on(\"hyprland.start\", function () hl.exec_cmd(\"$BIN/prostir\") end)"
    echo "     hyprland.conf:  exec-once = $BIN/prostir"
    echo "   With ecosystem:enforce_permissions on, also allow screen capture:"
    echo "     hyprland.lua:   hl.permission(\"$BIN/prostir\", \"screencopy\", \"allow\")"
    echo "     hyprland.conf:  permission = $BIN/prostir, screencopy, allow"
}

if (( uninstall )); then
    if pgrep -x -u "$(id -u)" prostir >/dev/null 2>&1; then
        echo "Note: Prostir is still running. Quit it from the tray; removing it does not stop it."
    fi
    rm -f "$BIN/prostir" "$ICON" "$DESKTOP" "$AUTOSTART"
    rm -rf "$DOCS"
    refresh_menu
    echo ">> Prostir removed from $BIN, the application menu and autostart."
    if (( purge )); then
        rm -rf "$SETTINGS"
        echo "   Settings and paired headsets removed ($SETTINGS)."
    elif [[ -d "$SETTINGS" ]]; then
        echo "   Settings and paired headsets are kept in $SETTINGS (--uninstall --purge removes them)."
    fi
    if [[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]]; then
        echo "   If you added Prostir to your Hyprland config, remove those lines too."
    fi
    echo "   The headset app stays on the headset. To remove it: adb uninstall dev.prostir.client"
    exit 0
fi

if [[ -x "$ROOT/prostir" ]]; then
    built="$ROOT/prostir"
else
    echo ">> building"
    (cd "$ROOT" && cargo build --release --locked -p prostir-host --bin prostir)
    built="$ROOT/target/release/prostir"
fi
assets="$ROOT/packaging"

install -Dm755 "$built" "$BIN/prostir"
install -Dm644 "$assets/prostir.svg" "$ICON"
install -Dm644 "$assets/prostir.desktop" "$DESKTOP"
sed -i "s|^Exec=.*|Exec=$BIN/prostir open|" "$DESKTOP"
for f in LICENSE LICENSE-EXCEPTION.md THIRD-PARTY-LICENSES.txt; do
    if [[ -f "$ROOT/$f" ]]; then install -Dm644 "$ROOT/$f" "$DOCS/$f"; fi
done
refresh_menu

echo ">> installed $BIN/prostir"
echo "   Start it from the application menu (Prostir) or run: $BIN/prostir"
case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "   Note: $BIN is not on your PATH, so the bare command \"prostir\" will not be found."
       echo "   Use the full path above, or add $BIN to PATH in your shell profile." ;;
esac

if [[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]]; then
    # Hyprland ignores XDG autostart files unless it runs under uwsm, so print the
    # config lines instead of writing one.
    hyprland_lines
elif (( autostart )); then
    install -Dm644 "$DESKTOP" "$AUTOSTART"
    echo "X-KDE-autostart-phase=2" >> "$AUTOSTART"
    echo "   It starts when you log in. Turn that off in the tray: \"Start with the system\"."
elif [[ -f "$AUTOSTART" ]]; then
    echo "   It starts when you log in (tray: \"Start with the system\")."
else
    echo "   To start it when you log in: tray menu, \"Start with the system\", or run this script with --autostart."
fi
