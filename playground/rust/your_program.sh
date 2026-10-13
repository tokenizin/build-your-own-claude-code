#!/bin/sh
#
# Run the Rust full-feature reference agent locally.
# Builds a release binary optimized for the host (Apple Silicon Ultra friendly).

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLAYGROUND_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${CODECRAFTERS_RUST_TARGET_DIR:-/tmp/codecrafters-build-claude-code-rust-playground}"

# Load playground/.env (and optional local override) without printing secrets
load_env() {
  env_file="$1"
  if [ -f "$env_file" ]; then
    # shellcheck disable=SC1090
    set -a
    . "$env_file"
    set +a
  fi
}

load_env "$PLAYGROUND_DIR/.env"
load_env "$PLAYGROUND_DIR/.env.local"
load_env "$SCRIPT_DIR/.env"

(
  cd "$SCRIPT_DIR"
  # Native CPU features on Apple Silicon / Ultra
  RUSTFLAGS="${RUSTFLAGS:--C target-cpu=native}" cargo build --release \
    --target-dir="$TARGET_DIR" \
    --manifest-path Cargo.toml
)

exec "$TARGET_DIR/release/codecrafters-claude-code" "$@"
