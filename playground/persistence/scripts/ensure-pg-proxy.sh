#!/usr/bin/env bash
# Ensure localhost:54329 forwards to docker neondb-local (avoids host Postgres on :5432).
set -euo pipefail
NAME=claude-code-pg-proxy
PORT=54329

if docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
  echo "proxy already running: $NAME → 127.0.0.1:${PORT}"
  exit 0
fi

if ! docker ps --format '{{.Names}}' | grep -qx neondb-local; then
  echo "ERROR: container neondb-local is not running" >&2
  exit 1
fi

NET=$(docker inspect neondb-local -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}}{{end}}' | awk 'NR==1{print; exit}')
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" --network "$NET" -p "127.0.0.1:${PORT}:5432" \
  alpine/socat TCP-LISTEN:5432,fork,reuseaddr TCP:neondb-local:5432 >/dev/null
echo "started $NAME → 127.0.0.1:${PORT} → neondb-local:5432"
