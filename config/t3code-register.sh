#!/bin/bash
set -eu

PORT="${T3CODE_PORT:?}"
BASE_DIR="${T3CODE_BASE_DIR:?}"

exec t3 serve --host 0.0.0.0 --port "$PORT" --base-dir "$BASE_DIR" \
  --auto-bootstrap-project-from-cwd "$@"
