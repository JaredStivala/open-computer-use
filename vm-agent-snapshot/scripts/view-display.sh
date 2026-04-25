#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "view-display.sh must run inside the Ubuntu VM. From macOS use: scripts/vm-view.sh" >&2
  exit 1
fi

DISPLAY_ID="${DISPLAY_ID:-:99}"
VNC_PORT="${VNC_PORT:-5900}"
VNC_PASSWORD="${VNC_PASSWORD:-agent}"
LOG_DIR="${LOG_DIR:-/tmp/agent-vnc}"

mkdir -p "$LOG_DIR" /tmp/agent

if ! DISPLAY="$DISPLAY_ID" xdpyinfo >/dev/null 2>&1; then
  DISPLAY_ID="$DISPLAY_ID" scripts/start-xorg-dummy.sh "$DISPLAY_ID"
fi

if pgrep -x x11vnc >/dev/null 2>&1; then
  echo "x11vnc already serving a display on port $VNC_PORT"
  exit 0
fi

: >"$LOG_DIR/x11vnc.log"

nohup x11vnc \
  -display "$DISPLAY_ID" \
  -forever \
  -shared \
  -passwd "$VNC_PASSWORD" \
  -listen 0.0.0.0 \
  -rfbport "$VNC_PORT" \
  -noxdamage \
  -noxfixes \
  -noscr \
  -nowf \
  -repeat \
  >"$LOG_DIR/x11vnc.log" 2>&1 &

for _ in $(seq 1 50); do
  if grep -q "The VNC desktop is" "$LOG_DIR/x11vnc.log" 2>/dev/null; then
    echo "VNC ready on port $VNC_PORT for display $DISPLAY_ID"
    exit 0
  fi
  sleep 0.1
done

echo "x11vnc may not have started; log follows:" >&2
sed -n '1,160p' "$LOG_DIR/x11vnc.log" >&2 || true
exit 1
