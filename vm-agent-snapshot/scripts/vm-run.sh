#!/usr/bin/env bash
set -euo pipefail

VM_NAME="${VM_NAME:-computer-use-agent}"
IMAGE="${IMAGE:-22.04}"
CPUS="${CPUS:-4}"
MEMORY="${MEMORY:-6G}"
DISK="${DISK:-30G}"
REMOTE_DIR="/home/ubuntu/computer-use-agent"
ARCHIVE="/tmp/computer-use-agent.tar.gz"
SMOKE_DISPLAY_ID="${SMOKE_DISPLAY_ID:-:98}"

if ! command -v multipass >/dev/null 2>&1; then
  echo "multipass is required. Install it, then rerun ./run.sh." >&2
  exit 1
fi

if ! multipass info "$VM_NAME" >/dev/null 2>&1; then
  echo "Creating Ubuntu 22.04 VM: $VM_NAME"
  multipass launch "$IMAGE" --name "$VM_NAME" --cpus "$CPUS" --memory "$MEMORY" --disk "$DISK"
else
  multipass start "$VM_NAME" >/dev/null 2>&1 || true
fi

echo "Syncing repo into $VM_NAME:$REMOTE_DIR"
COPYFILE_DISABLE=1 tar \
  --no-xattrs \
  --exclude .git \
  --exclude target \
  --exclude "$ARCHIVE" \
  -czf "$ARCHIVE" \
  .

multipass exec "$VM_NAME" -- bash -lc "mkdir -p '$REMOTE_DIR'"
multipass transfer "$ARCHIVE" "$VM_NAME:/tmp/computer-use-agent.tar.gz"
rm -f "$ARCHIVE"
multipass exec "$VM_NAME" -- bash -lc "tar --warning=no-timestamp --warning=no-unknown-keyword -xzf /tmp/computer-use-agent.tar.gz -C '$REMOTE_DIR' && rm -f /tmp/computer-use-agent.tar.gz"

echo "Installing/updating VM dependencies"
multipass exec "$VM_NAME" -- bash -lc "cd '$REMOTE_DIR' && scripts/ubuntu-setup.sh"

echo "Running real OS-level smoke: uinput -> X11 -> XDamage/capture -> verifier"
multipass exec "$VM_NAME" -- bash -lc "source ~/.cargo/env && cd '$REMOTE_DIR' && DISPLAY_ID='$SMOKE_DISPLAY_ID' scripts/linux-real-smoke.sh"

echo "Running Groq-backed model smoke"
multipass exec "$VM_NAME" -- bash -lc "source ~/.cargo/env && cd '$REMOTE_DIR' && DISPLAY_ID='$SMOKE_DISPLAY_ID' scripts/model-smoke.sh"

echo
echo "Ready. To rerun all tests: ./run.sh"
echo "VM shell: multipass shell $VM_NAME"
echo "Repo in VM: $REMOTE_DIR"
