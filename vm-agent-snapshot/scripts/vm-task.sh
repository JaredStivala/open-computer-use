#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 1 ]; then
  echo "Usage: scripts/vm-task.sh \"task goal\"" >&2
  exit 2
fi

VM_NAME="${VM_NAME:-computer-use-agent}"
DISPLAY_ID="${DISPLAY_ID:-:99}"
REMOTE_DIR="/home/ubuntu/computer-use-agent"

if ! command -v multipass >/dev/null 2>&1; then
  echo "multipass is required" >&2
  exit 1
fi

if ! multipass info "$VM_NAME" >/dev/null 2>&1; then
  echo "VM $VM_NAME does not exist yet. Run ./run.sh first." >&2
  exit 1
fi

GOAL="$*"
multipass exec "$VM_NAME" -- bash -lc "source ~/.cargo/env && cd '$REMOTE_DIR' && DISPLAY_ID='$DISPLAY_ID' scripts/run-task.sh $(printf '%q' "$GOAL")"
