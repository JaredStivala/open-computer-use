#!/usr/bin/env bash
set -euo pipefail

VM_NAME="${VM_NAME:-computer-use-agent}"
DISPLAY_ID="${DISPLAY_ID:-:99}"
VNC_PORT="${VNC_PORT:-5900}"

if ! command -v multipass >/dev/null 2>&1; then
  echo "multipass is required" >&2
  exit 1
fi

if ! multipass info "$VM_NAME" >/dev/null 2>&1; then
  echo "VM $VM_NAME does not exist yet. Run ./run.sh first." >&2
  exit 1
fi

IP="$(multipass info "$VM_NAME" | awk '/IPv4:/ {print $2; exit}')"
multipass exec "$VM_NAME" -- bash -lc "cd /home/ubuntu/computer-use-agent && DISPLAY_ID='$DISPLAY_ID' VNC_PORT='$VNC_PORT' scripts/view-display.sh"

echo "Opening VNC viewer for $IP:$VNC_PORT"
if [ "$(uname -s)" = "Darwin" ]; then
  open "vnc://$IP:$VNC_PORT"
else
  echo "Open vnc://$IP:$VNC_PORT in a VNC client."
fi
