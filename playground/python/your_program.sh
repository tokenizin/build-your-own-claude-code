#!/bin/sh
#
# Run the Python full-feature reference agent locally.

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLAYGROUND_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

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

PYTHONSAFEPATH=1 PYTHONPATH="$SCRIPT_DIR" exec uv run \
  --project "$SCRIPT_DIR" \
  --quiet \
  -m app.main \
  "$@"
