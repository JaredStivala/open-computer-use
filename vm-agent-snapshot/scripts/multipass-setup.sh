#!/usr/bin/env bash
set -euo pipefail

VM_NAME="${VM_NAME:-computer-use-agent}"
IMAGE="${IMAGE:-22.04}"
CPUS="${CPUS:-4}"
MEMORY="${MEMORY:-6G}"
DISK="${DISK:-30G}"
REMOTE_DIR="/home/ubuntu/computer-use-agent"
ARCHIVE="/tmp/computer-use-agent.tar.gz"

if ! command -v multipass >/dev/null 2>&1; then
  echo "multipass is required on macOS host" >&2
  exit 1
fi

if ! multipass info "$VM_NAME" >/dev/null 2>&1; then
  multipass launch "$IMAGE" --name "$VM_NAME" --cpus "$CPUS" --memory "$MEMORY" --disk "$DISK"
else
  multipass start "$VM_NAME" >/dev/null 2>&1 || true
fi

COPYFILE_DISABLE=1 tar \
  --no-xattrs \
  --exclude .git \
  --exclude target \
  --exclude "$ARCHIVE" \
  -czf "$ARCHIVE" \
  .

multipass exec "$VM_NAME" -- bash -lc "rm -rf '$REMOTE_DIR' && mkdir -p '$REMOTE_DIR'"
multipass transfer "$ARCHIVE" "$VM_NAME:/tmp/computer-use-agent.tar.gz"
rm -f "$ARCHIVE"
multipass exec "$VM_NAME" -- bash -lc "tar --warning=no-timestamp --warning=no-unknown-keyword -xzf /tmp/computer-use-agent.tar.gz -C '$REMOTE_DIR' && rm -f /tmp/computer-use-agent.tar.gz"

multipass exec "$VM_NAME" -- bash -lc "cd '$REMOTE_DIR' && scripts/ubuntu-setup.sh"
multipass exec "$VM_NAME" -- bash -lc "source ~/.cargo/env && cd '$REMOTE_DIR' && cargo build --workspace"
multipass exec "$VM_NAME" -- bash -lc "source ~/.cargo/env && cd '$REMOTE_DIR' && DISPLAY_ID=:99 scripts/linux-real-smoke.sh"
multipass exec "$VM_NAME" -- bash -lc "source ~/.cargo/env && cd '$REMOTE_DIR' && DISPLAY_ID=:99 scripts/model-smoke.sh"

echo "VM setup complete."
multipass info "$VM_NAME"
echo "Repo inside VM: $REMOTE_DIR"
echo "Run another real Linux smoke test with:"
echo "  multipass exec $VM_NAME -- bash -lc 'source ~/.cargo/env && cd $REMOTE_DIR && DISPLAY_ID=:99 scripts/linux-real-smoke.sh'"
echo "Run another Groq-backed model smoke test with:"
echo "  multipass exec $VM_NAME -- bash -lc 'source ~/.cargo/env && cd $REMOTE_DIR && DISPLAY_ID=:99 scripts/model-smoke.sh'"
