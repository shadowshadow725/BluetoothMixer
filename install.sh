#!/usr/bin/env bash
# Bluetooth Mixer installer for Debian/Raspberry Pi OS (PipeWire + WirePlumber).
set -euo pipefail

PORT="${PORT:-8420}"
BIN=/usr/local/bin/bluetooth-mixer
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

echo "==> Installing audio / bluetooth packages"
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends \
    pipewire wireplumber wireplumber-pulse pipewire-audio-client-libraries \
    bluez bluez-tools libspa-0.2-modules \
    pulseaudio-utils swh-plugins

echo "==> Building bluetooth-mixer"
cargo build --release || {
    echo "cargo not found - install Rust with:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
}
sudo install -m 0755 target/release/bluetooth-mixer "$BIN"

echo "==> Enabling bluetooth service"
sudo systemctl enable --now bluetooth

echo "==> Installing systemd user unit (port $PORT)"
mkdir -p "$UNIT_DIR"
sed "s/--port 8420/--port $PORT/" "$(dirname "$0")/bluetooth-mixer.service" > "$UNIT_DIR/bluetooth-mixer.service"
systemctl --user daemon-reload
systemctl --user enable --now bluetooth-mixer

echo "==> Allowing the port through the firewall (if ufw is active)"
if command -v ufw >/dev/null && sudo ufw status | grep -q "^Status: active"; then
    sudo ufw allow "$PORT/tcp"
fi

cat <<EOF

Done. Open  http://$(hostname -I 2>/dev/null | awk '{print $1}'):$PORT

Logs:      journalctl --user -u bluetooth-mixer -f
Restart:   systemctl --user restart bluetooth-mixer
Config:    ${XDG_CONFIG_HOME:-$HOME/.config}/bluetooth-mixer/state.json

EOF