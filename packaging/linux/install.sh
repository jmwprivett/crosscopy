#!/usr/bin/env bash
# Installs CrossCopy for the current user (no root needed):
#   ~/.local/bin/crosscopy-tray, ~/.local/bin/crosscopy
#   a launcher entry and icon under ~/.local/share
set -euo pipefail

cd "$(dirname "$0")"
BIN="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/256x256/apps"
mkdir -p "$BIN" "$APPS" "$ICONS"

install -m 755 crosscopy-tray crosscopy "$BIN/"
install -m 644 crosscopy.png "$ICONS/crosscopy.png"
sed "s|@BIN@|$BIN|g" crosscopy.desktop > "$APPS/crosscopy.desktop"

echo "Installed to $BIN"

missing=()
command -v zenity >/dev/null || missing+=("zenity (pairing/update dialogs)")
ldconfig -p 2>/dev/null | grep -q libayatana-appindicator3 || missing+=("libayatana-appindicator (tray icon)")
ldconfig -p 2>/dev/null | grep -q libxdo || missing+=("xdotool/libxdo (tray menu)")
if ((${#missing[@]})); then
    echo
    echo "Also install:"
    printf '  - %s\n' "${missing[@]}"
    echo "On Arch: sudo pacman -S zenity libayatana-appindicator xdotool"
fi

cat <<EOF

To start CrossCopy with Hyprland, add this to ~/.config/hypr/hyprland.conf:
  exec-once = $BIN/crosscopy-tray

The icon appears in Waybar's "tray" module. Start it now with:
  $BIN/crosscopy-tray &
EOF
