#!/usr/bin/env bash
set -euo pipefail

sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
  build-essential \
  ca-certificates \
  curl \
  at-spi2-core \
  dbus-x11 \
  gsettings-desktop-schemas \
  imagemagick \
  libatspi2.0-dev \
  libgtk-3-bin \
  libgtk-4-bin \
  libturbojpeg0-dev \
  libx11-dev \
  libxcomposite-dev \
  libxdamage-dev \
  libxext-dev \
  openbox \
  pkg-config \
  software-properties-common \
  xdotool \
  xdg-utils \
  x11-apps \
  x11-utils \
  x11vnc \
  xauth \
  xinit \
  xserver-xorg-core \
  xserver-xorg-input-libinput \
  xserver-xorg-video-dummy \
  x11-xserver-utils \
  xserver-xephyr

if ! command -v firefox >/dev/null 2>&1 || [ -L "$(command -v firefox)" ]; then
  if ! grep -rq 'mozillateam' /etc/apt/sources.list.d 2>/dev/null; then
    sudo add-apt-repository -y ppa:mozillateam/ppa
    sudo tee /etc/apt/preferences.d/mozilla-firefox >/dev/null <<'PIN'
Package: *
Pin: release o=LP-PPA-mozillateam
Pin-Priority: 1001
PIN
    sudo apt-get update
  fi
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y firefox
fi

if ! lsmod | grep -q '^uinput'; then
  sudo modprobe uinput
fi

if ! getent group uinput >/dev/null; then
  sudo groupadd --system uinput
fi

sudo usermod -aG uinput "$USER" || true
sudo usermod -aG input "$USER" || true

sudo tee /etc/udev/rules.d/99-agent-uinput.rules >/dev/null <<'RULE'
KERNEL=="uinput", GROUP="uinput", MODE="0660", OPTIONS+="static_node=uinput"
RULE
sudo udevadm control --reload-rules
sudo udevadm trigger /dev/uinput || true
if [ -e /dev/uinput ]; then
  sudo chgrp uinput /dev/uinput || true
  sudo chmod 0660 /dev/uinput || true
fi

mkdir -p /tmp/agent

if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi

cat <<'MSG'
Ubuntu dependencies installed.
Log out and back in if /dev/uinput group membership was just added.
Run `make build` next, then `make smoke-local` for the no-model loop.
MSG
