#!/usr/bin/env bash
set -euo pipefail

case "$(uname -s)" in
  Darwin)
    exec scripts/vm-run.sh "$@"
    ;;
  Linux)
    source "$HOME/.cargo/env" 2>/dev/null || true
    scripts/ubuntu-setup.sh
    scripts/linux-real-smoke.sh
    scripts/model-smoke.sh
    ;;
  *)
    echo "Unsupported host OS: $(uname -s)" >&2
    exit 1
    ;;
esac
