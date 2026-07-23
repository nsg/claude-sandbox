#!/bin/bash
set -eu

PORT="${T3CODE_PORT:?}"
BASE_DIR="${T3CODE_BASE_DIR:?}"

export T3CODE_HOME="$BASE_DIR"

PAIR_ADMIN_PID=""
if [ -n "${T3CODE_PAIR_ADMIN_PORT:-}" ]; then
  node /usr/local/lib/t3code-pair-admin.js &
  PAIR_ADMIN_PID=$!
fi

cleanup() {
  if [ -n "$PAIR_ADMIN_PID" ]; then
    kill "$PAIR_ADMIN_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

t3 serve --host 0.0.0.0 --port "$PORT" --base-dir "$BASE_DIR" \
  --auto-bootstrap-project-from-cwd "$@"
