#!/usr/bin/env bash
set -euo pipefail

CONFIG="${CONFIG:-config/agent.example.toml}"
DISPLAY_ID="${DISPLAY_ID:-${DISPLAY:-:99}}"
LOG_DIR="${LOG_DIR:-/tmp/agent-model-smoke}"
GOAL="${GOAL:-This is a smoke completion test. The goal-state assertion is already satisfied. Return a Finish action with success true now. Do not emit Noop.}"

mkdir -p "$LOG_DIR" /tmp/agent

if [ "$(uname -s)" = "Linux" ]; then
  if ! DISPLAY="$DISPLAY_ID" xdpyinfo >/dev/null 2>&1; then
    DISPLAY_ID="$DISPLAY_ID" scripts/start-xorg-dummy.sh "$DISPLAY_ID"
  fi
  export DISPLAY="$DISPLAY_ID"
  DISPLAY="$DISPLAY_ID" xset m 1/1 0 >/dev/null 2>&1 || true
  if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    eval "$(dbus-launch --sh-syntax)"
    export DBUS_SESSION_BUS_ADDRESS
  fi
fi

source "$HOME/.cargo/env" 2>/dev/null || true
cargo build --workspace

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
start_daemon inputd
start_daemon verifyd
start_daemon reasonerd
sleep 1

target/debug/supervisord \
  --config "$CONFIG" \
  --goal "$GOAL" \
  --display-id "$DISPLAY_ID"
