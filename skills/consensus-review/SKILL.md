---
name: consensus-review
description: Cross-vendor code critique. Anthropic models (Opus critic by default, Fable for judging and the hardest targets, Sonnet legwork) and OpenAI Codex (GPT-6.1-Sol) each independently review the codebase, then debate each other's findings over several rounds to reach a consensus on real issues and good design. Use when the user asks for a consensus review, cross-model critique, or "get a second opinion from Codex".
---

# Consensus Review

Two independent model families review the same codebase, then argue until they agree. The value comes from independence first (blind opinions), adversarial exchange second (defend or concede), and only then synthesis. Do not partition the codebase between the sides — each side reviews the whole thing and forms its own overall opinion.

## Roles

- **Anthropic side**: **Opus is the default critic** — it is a strong reviewer at a fraction of Fable's cost, and most review targets don't need more. Escalate the critic to Fable only when the target genuinely demands the extra depth: subtle concurrency/cancellation semantics, security-sensitive logic, architecture-level judgment calls, or a prior Opus pass that missed things. Sonnet may do cheap mechanical legwork (inventories, grep sweeps, metrics) feeding the critic.
- **OpenAI side**: GPT-6.1-Sol at `high` reasoning.

Keep model and effort choices per run; never write delegation defaults into user configuration. Effort stays at `high` — `xhigh`/`max` burn tokens for very little extra smarts. Reserve Fable for the hardest reviews and final arbitration. Brief each side the way the `delegate` skill describes for its vendor, and say "read-only, edit nothing" in every critic brief.

## Durable state

Proven failure (2026-08-03, camon): the output of a long Codex re-review was in the session scratchpad, a restart wiped it, and the verdict was lost and the work repeated. So:

- **Everything lives in a durable state directory**, e.g. `/workspace/.claude-sandbox/consensus-<date>/` — prompts, reports, debate transcripts — never the session scratchpad.
- **Save each prompt there before launching the critic**, and have each critic write its full report to `<side>-round<N>.md` there, ending with a short summary and the path.
- **Keep a `PROGRESS.md`** with the exact resume point, updated at every phase boundary and before every turn-ending wait, so a cold session could resume without this conversation.
- **Decide from the files, not from memory.** On any wake: report non-empty → proceed with it; prompt saved but report empty or absent → relaunch that critic from the saved prompt. A critic that stalls is stopped and relaunched from its saved prompt, not waited on indefinitely.

## Protocol

**Round 1 — blind opinions.** Each side reviews the full codebase and writes an opinionated report: concrete findings (file:line, severity, why it's bad, what better looks like) plus an overall design assessment. Honest and specific — actual bugs, bad practices, ugly or poorly structured code, bad patterns. No praise padding, no formatter-level nits. Neither side sees the other's report yet. Launch both sides together.

**Round 2+ — debate.** Hand each side the other's report. Each responds per finding: agree (and why), dispute (with evidence from the code), or concede its own when refuted. Feed responses back and forth until positions stabilize — usually 2–3 exchanges. Every exchange must cite code, not authority. Keep the full exchange in the state directory; each debate turn is a fresh critic run that gets the accumulated transcript.

**Consensus.** Fable judges the finished debate — this is where Fable's depth belongs, so don't delegate the judging down-tier. The judge reads the disputed code itself and formulates its own opinion — a third reviewer on contested points, not a vote counter: findings both sides endorse are confirmed; still-contested findings get a ruling with reasoning; note what each side uniquely caught. Produce one final report: confirmed issues ranked by severity, contested points with both positions and the ruling, and the shared design-level assessment.

## Never let two agents write the same tree

Reviewers that mutation-test (apply a change, run the suite, revert) are writing to the working tree, even though they restore afterwards. If an implementer is editing the same files at the same time, a restore silently overwrites its edits and leaves a hybrid that does not compile — and neither agent knows why. Serialize: an implementer runs, then reviewers run, then the implementer resumes. Never both at once on overlapping files. When a reviewer does mutate, require it to save a copy of the version it reviewed so the two states can be diffed if something goes wrong.

## Token discipline

Default to one critic per side plus the debate rounds. Scale up (multiple critics per side, extra lenses) only when the user asks for a thorough/"plenty of agents" review. Don't add rounds after positions stop moving.
