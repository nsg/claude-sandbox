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
  -m gpt-5.6-sol -c model_reasoning_effort=high \
  -o $S/report.md - < $S/brief.md > $S/log.txt 2>&1
```

- **Launch from the orchestrator's own background Bash** (`run_in_background: true`), never from inside a subagent — a codex process started by a subagent dies when that subagent's turn ends. The harness completion notification replaces any watcher; do not poll.
- `-o <file>` writes codex's final message to a file, keeping a long report out of context until deliberately read.
- Write the brief to a file and pipe it via `- < brief.md`; don't inline large prompts as shell arguments.
- Reasoning effort: `-c model_reasoning_effort=high|medium|low`.
- Ensure the shell cwd is the target repo before launching; `--sandbox workspace-write` scopes writes to cwd.
- For read-only review work, omit `--sandbox` — the default read-only sandbox suffices (this is the consensus-review pattern).

## Sandbox limits — plan the verification split up front

`workspace-write` **blocks socket creation**: every test that binds a TCP listener fails with EPERM (seen: 122 of camon's tests). A well-briefed codex will correctly refuse to commit on a failed gate, so divide the work when the gates need sockets:

1. Codex: edit + fmt + clippy + socket-free checks, **no commit**, report state.
2. Orchestrator: re-run mechanical checks (scope, diff-property greps), run the socket-dependent test suites, inspect samples, then commit.

## Briefing style

GPT models are excellent literal instruction-followers; the brief is the quality lever. Be exhaustive and specific:

- State inherited/partial state explicitly ("do NOT discard the uncommitted diff; re-visit partially-done files").
- Enumerate scope precisely: globs, explicit exclusions, out-of-scope paths.
- Give mechanical self-check commands (exact grep/diff invocations) to run before the gates.
- List gates with expected reference outputs — test totals, known flakes and how to treat them.
- Spell out commit identity/format rules verbatim; forbid push, branches, and parallel workers.
- Define the exact report format wanted in the final message (it lands in the `-o` file).

## Failure modes

- Timeouts recur under load (seen repeatedly 2026-08): retry once, then stop and reassess — don't retry-loop.
- Exit 0 + report file present = normal completion; the report may still say "blocked", so read it before assuming success.
