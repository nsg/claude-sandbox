---
name: plan-usage
description: Use when an agent inside claude-sandbox needs current Anthropic, OpenAI Codex, or Ollama Cloud plan headroom to choose a runner, schedule substantial work, or answer questions about bucket utilization and reset timing.
---

# Plan Usage

Read the cached public usage summary from the host-side T3 admin server. Use it
before a substantial delegation or fan-out when quota could change provider
routing, and when the user asks for current usage or reset timing. Do not query
it for every ordinary task.

## Connect from the sandbox

The agent runs inside Podman. `localhost` is the container, not the host that
owns the admin server. Use Podman's host alias and the admin port printed when
T3 Code started (3774 unless the host selected a fallback):

```bash
read_plan_usage() {
  local host=host.containers.internal
  local port=${T3CODE_ADMIN_PORT:-3774}
  local response status json

  if ! response=$(curl --noproxy '*' --silent --show-error \
    --connect-timeout 2 --max-time 5 --write-out '\n%{http_code}' \
    "http://${host}:${port}/api/usage"); then
    printf 'Could not reach the host usage endpoint.\n' >&2
    return 1
  fi

  status=${response##*$'\n'}
  json=${response%$'\n'*}
  case $status in
    200|503)
      if [[ -n $json ]] && jq -e '
        .schema_version == 1
        and (.providers | type == "object")
        and (.providers | has("anthropic") and has("openai") and has("ollama"))
      ' <<<"$json" >/dev/null; then
        jq . <<<"$json"
      else
        printf 'The host returned an invalid usage response.\n' >&2
        return 1
      fi
      ;;
    303)
      printf 'The host admin service predates /api/usage; update and restart it.\n' >&2
      return 1
      ;;
    *)
      printf 'The usage endpoint returned HTTP %s.\n' "$status" >&2
      return 1
      ;;
  esac
}

read_plan_usage
```

Do not add `curl --fail`: HTTP 503 carries the valid API body showing that all
provider snapshots are unknown. `--noproxy '*'` keeps the request on the local
container-to-host path even when proxy environment variables are configured.

`host.docker.internal` is an acceptable fallback only if
`host.containers.internal` does not resolve. If 3774 is not the port printed at
startup and `T3CODE_ADMIN_PORT` is unset, ask for that port rather than scanning
the host.

## Interpret the response

The top-level `providers` object always contains `anthropic`, `openai`, and
`ollama`. Each has:

- `freshness`: `fresh`, `stale`, or `unknown`.
- `buckets`: the limits that provider reported. An absent bucket is unknown,
  not 0% used.

Each bucket has:

- `period`: `session`, `weekly`, `monthly`, or `other`.
- `scope`: `overall` or an anonymized `model`-specific limit.
- `used_percent`: an integer from 0 through 100.
- `resets_at`: an RFC 3339 UTC timestamp, or `null` when unknown or unreported.

Treat providers independently. A high percentage near its reset can be less
urgent than a lower percentage with most of its window remaining. Do not infer
headroom from an `unknown` provider, and label conclusions based on `stale` data
as tentative. Parse `resets_at` as an absolute time so elapsed time since the
request does not make a stored countdown wrong.

The response is advisory telemetry sourced from cache files the managed
container updates. It intentionally excludes credentials, account and plan
identifiers, model names, costs, credits, and raw provider errors. Do not use it
as an authentication, billing, or enforcement authority.

## Diagnose access without crossing boundaries

Check the known host alias and admin service only:

```bash
getent hosts host.containers.internal
curl --noproxy '*' --silent --show-error --dump-header - \
  --output /dev/null --connect-timeout 2 --max-time 5 \
  "http://host.containers.internal:${T3CODE_ADMIN_PORT:-3774}/"
```

- A JSON body with HTTP 200 or 503 means the usage endpoint is working.
- HTTP 303 with `Location: /`, or the HTML PIN page from `/api/usage`, means the
  reachable host is running an older admin binary. Report that it must be
  updated and restarted; the usage endpoint itself never needs the PIN.
- Connection refused or timeout usually means the admin server is disabled or
  the port is wrong. The server is started only when T3 admin is enabled; ask
  for the startup-printed admin URL.
- Failure to resolve `host.containers.internal` permits trying
  `host.docker.internal`. Do not guess gateway addresses or scan other ports.

Never submit the admin PIN, reuse portal cookies, or call provider APIs to work
around a missing usage endpoint. Report the unavailable or stale state and
continue using the information already available in the conversation.
