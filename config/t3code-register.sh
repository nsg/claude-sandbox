#!/bin/bash
set -eu

PORT="${T3CODE_PORT:?}"
BASE_DIR="${T3CODE_BASE_DIR:?}"

export T3CODE_HOME="$BASE_DIR"

# `t3 serve` stopped accepting --auto-bootstrap-project-from-cwd and never
# bootstraps a project; the default command still does, given --no-browser.
exec t3 --host 0.0.0.0 --port "$PORT" --base-dir "$BASE_DIR" \
  --no-browser --auto-bootstrap-project-from-cwd "$@"
