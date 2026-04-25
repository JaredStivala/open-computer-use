#!/usr/bin/env bash
set -euo pipefail

DISPLAY_ID="${1:-${DISPLAY_ID:-:99}}"
WIDTH="${WIDTH:-1024}"
HEIGHT="${HEIGHT:-768}"
DEPTH="${DEPTH:-24}"
DISPLAY_NUM="${DISPLAY_ID#:}"
SOCKET="/tmp/.X11-unix/X${DISPLAY_NUM}"
LOG="/tmp/agent/Xorg-${DISPLAY_NUM}.log"
PID_FILE="/tmp/agent/Xorg-${DISPLAY_NUM}.pid"
CONF="/tmp/agent/xorg-dummy-${DISPLAY_NUM}.conf"

mkdir -p /tmp/agent

if [ -S "$SOCKET" ] && DISPLAY="$DISPLAY_ID" xdpyinfo >/dev/null 2>&1; then
  echo "Xorg dummy display already running on $DISPLAY_ID"
  exit 0
fi

cat >"$CONF" <<CONF_EOF
Section "ServerFlags"
    Option "AutoAddDevices" "true"
    Option "DontVTSwitch" "true"
    Option "AllowMouseOpenFail" "true"
EndSection

Section "Device"
    Identifier "DummyDevice"
    Driver "dummy"
    VideoRam 256000
EndSection

Section "Monitor"
    Identifier "DummyMonitor"
    HorizSync 30.0-80.0
    VertRefresh 50.0-75.0
    Modeline "${WIDTH}x${HEIGHT}" 65.00 ${WIDTH} 1048 1184 1344 ${HEIGHT} 771 777 806
EndSection

Section "Screen"
    Identifier "DummyScreen"
    Device "DummyDevice"
    Monitor "DummyMonitor"
    DefaultDepth ${DEPTH}
    SubSection "Display"
        Depth ${DEPTH}
        Modes "${WIDTH}x${HEIGHT}"
        Virtual ${WIDTH} ${HEIGHT}
    EndSubSection
EndSection
CONF_EOF

sudo Xorg "$DISPLAY_ID" \
  -noreset \
  -ac \
  +extension MIT-SHM \
  +extension DAMAGE \
  +extension Composite \
  -config "$CONF" \
  -logfile "$LOG" \
  >/tmp/agent/Xorg-${DISPLAY_NUM}.stdout 2>/tmp/agent/Xorg-${DISPLAY_NUM}.stderr &

echo "$!" >"$PID_FILE"

for _ in $(seq 1 100); do
  if DISPLAY="$DISPLAY_ID" xdpyinfo >/dev/null 2>&1; then
    echo "Xorg dummy display ready on $DISPLAY_ID"
    exit 0
  fi
  sleep 0.1
done

echo "Xorg dummy display failed to start; log follows:" >&2
sed -n '1,200p' "$LOG" >&2 || true
exit 1
