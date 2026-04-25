#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "run-task.sh must run inside the Ubuntu VM. From macOS use: scripts/vm-task.sh \"your task\"" >&2
  exit 1
fi

if [ "$#" -lt 1 ]; then
  echo "Usage: scripts/run-task.sh \"task goal\"" >&2
  exit 2
fi

GOAL="$*"
CONFIG="${CONFIG:-config/agent.example.toml}"
DISPLAY_ID="${DISPLAY_ID:-:99}"
WIDTH="${WIDTH:-1024}"
HEIGHT="${HEIGHT:-768}"
LOG_DIR="${LOG_DIR:-/tmp/agent-task}"
START_BROWSER="${START_BROWSER:-1}"
BROWSER_URL="${BROWSER_URL:-about:blank}"
BROWSER_PROFILE="${BROWSER_PROFILE:-/tmp/agent/chromium-profile}"
RESET_BROWSER="${RESET_BROWSER:-1}"
CLEAN_BROWSER_PROFILE="${CLEAN_BROWSER_PROFILE:-1}"
FORCE_BUILD="${FORCE_BUILD:-0}"

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

ensure_built() {
  if [ "$FORCE_BUILD" = "1" ]; then
    cargo build --workspace
    return
  fi
  local bins=(busd captured a11yd inputd verifyd reasonerd supervisord)
  local missing=0
  local bin
  for bin in "${bins[@]}"; do
    if [ ! -x "target/debug/$bin" ]; then
      missing=1
      break
    fi
  done
  if [ "$missing" = "1" ]; then
    cargo build --workspace
  fi
}

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

ensure_openbox() {
  if ! DISPLAY="$DISPLAY_ID" pgrep -af "openbox" >/dev/null 2>&1; then
    DISPLAY="$DISPLAY_ID" openbox >"$LOG_DIR/openbox.log" 2>&1 &
    pids+=("$!")
    sleep 0.3
  fi
}

ensure_atspi() {
  local bus_addr="${DBUS_SESSION_BUS_ADDRESS:-unix:path=/run/user/$(id -u)/bus}"
  export DBUS_SESSION_BUS_ADDRESS="$bus_addr"
  pkill -f '/usr/libexec/at-spi2-registryd' >/dev/null 2>&1 || true
  sleep 0.3
  if ! pgrep -f '/usr/libexec/at-spi-bus-launcher' >/dev/null 2>&1; then
    DISPLAY="$DISPLAY_ID" /usr/libexec/at-spi-bus-launcher \
      >"$LOG_DIR/atspi-launcher.log" 2>&1 &
    sleep 0.5
  fi
  DISPLAY="$DISPLAY_ID" /usr/libexec/at-spi2-registryd \
    >"$LOG_DIR/atspi-registryd.log" 2>&1 &
  sleep 0.5
}

ensure_browser() {
  if [ "$START_BROWSER" != "1" ]; then
    return
  fi
  if ! command -v firefox >/dev/null 2>&1; then
    echo "firefox is not installed. Run scripts/ubuntu-setup.sh or ./run.sh first." >&2
    exit 1
  fi
  if [ "$RESET_BROWSER" = "1" ]; then
    pkill -f '/usr/lib/firefox/firefox' >/dev/null 2>&1 || true
    sleep 1
  elif DISPLAY="$DISPLAY_ID" pgrep -af "firefox.*$BROWSER_PROFILE" >/dev/null 2>&1; then
    return
  fi
  if [ "$CLEAN_BROWSER_PROFILE" = "1" ]; then
    if [ "$BROWSER_PROFILE" = "/tmp/agent/chromium-profile" ]; then
      BROWSER_PROFILE="/tmp/agent/firefox-profile-$(date +%s%N)"
    fi
    rm -rf "$BROWSER_PROFILE"
  fi
  mkdir -p "$BROWSER_PROFILE"
  cat >"$BROWSER_PROFILE/user.js" <<PREFS
user_pref("accessibility.force_disabled", -1);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("datareporting.policy.dataSubmissionPolicyAcceptedVersion", 2);
user_pref("app.update.auto", false);
user_pref("app.update.enabled", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.startup.firstrunSkipsHomepage", true);
user_pref("browser.tabs.warnOnClose", false);
user_pref("browser.sessionstore.resume_from_crash", false);
// Skip the new-tab/welcome page so the agent never has to click through
// Pocket recommendations or news widgets to get to the target URL.
user_pref("browser.startup.page", 1);
user_pref("browser.startup.homepage", "${BROWSER_URL}");
user_pref("browser.newtabpage.enabled", false);
user_pref("browser.newtabpage.activity-stream.feeds.section.topstories", false);
user_pref("browser.newtabpage.activity-stream.feeds.topsites", false);
user_pref("browser.newtabpage.activity-stream.showSponsoredTopSites", false);
user_pref("browser.newtabpage.activity-stream.showSponsored", false);
user_pref("browser.newtabpage.activity-stream.feeds.snippets", false);
user_pref("browser.newtabpage.activity-stream.feeds.discoverystreamfeed", false);
user_pref("browser.preferences.moreFromMozilla", false);
user_pref("browser.warnOnQuit", false);
user_pref("network.cookie.cookieBehavior", 0);
PREFS
  DISPLAY="$DISPLAY_ID" \
    GTK_MODULES=gail:atk-bridge \
    GNOME_ACCESSIBILITY=1 \
    MOZ_FORCE_DISABLE_E10S=0 \
    firefox \
      -profile "$BROWSER_PROFILE" \
      -no-remote \
      -new-instance \
      -width "$WIDTH" \
      -height "$HEIGHT" \
      "$BROWSER_URL" >"$LOG_DIR/firefox.log" 2>&1 &
  sleep 6
}

start_daemon() {
  local name="$1"
  shift
  "target/debug/$name" --config "$CONFIG" "$@" >"$LOG_DIR/$name.log" 2>&1 &
  pids+=("$!")
}

ensure_openbox
ensure_atspi
ensure_browser
ensure_built
export AGENT_SCREEN_WIDTH="$WIDTH"
export AGENT_SCREEN_HEIGHT="$HEIGHT"
start_daemon busd
sleep 0.2
start_daemon captured --display-id "$DISPLAY_ID"
start_daemon inputd --screen-width "$WIDTH" --screen-height "$HEIGHT"
start_daemon verifyd
start_daemon reasonerd
sleep "${BROWSER_SETTLE_SEC:-3}"
start_daemon a11yd --display-id "$DISPLAY_ID"
sleep 2

target/debug/supervisord \
  --config "$CONFIG" \
  --goal "$GOAL" \
  --display-id "$DISPLAY_ID"
