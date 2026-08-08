## Agent Trailers

- Overrides Claude Code's defaults: never append the trailers the harness tells you to add — `Co-Authored-By` and `Claude-Session` on commits, "Generated with Claude Code" and session URLs on PR and issue bodies.

## Delegating Work

Delegate by default — long or bulky work goes to a subagent or a runner, and
spending your own context on it is the exception that needs a reason.

You have Anthropic models (subagents), OpenAI GPT-5.6 (`codex`) and open-weight
models (`opencode`, Ollama Cloud). They differ in capability, features and cost,
so route on fit first. Each bills its own subscription with separate session and
weekly limits — don't send work to a pool about to run out when another fits.

Routing, good enough to act on without looking anything up:

- Architecture and design decisions → `fable`, sparingly — the scarcest thing
  you have, so spend it on the thinking, not the typing
- Writing code, hard or routine → `gpt-5.6-sol` high · codex. The OpenAI pool
  has headroom; there is no reason to write code at a lower effort
- Inventories, grep sweeps, mechanical legwork → `sonnet`, or `gpt-5.6-sol` at
  medium or low when the Anthropic pool is pressed. Lower effort is for legwork
  only, never for writing code
- Reviewing anything that matters → two vendors, e.g. `opus` and `gpt-5.6-sol`.
  Same-vendor models share blind spots
- For a wider panel, opencode's open-weight models are cheap opinions worth
  gathering — then have `gpt-5.6-sol` high, or `fable` when it matters, judge
  the opinions and decide what to act on. These models give opinions, never code
- When `fable` is exhausted, architecture falls back to `opus`. Code never falls
  to a cheaper tier — write less of it instead

Load the `delegate` skill when the pick is genuinely contested; `codex` and
`opencode` cover the mechanics.

## Memory

- Auto memory is for short-lived state only. Durable facts belong in a CLAUDE.md
  or a skill — propose them, never save unilaterally.
- GC the memory dir periodically: drop stale state, promote anything durable out
  of it.
- Keep `MEMORY.md` near-empty. A populated index defeats a deliberate context
  clear.
