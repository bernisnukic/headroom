#!/usr/bin/env bash
# Install Headroom for the current user.
#
#   ./install.sh              build (if cargo is available) or use ./bin/headroom, then install
#   ./install.sh --autostart  also start it in the tray at login
#
# Installs ~/.local/bin/headroom and ~/.local/share/applications/headroom.desktop.
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

autostart=false
[[ ${1:-} == "--autostart" ]] && autostart=true

bin=""
if command -v cargo >/dev/null; then
  # gpui needs these headers at build time (Ubuntu/Debian names)
  missing=()
  for p in libxkbcommon-dev libwayland-dev libvulkan-dev libfontconfig-dev libxcb1-dev pkg-config; do
    dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p")
  done
  if ((${#missing[@]})); then
    echo "Missing build packages: ${missing[*]}"
    echo "Install them with: sudo apt install ${missing[*]}"
    exit 1
  fi
  if [[ ! -e /usr/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0 ]]; then
    echo "libxkbcommon-x11-0 is missing: sudo apt install libxkbcommon-x11-0"
    exit 1
  fi
  cargo build --release
  bin=target/release/headroom
elif [[ -x bin/headroom ]]; then
  echo "cargo not found; using the prebuilt bin/headroom (x86_64, built on Ubuntu 26.04)"
  bin=bin/headroom
else
  echo "Need either cargo (https://rustup.rs) or a prebuilt bin/headroom."
  exit 1
fi

mkdir -p ~/.local/bin ~/.local/share/applications
# replace atomically so a running copy keeps its old file
install -m 755 "$bin" ~/.local/bin/headroom.new
mv ~/.local/bin/headroom.new ~/.local/bin/headroom

desktop() {
  cat <<EOF
[Desktop Entry]
Type=Application
Name=Headroom
Comment=How much room is left before Linux starts killing apps: memory, CPU and GPU
Exec=$HOME/.local/bin/headroom$1
Icon=utilities-system-monitor
Terminal=false
Categories=System;Monitor;
StartupWMClass=headroom
Keywords=memory;ram;cpu;gpu;oom;swap;
EOF
}
desktop "" > ~/.local/share/applications/headroom.desktop

if $autostart; then
  mkdir -p ~/.config/autostart
  desktop " --tray" > ~/.config/autostart/headroom.desktop
  echo "Autostart: ~/.config/autostart/headroom.desktop"
fi

echo "Installed ~/.local/bin/headroom. Start it with: headroom --tray &"
echo "The top-bar icon needs GNOME's AppIndicator extension (on by default in Ubuntu)."
