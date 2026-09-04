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

The agent runs inside Podman. T3 Code reverse-forwards the host admin server's
TCP port into the container with pasta, so connect to container-local
`127.0.0.1` and the admin port printed when T3 Code started (3774 unless the
host selected a fallback):

```bash
read_plan_usage() {
  local port=${T3CODE_ADMIN_PORT:-3774}
  local response status json

  if ! response=$(curl --noproxy '*' --silent --show-error \
    --connect-timeout 2 --max-time 5 --write-out '\n%{http_code}' \
    "http://127.0.0.1:${port}/api/usage"); then
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

If 3774 is not the port printed at startup and `T3CODE_ADMIN_PORT` is unset,
ask for that port rather than scanning the host.

## Interpret the response

The top-level `providers` object always contains `anthropic`, `openai`, and
`ollama`. Each has:

- `freshness`: `fresh`, `stale`, or `unknown`.
- `updated_at`: the RFC 3339 UTC time of the last successful observation, or
  `null` when no valid snapshot exists. It may be absent during a rolling
  upgrade from the original schema-v1 server.
- `buckets`: the limits that provider reported. An absent bucket is unknown,
  not 0% used.

Each bucket has:

- `period`: `session`, `weekly`, `monthly`, or `other`.
- `label`: a sanitized provider limit or model display name, when available.
- `window`: the provider's native window name, when useful and available.
- `scope`: `overall` or `model` only when the provider establishes that
  meaning; it is absent when the limit's semantics are provider-specific.
- `used_percent`: an integer from 0 through 100.
- `resets_at`: an RFC 3339 UTC timestamp, or `null` when unknown or unreported.

Treat providers independently. A high percentage near its reset can be less
urgent than a lower percentage with most of its window remaining. Do not infer
headroom from an `unknown` provider, infer a model scope when `scope` is absent,
or treat `label` as a stable identifier. Prefer the label when describing a
bucket, and label conclusions based on `stale` data as tentative. Parse
`updated_at` and `resets_at` as absolute times so elapsed time since the request
does not make a stored countdown wrong.

The response is advisory, account-global telemetry from a host cache. A single
elected collector refreshes each provider independently every 30 minutes; the
endpoint itself is passive. It retains provider limit and model labels needed
to interpret the percentages, while excluding credentials and tokens, account
identifiers, billing amounts, costs and credits, and raw provider errors. Do
not use it as an authentication, billing, or enforcement authority.

## Guard long-running delegated work

Jobs expected to remain active for roughly 30 minutes or longer need a usage
guard in their brief. Name the worker's billing provider explicitly as
`anthropic`, `openai`, or `ollama`; a nested worker uses its own provider, not
its parent's. The worker should:

1. Read usage at startup and about once per 30 minutes of active work, ideally
   at a durable task boundary. The cache refreshes on that cadence, so faster
   polling normally adds no information.
2. Inspect only its assigned provider. Within that provider, consider overall
   buckets and any model bucket that clearly applies to the selected model or
   tier; ignore unrelated providers and model buckets. Do not invent a match
   when the response leaves a bucket's scope ambiguous.
3. Compare remaining headroom, reset time, observed consumption since the last
   reading, and estimated work remaining. Act before a relevant bucket is
   likely to reach 100% before the next check or before completion. A large
   percentage alone is not decisive when its reset is imminent.
4. Before pausing, waiting, or stopping, make the work resumable: save partial
   output, record completed and pending steps, the last successful checks, the
   applicable usage snapshot, and the exact next action. Stop starting new
   child work once the guard trips.
5. Choose the least disruptive safe response: reduce concurrency, throttle or
   defer optional work; checkpoint and end cleanly; or, when a known reset is
   close, arrange a detached wait/resume and recheck after the reset. Short
   session windows are especially suitable for checkpoint-and-resume. Do not
   keep an interactive orchestrator turn blocked in a long blind sleep.

If the worker's sandbox cannot reach the host endpoint, do not weaken the
sandbox for telemetry. Have the orchestrator perform the same 30-minute check
and expose only that worker's provider snapshot through the runner's supported
follow-up mechanism or a shared status file. `stale` or `unknown` data is not
proof of available headroom: checkpoint conservatively and report the missing
signal rather than querying provider APIs or guessing.

## Diagnose access without crossing boundaries

Check the reverse-forwarded admin service only:

```bash
curl --noproxy '*' --silent --show-error --dump-header - \
  --output /dev/null --connect-timeout 2 --max-time 5 \
  "http://127.0.0.1:${T3CODE_ADMIN_PORT:-3774}/"
```

- A JSON body with HTTP 200 or 503 means the usage endpoint is working.
- HTTP 303 with `Location: /`, or the HTML PIN page from `/api/usage`, means the
  reachable host is running an older admin binary. Report that it must be
  updated and restarted; the usage endpoint itself never needs the PIN.
- Connection refused or timeout usually means the admin server is disabled or
  the port is wrong. The server is started only when T3 admin is enabled; ask
  for the startup-printed admin URL.
- Connection refused or timeout can also mean the container was launched
  without pasta's reverse TCP forwarding. Do not guess gateway addresses or
  scan other ports.

Never submit the admin PIN, reuse portal cookies, or call provider APIs to work
around a missing usage endpoint. Report the unavailable or stale state and
continue using the information already available in the conversation.
