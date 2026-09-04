---
name: claude
description: Delegate architecture, implementation, review, or research to Anthropic models (Fable/Opus/Sonnet/Haiku) through the Claude Code CLI. Use from Codex, opencode, or another non-Claude orchestrator when work should consume the Anthropic subscription, needs an independent Anthropic opinion, or calls for Claude-specific design judgment.
---

# Claude Delegation

Run Anthropic models non-interactively through `claude -p`. Which model to pick is ranked in the `delegate` skill; this skill covers runner mechanics. From an existing Claude Code session, prefer its native `Agent` tool instead of starting a nested CLI.

## One-shot invocation (verified with Claude Code 2.1.226)

```bash
TASK_STATE=/workspace/.claude-sandbox/claude-delegation
claude -p --model opus --effort high --permission-mode plan \
  --no-session-persistence \
  < "$TASK_STATE/brief.md" \
  > "$TASK_STATE/report.md" 2> "$TASK_STATE/log.txt"
```

- Run with the shell cwd set to the target repository. Claude discovers its `CLAUDE.md`, skills, and project settings from there; do not use `--bare` when those instructions matter.
- Put long briefs in a file and pass them on stdin. Capture stdout as the report and stderr as the operational log.
- Use model aliases `fable`, `opus`, `sonnet`, or `haiku`; set effort with `--effort low|medium|high|xhigh|max`, subject to model support.
- Keep `--no-session-persistence` for disposable one-shot work. Omit it when the result may need a follow-up, then continue with `claude -c -p`. Use JSON output when an exact session ID must be captured for `claude -r <session-id> -p`.
- For machine-readable results, use `--output-format json`; use `--json-schema` when a downstream step requires a strict shape.

## Permissions

Claude Code is an agent permission system, not an OS sandbox. Choose the narrowest mode that can complete the task:

- Read-only exploration or review: `--permission-mode plan`.
- Edits that do not require arbitrary shell commands: `--permission-mode acceptEdits`.
- Autonomous implementation: prefer `--permission-mode auto` when the account supports it, or combine `acceptEdits` with narrow `--allowedTools` entries for the required checks.
- Do not default to `--dangerously-skip-permissions` / `bypassPermissions`. It removes Claude's permission checks, and this development container has network access plus selected host bridges. Use it only when the owner explicitly authorizes that risk for the task.

State destructive-action, commit, push, deployment, and external-write boundaries in the brief even when the permission mode should block them.

## Long-running work

Prefer a foreground `claude -p` process launched from the orchestrator's own background shell when the harness provides reliable process completion; this keeps edits in the current tree and produces an explicit report file. A child launched by a short-lived subagent may die with that subagent. Keep the report and log under `/workspace/.claude-sandbox/`, not `/tmp` or the home directory.

For work expected to remain active for roughly 30 minutes or longer, include
the `delegate` skill's long-running usage guard in the brief with provider
`anthropic`. Require any nested delegates to receive their own guard for the
provider they actually use.

Claude Code also supports detached sessions:

```bash
claude --bg --model opus --effort high --permission-mode plan "Review the repository"
claude agents --json
claude logs <session-id>
```

Use `--bg` only when detached-session management is specifically useful, primarily for independent or read-only work. It is not the default for ordinary one-shot delegation. Background sessions may isolate edits in a Claude-managed worktree, so do not use them for changes expected in the current tree unless the brief and handoff explicitly account for that worktree.

## Briefing and verification

- State inherited dirty-tree state and forbid discarding unrelated changes.
- Define the exact scope, exclusions, constraints, gates, and final report format.
- Tell the delegate whether it may edit, commit, push, contact external systems, or start more agents.
- Use Fable for architecture and arbitration, Opus for design-sensitive implementation or critique, Sonnet for ordinary work and legwork, and Haiku only for simple operations; consult `delegate` for contested routing.
- Treat the report as a claim, not proof. Inspect the diff and rerun the relevant checks in the orchestrating harness before accepting or committing changes.

## Failure modes

- Check `claude auth status` if a run fails before producing model output.
- Bound synchronous calls with a task-appropriate timeout. Retry a transient service failure once; do not retry-loop quota or authentication errors.
- Exit zero means the CLI completed, not that the delegated objective succeeded. Read the report for blockers and verify the resulting tree.
