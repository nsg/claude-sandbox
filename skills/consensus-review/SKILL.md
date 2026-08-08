---
name: consensus-review
description: Cross-vendor code critique. Anthropic models (Opus critic by default, Fable for judging and the hardest targets, Sonnet legwork) and OpenAI Codex (GPT-5.6-Sol) each independently review the codebase, then debate each other's findings over several rounds to reach a consensus on real issues and good design. Use when the user asks for a consensus review, cross-model critique, or "get a second opinion from Codex".
---

# Consensus Review

Two independent model families review the same codebase, then argue until they agree. The value comes from independence first (blind opinions), adversarial exchange second (defend or concede), and only then synthesis. Do not partition the codebase between the sides — each side reviews the whole thing and forms its own overall opinion.

## Roles

- **Anthropic side**: a subagent forms the opinion. **Opus is the default critic** — it is a strong reviewer at a fraction of Fable's cost, and most review targets don't need more. The orchestrator (Fable) decides per task whether this is an Opus problem or a Fable problem: escalate the critic to Fable only when the target genuinely demands the extra depth — subtle concurrency/cancellation semantics, security-sensitive logic, architecture-level judgment calls, or a prior Opus pass that missed things. Both models have their own strengths and blind spots; don't burn Fable tokens where Opus suffices. Sonnet may do cheap mechanical legwork (inventories, grep sweeps, metrics) feeding the critic. Haiku only as a dumb wrapper (below).
- **OpenAI side**: Codex CLI running GPT-5.6-Sol at `high` reasoning effort. You cannot spawn OpenAI models directly — spawn a Haiku subagent whose only job is to run `codex exec` via Bash and relay the output verbatim, without summarizing or editing.

## Invoking Codex

Per-invocation flags only — never write model defaults into `~/.codex/config.toml`.

1. Write the review prompt to a file in the run's durable state directory (below) — avoids shell-quoting hell and survives restarts.
2. Run, always under a hard timeout:
   ```
   cd <repo> && timeout 45m codex exec --skip-git-repo-check \
     -m gpt-5.6-sol -c model_reasoning_effort=high \
     -o <state>/codex-out.md "$(cat <state>/codex-prompt.txt)"
   ```
   Size the timeout to ~3× the expected runtime (a full-repo review has taken ~15 min; a single-diff re-review less). `codex exec` can hang indefinitely — the bound is what converts a hang into an exit, which is what makes every downstream wake-up mechanism actually fire.
3. Long runs: launch in the background and read `codex-out.md` on completion; the file contains only the final message. Launch from the **orchestrating session's** background Bash, not from inside a subagent — a subagent-launched codex is killed when the subagent's turn ends. The Haiku wrapper is only for short runs that finish within its turn.

## Long runs: never trust a single wake-up

Proven failure (2026-08-03, camon): a Codex re-review hung with no timeout, the armed watcher had itself already died, the output file was in the session scratchpad, and the session ended its turn saying "the completion notification will wake me." Four hours of silence, then a restart wiped the scratchpad — verdict lost, work repeated. Every layer below existed to prevent this and each was skipped or fragile; treat them as mandatory together, not alternatives:

- **Durable state directory.** All run artifacts — prompts, `-o` outputs, debate transcripts — go in a directory that survives session restarts (e.g. `/workspace/.claude-sandbox/consensus-<date>/`), never the session scratchpad. Keep a `PROGRESS.md` there with the exact resume point, updated at every phase boundary and before every turn-ending wait.
- **Hard timeout on codex** (above). On exit 124, retry once; if the retry also times out, stop and report the stall instead of looping.
- **Independent watcher.** Arm one single-shot background watcher in the orchestrating session with its own deadline slightly above the codex timeout: loop with `sleep 60` until the output file is non-empty (print a success line) or the codex process is gone without writing it (print a distinct failure line), and exit unconditionally at the deadline so the watcher cannot hang either. Check the process with `pgrep -f '[c]odex exec'` — a plain pattern matches the pgrep command itself and reports the process as running after it has exited. **Verify the watcher survived its first minute** (`TaskOutput` / check it's still running) before ending the turn — a watcher that crashed at spawn guards nothing, and this exact failure has happened.
- **Wake on evidence, not on notifications.** Whichever signal arrives first — codex completion, watcher line, or a scheduled restart — decide from the files: output non-empty → proceed with it; prompt saved but output empty/absent → relaunch from the saved prompt. Never end a turn waiting on a background process unless at least two independently-bounded wake sources are live and `PROGRESS.md` would let a cold session resume without this conversation.

Notes:
- Codex's default sandbox is read-only — correct for review; don't loosen it.
- Effort stays at `high`. `xhigh`/`max` burn tokens for very little extra smarts.
- Model: `gpt-5.6-sol` is the current best (2026-07). If in doubt, check `~/.codex/models_cache.json` for what's available and pick the frontier coding model.

## Protocol

**Round 1 — blind opinions.** Each side reviews the full codebase and writes an opinionated report: concrete findings (file:line, severity, why it's bad, what better looks like) plus an overall design assessment. Honest and specific — actual bugs, bad practices, ugly or poorly structured code, bad patterns. No praise padding, no formatter-level nits. Neither side sees the other's report yet. Save both reports to the scratchpad.

**Round 2+ — debate.** Hand each side the other's report. Each responds per finding: agree (and why), dispute (with evidence from the code), or concede its own when refuted. Feed responses back and forth until positions stabilize — usually 2–3 exchanges. Every exchange must cite code, not authority. Keep the full exchange in scratchpad files; each debate turn gets the accumulated transcript.

**Consensus.** Fable judges the finished debate (the orchestrating session may do this itself) — this is where Fable's depth belongs, so don't delegate the judging down-tier. The judge reads the disputed code itself and formulates its own opinion — a third reviewer on contested points, not a vote counter: findings both sides endorse are confirmed; still-contested findings get a ruling with reasoning; note what each side uniquely caught. Produce one final report: confirmed issues ranked by severity, contested points with both positions and the ruling, and the shared design-level assessment.

## Never let two agents write the same tree

Reviewers that mutation-test (apply a change, run the suite, revert) are writing to the working tree, even though they restore afterwards. If an implementer is editing the same files at the same time, a restore silently overwrites its edits and leaves a hybrid that does not compile — and neither agent knows why. Serialize: an implementer runs, then reviewers run, then the implementer resumes. Never both at once on overlapping files. When a reviewer does mutate, require it to save a copy of the version it reviewed so the two states can be diffed if something goes wrong.

## Token discipline

Default to one critic per side plus the debate rounds. Scale up (multiple critics per side, extra lenses) only when the user asks for a thorough/"plenty of agents" review. Don't add rounds after positions stop moving.
