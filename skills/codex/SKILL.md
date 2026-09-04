---
name: codex
description: Delegate implementation or review work to OpenAI GPT-5.6 models (Sol/Terra/Luna) via the Codex CLI — invocation pattern, sandbox limits, briefing style, verification split
---

# Codex Delegation

Run OpenAI models non-interactively through `codex exec` to offload work from the Anthropic budget. Which model/effort to pick is ranked in the `delegate` skill; this skill is the how.

## Invocation (proven 2026-08)

```bash
S=<scratchpad>
codex exec --skip-git-repo-check --sandbox workspace-write \
  -c sandbox_workspace_write.network_access=true \
  -c 'approval_policy="never"' \
  -m gpt-5.6-sol -c model_reasoning_effort=high \
  -o $S/report.md - < $S/brief.md > $S/log.txt 2>&1
```

- **Launch from the orchestrator's own background Bash** (`run_in_background: true`), never from inside a subagent — a codex process started by a subagent dies when that subagent's turn ends. The harness completion notification replaces any watcher; do not poll.
- `-o <file>` writes codex's final message to a file, keeping a long report out of context until deliberately read.
- Write the brief to a file and pipe it via `- < brief.md`; don't inline large prompts as shell arguments.
- Reasoning effort: `-c model_reasoning_effort=high|medium|low`.
- Ensure the shell cwd is the target repo before launching; `--sandbox workspace-write` scopes writes to cwd, while the paired network override deliberately allows unrestricted outbound access.
- For read-only review work, omit `--sandbox` — the default read-only sandbox suffices (this is the consensus-review pattern).

## Sandbox and network boundary

The explicit network override permits outbound connections and local socket
binding while retaining the `workspace-write` filesystem boundary. This was
verified with Codex CLI 0.152.1. It does not expose host services that were not
forwarded into the container, make paths outside the writable roots editable,
or provide host credentials. Keep delegated changes uncommitted for the
orchestrator to inspect, test, and commit.

## Briefing style

GPT models are excellent literal instruction-followers; the brief is the quality lever. Be exhaustive and specific:

- State inherited/partial state explicitly ("do NOT discard the uncommitted diff; re-visit partially-done files").
- For work expected to remain active for roughly 30 minutes or longer, include
  the `delegate` skill's long-running usage guard with provider `openai` and
  propagate an adapted guard to nested delegates. The network-enabled worker
  should read the usage endpoint directly; if the admin service is unavailable,
  checkpoint and report that rather than querying provider APIs.
- Enumerate scope precisely: globs, explicit exclusions, out-of-scope paths.
- Give mechanical self-check commands (exact grep/diff invocations) to run before the gates.
- List gates with expected reference outputs — test totals, known flakes and how to treat them.
- Spell out commit identity/format rules verbatim; forbid push, branches, and parallel workers.
- Define the exact report format wanted in the final message (it lands in the `-o` file).

## Failure modes

- Timeouts recur under load (seen repeatedly 2026-08): retry once, then stop and reassess — don't retry-loop.
- Exit 0 + report file present = normal completion; the report may still say "blocked", so read it before assuming success.
