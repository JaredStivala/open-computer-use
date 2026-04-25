#!/usr/bin/env bash
set -euo pipefail

CONFIG="${CONFIG:-config/agent.example.toml}"
GOAL="${GOAL:-smoke test}"
LOG_DIR="${LOG_DIR:-/tmp/agent-smoke}"

mkdir -p "$LOG_DIR" /tmp/agent
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
start_daemon captured
start_daemon a11yd
start_daemon inputd
start_daemon verifyd
sleep 1

target/debug/supervisord \
  --config "$CONFIG" \
  --goal "$GOAL" \
  --scripted-task
