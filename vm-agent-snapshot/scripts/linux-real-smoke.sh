#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "linux-real-smoke requires Linux/X11; use scripts/multipass-setup.sh from macOS." >&2
  exit 1
fi

CONFIG="${CONFIG:-config/agent.example.toml}"
DISPLAY_ID="${DISPLAY_ID:-:98}"
WIDTH="${WIDTH:-1024}"
HEIGHT="${HEIGHT:-768}"
LOG_DIR="${LOG_DIR:-/tmp/agent-real-smoke}"
PROBE_BIN="/tmp/agent/x11_pixel_probe"

mkdir -p "$LOG_DIR" /tmp/agent

if ! DISPLAY="$DISPLAY_ID" xdpyinfo >/dev/null 2>&1; then
  DISPLAY_ID="$DISPLAY_ID" WIDTH="$WIDTH" HEIGHT="$HEIGHT" scripts/start-xorg-dummy.sh "$DISPLAY_ID"
fi

export DISPLAY="$DISPLAY_ID"
DISPLAY="$DISPLAY_ID" xset m 1/1 0 >/dev/null 2>&1 || true
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
  eval "$(dbus-launch --sh-syntax)"
  export DBUS_SESSION_BUS_ADDRESS
fi

source "$HOME/.cargo/env" 2>/dev/null || true
cargo build --workspace
cc tools/x11_pixel_probe.c -lX11 -o "$PROBE_BIN"

pids=()
cleanup() {
  for pid in "${pids[@]:-}"; do
    kill "$pid" >/dev/null 2>&1 || true
  done
  for pid in "${pids[@]:-}"; do
    wait "$pid" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

"$PROBE_BIN" >"$LOG_DIR/pixel-probe.log" 2>&1 &
pids+=("$!")
sleep 1

start_daemon() {
  local name="$1"
  shift
  "target/debug/$name" --config "$CONFIG" "$@" >"$LOG_DIR/$name.log" 2>&1 &
  pids+=("$!")
}

start_daemon busd
sleep 0.2
start_daemon captured --display-id "$DISPLAY_ID"
start_daemon a11yd --display-id "$DISPLAY_ID"
start_daemon inputd --screen-width "$WIDTH" --screen-height "$HEIGHT"
start_daemon verifyd
sleep 1

target/debug/smokectl \
  --config "$CONFIG" \
  --display-id "$DISPLAY_ID" \
  --x 100 \
  --y 100
