#!/bin/bash
set -eu

PORT="${T3CODE_PORT:?}"
BASE_DIR="${T3CODE_BASE_DIR:?}"

export T3CODE_HOME="$BASE_DIR"

# The t3 package only downloads the real release into BASE_DIR, so the client
# can't be patched in the image. `t3 --version` triggers that download. A
# failed patch must not keep T3 Code from starting.
if version=$(t3 --version); then
  t3code-full-paths "$BASE_DIR/runtime/versions/${version##* v}/client" || true
fi

# `t3 serve` stopped accepting --auto-bootstrap-project-from-cwd and never
# bootstraps a project; the default command still does, given --no-browser.
exec t3 --host 0.0.0.0 --port "$PORT" --base-dir "$BASE_DIR" \
  --no-browser --auto-bootstrap-project-from-cwd "$@"
