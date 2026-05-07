#!/bin/bash
set -eu

PORT="${T3CODE_PORT:?}"
BASE_DIR="${T3CODE_BASE_DIR:?}"
INSTANCE_NAME="${T3CODE_INSTANCE_NAME:?}"
PROJECT_LABEL="${T3CODE_PROJECT_LABEL:?}"
REGISTRY="/root/.t3/hub-registry/${INSTANCE_NAME}.json"

cleanup() { rm -f "$REGISTRY"; }
trap cleanup EXIT

t3 serve --host 0.0.0.0 --port "$PORT" --base-dir "$BASE_DIR" \
  --auto-bootstrap-project-from-cwd "$@" &
T3_PID=$!

for _ in $(seq 1 30); do
  curl -sf "http://localhost:${PORT}/.well-known/t3/environment" >/dev/null 2>&1 && break
  sleep 1
done

TOKEN=$(t3 auth session issue --ttl 30d --role owner --token-only \
  --base-dir "$BASE_DIR" 2>/dev/null) || true
ENV_ID=$(curl -sf "http://localhost:${PORT}/.well-known/t3/environment" \
  | jq -r '.environmentId // empty') || true

if [ -n "${TOKEN:-}" ] && [ -n "${ENV_ID:-}" ]; then
  mkdir -p /root/.t3/hub-registry
  cat > "$REGISTRY" <<EOF
{
  "label": "${PROJECT_LABEL}",
  "environmentId": "${ENV_ID}",
  "httpBaseUrl": "http://localhost:${PORT}",
  "wsBaseUrl": "ws://localhost:${PORT}",
  "bearerToken": "${TOKEN}",
  "createdAt": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "port": ${PORT}
}
EOF
fi

wait $T3_PID
