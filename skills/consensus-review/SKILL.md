---
name: consensus-review
description: Cross-vendor code critique. Anthropic models (Opus critic by default, Fable for judging and the hardest targets, Sonnet legwork) and OpenAI Codex (GPT-5.6-Sol) each independently review the codebase, then debate each other's findings over several rounds to reach a consensus on real issues and good design. Use when the user asks for a consensus review, cross-model critique, or "get a second opinion from Codex".
---

# Consensus Review

Two independent model families review the same codebase, then argue until they agree. The value comes from independence first (blind opinions), adversarial exchange second (defend or concede), and only then synthesis. Do not partition the codebase between the sides — each side reviews the whole thing and forms its own overall opinion.

## Roles

- **Anthropic side**: use a native Anthropic subagent when the orchestrator provides one; otherwise run Claude Code using the `claude` skill. **Opus is the default critic** — it is a strong reviewer at a fraction of Fable's cost, and most review targets don't need more. Escalate the critic to Fable only when the target genuinely demands the extra depth: subtle concurrency/cancellation semantics, security-sensitive logic, architecture-level judgment calls, or a prior Opus pass that missed things. Sonnet may do cheap mechanical legwork (inventories, grep sweeps, metrics) feeding the critic.
- **OpenAI side**: use a native GPT-5.6-Sol worker at `high` reasoning when the orchestrator provides one; otherwise run it through the Codex CLI using the `codex` skill.

## Invoking external critics

Keep model and effort choices per invocation; never write delegation defaults into user configuration. Follow the `claude` or `codex` skill for the selected external critic.

1. Write each review prompt to a file in the run's durable state directory (below) — avoids shell-quoting hell and survives restarts.
2. Run external critics under a hard timeout. For OpenAI:
   ```
   STATE=/workspace/.claude-sandbox/consensus-YYYY-MM-DD
   REPO=/workspace
   cd "$REPO" && timeout 45m codex exec --skip-git-repo-check \
     -m gpt-5.6-sol -c model_reasoning_effort=high \
     -o "$STATE/codex-out.md" - < "$STATE/codex-prompt.txt"
   ```
   For Anthropic:
   ```
   STATE=/workspace/.claude-sandbox/consensus-YYYY-MM-DD
   REPO=/workspace
   cd "$REPO" && timeout 45m claude -p --model opus --effort high \
     --permission-mode plan --no-session-persistence \
     < "$STATE/claude-prompt.txt" > "$STATE/claude-out.md" \
     2> "$STATE/claude-log.txt"
   ```
   Size the timeout to ~3× the expected runtime (a full-repo review has taken ~15 min; a single-diff re-review less). Either runner can hang indefinitely — the bound converts a hang into an exit, which makes downstream wake-up mechanisms fire.
3. Long runs: launch in the background and read the saved report on completion. Launch from the **orchestrating session's** background shell, not from inside a short-lived subagent — a runner process may be killed when that subagent's turn ends.

## Long runs: never trust a single wake-up

Proven failure (2026-08-03, camon): a Codex re-review hung with no timeout, the armed watcher had itself already died, the output file was in the session scratchpad, and the session ended its turn saying "the completion notification will wake me." Four hours of silence, then a restart wiped the scratchpad — verdict lost, work repeated. Every layer below existed to prevent this and each was skipped or fragile; treat them as mandatory together, not alternatives:

- **Durable state directory.** All run artifacts — prompts, `-o` outputs, debate transcripts — go in a directory that survives session restarts (e.g. `/workspace/.claude-sandbox/consensus-<date>/`), never the session scratchpad. Keep a `PROGRESS.md` there with the exact resume point, updated at every phase boundary and before every turn-ending wait.
- **Hard timeout on every external runner** (above). On exit 124, retry once; if the retry also times out, stop and report the stall instead of looping.
- **Independent watcher.** Arm one single-shot background watcher in the orchestrating session with its own deadline slightly above the runner timeout: loop with `sleep 60` until the output file is non-empty (print a success line) or the runner process is gone without writing it (print a distinct failure line), and exit unconditionally at the deadline so the watcher cannot hang either. Check the selected runner with a self-excluding pattern such as `pgrep -f '[c]odex exec'` or `pgrep -f '[c]laude -p'`; a plain pattern matches the `pgrep` command itself and can falsely report a live process. **Verify the watcher survived its first minute** before ending the turn — a watcher that crashed at spawn guards nothing, and this exact failure has happened.
- **Wake on evidence, not on notifications.** Whichever signal arrives first — runner completion, watcher line, or a scheduled restart — decide from the files: output non-empty → proceed with it; prompt saved but output empty/absent → relaunch from the saved prompt. Never end a turn waiting on a background process unless at least two independently-bounded wake sources are live and `PROGRESS.md` would let a cold session resume without this conversation.

Notes:
- Codex's default sandbox and Claude's `plan` permission mode are read-only — correct for review; don't loosen them.
- Effort stays at `high`. `xhigh`/`max` burn tokens for very little extra smarts.
- Use `gpt-5.6-sol` for OpenAI and Opus for the default Anthropic critic; reserve Fable for the hardest reviews and final arbitration.

## Protocol

**Round 1 — blind opinions.** Each side reviews the full codebase and writes an opinionated report: concrete findings (file:line, severity, why it's bad, what better looks like) plus an overall design assessment. Honest and specific — actual bugs, bad practices, ugly or poorly structured code, bad patterns. No praise padding, no formatter-level nits. Neither side sees the other's report yet. Save both reports to the scratchpad.

**Round 2+ — debate.** Hand each side the other's report. Each responds per finding: agree (and why), dispute (with evidence from the code), or concede its own when refuted. Feed responses back and forth until positions stabilize — usually 2–3 exchanges. Every exchange must cite code, not authority. Keep the full exchange in scratchpad files; each debate turn gets the accumulated transcript.

**Consensus.** Fable judges the finished debate, natively in an Anthropic harness or through the `claude` skill from another harness — this is where Fable's depth belongs, so don't delegate the judging down-tier. The judge reads the disputed code itself and formulates its own opinion — a third reviewer on contested points, not a vote counter: findings both sides endorse are confirmed; still-contested findings get a ruling with reasoning; note what each side uniquely caught. Produce one final report: confirmed issues ranked by severity, contested points with both positions and the ruling, and the shared design-level assessment.

## Never let two agents write the same tree

Reviewers that mutation-test (apply a change, run the suite, revert) are writing to the working tree, even though they restore afterwards. If an implementer is editing the same files at the same time, a restore silently overwrites its edits and leaves a hybrid that does not compile — and neither agent knows why. Serialize: an implementer runs, then reviewers run, then the implementer resumes. Never both at once on overlapping files. When a reviewer does mutate, require it to save a copy of the version it reviewed so the two states can be diffed if something goes wrong.

## Token discipline

Default to one critic per side plus the debate rounds. Scale up (multiple critics per side, extra lenses) only when the user asks for a thorough/"plenty of agents" review. Don't add rounds after positions stop moving.
