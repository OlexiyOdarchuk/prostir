#!/usr/bin/env bash
# Installs the Prostir host for the current user (no root):
#   ~/.local/bin/prostir, its icon and menu entry, and optionally autostart.
#
#   ./scripts/install.sh              build and install
#   ./scripts/install.sh --autostart  also start with the Plasma session
#   ./scripts/install.sh --uninstall  remove everything installed here
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# Release archive: install.sh sits next to the binary. Source tree: in scripts/.
if [[ -x "$HERE/prostir" ]]; then ROOT="$HERE"; else ROOT="$(cd "$HERE/.." && pwd)"; fi
BIN="$HOME/.local/bin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
ICON="$DATA/icons/hicolor/scalable/apps/prostir.svg"
DESKTOP="$DATA/applications/prostir.desktop"
AUTOSTART="$CONFIG/autostart/prostir.desktop"

if [[ "${1:-}" == "--uninstall" ]]; then
    rm -f "$BIN/prostir" "$ICON" "$DESKTOP" "$AUTOSTART"
    echo "Prostir removed (settings in $CONFIG/prostir are kept)."
    exit 0
fi

if [[ -x "$ROOT/prostir" ]]; then
    built="$ROOT/prostir"
else
    echo ">> building"
    (cd "$ROOT" && cargo build --release -p prostir-host --bin prostir)
    built="$ROOT/target/release/prostir"
fi
assets="$ROOT/packaging"

install -Dm755 "$built" "$BIN/prostir"
install -Dm644 "$assets/prostir.svg" "$ICON"
install -Dm644 "$assets/prostir.desktop" "$DESKTOP"
sed -i "s|^Exec=.*|Exec=$BIN/prostir|" "$DESKTOP"
command -v update-desktop-database >/dev/null && update-desktop-database "$DATA/applications" || true

if [[ "${1:-}" == "--autostart" ]]; then
    install -Dm644 "$DESKTOP" "$AUTOSTART"
    echo "X-KDE-autostart-phase=2" >> "$AUTOSTART"
fi

echo ">> installed $BIN/prostir"
echo "   Start it from the application menu (Prostir) or run: prostir"
[[ -f "$AUTOSTART" ]] && echo "   It starts with your Plasma session." || echo "   Autostart: tray menu → «Запускати разом із системою», or rerun with --autostart."
