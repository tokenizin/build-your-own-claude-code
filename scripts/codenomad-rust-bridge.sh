#!/usr/bin/env bash
# Thin wrapper around codenomad-rust-bridge.py (P4).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
exec python3 "$SCRIPT_DIR/codenomad-rust-bridge.py" "$@"
