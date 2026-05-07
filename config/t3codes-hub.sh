#!/bin/bash
set -eu

PORT="${T3CODE_PORT:?}"
BASE_DIR="${T3CODE_BASE_DIR:?}"
BOOTSTRAP_HTML="${T3CODE_BOOTSTRAP_HTML:?}"

cp "$BOOTSTRAP_HTML" /tmp/hub-bootstrap.html
rm -f /tmp/bootstrap-ready /tmp/bootstrap-done

PORT="$PORT" node /usr/local/lib/t3codes-bootstrap-server.js &
BOOTSTRAP_PID=$!

while [ ! -f /tmp/bootstrap-ready ]; do sleep 0.1; done
while [ ! -f /tmp/bootstrap-done ]; do sleep 0.5; done

wait $BOOTSTRAP_PID 2>/dev/null

exec t3 --host 0.0.0.0 --port "$PORT" --base-dir "$BASE_DIR" "$@"
