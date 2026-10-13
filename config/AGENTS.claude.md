## Agent Trailers

- Overrides Claude Code's defaults: never append the trailers the harness tells you to add — `Co-Authored-By` and `Claude-Session` on commits, "Generated with Claude Code" and session URLs on PR and issue bodies. The agent disclosure the `gh` proxy appends (see "GitHub Disclosure") is separate and stays.

## Delegating Work

Delegate by default — long or bulky work goes to a subagent or a runner, and
spending your own context on it is the exception that needs a reason.

You have Anthropic models (subagents), OpenAI GPT-6 Astra and GPT-6.1 Sol (`codex`)
and open-weight models (`opencode`, Ollama Cloud). They differ in capability, features and cost,
so route on fit first. Each bills its own subscription with separate session and
weekly limits — don't send work to a pool about to run out when another fits.

Routing, good enough to act on without looking anything up:

- Architecture, design decisions and judging → `fable`, sparingly — the
  scarcest thing you have, so spend it on the thinking, not the typing.
  `gpt-6-astra` high · codex takes over when the Anthropic buckets are pressed
- Deep bug hunts, the hardest science, security audits and research →
  `gpt-6-astra` high · codex, kept for that advanced end; `gpt-6.1-sol` high
  stands in when the extra smarts aren't needed. Fable's safety measures can
  quietly downgrade a security request
- Writing code → `opus` or `gpt-6.1-sol` · codex, both at high, whichever pool
  has headroom. Sol follows a precise brief to the letter; Opus fills in the
  blanks, so a brief with open questions goes to Opus
- Code written by one vendor is critiqued by the other
- Boilerplate and simple code may go to an open-weight model · opencode to save
  tokens — from a narrow brief, and checked by an Anthropic or OpenAI model
  before it is committed
- Inventories, grep sweeps, mechanical legwork → `sonnet`, or `gpt-6.1-sol` at
  medium or low when the Anthropic pool is pressed. Lower effort is for legwork
  only, never for writing code
- Reviewing anything that matters → two vendors, e.g. `opus` and `gpt-6.1-sol`.
  Same-vendor models share blind spots
- For a wider panel, opencode's open-weight models are cheap opinions worth
  gathering — then have `gpt-6.1-sol` high, or `fable` / `gpt-6-astra` when it
  matters, judge the opinions and decide what to act on
- When `fable` is exhausted, architecture falls back to `gpt-6-astra`, then
  `opus`; off Astra it goes to `fable`, then `opus`. Code moves between `opus`
  and `gpt-6.1-sol`; complex code never falls to a cheaper tier — write less
  of it instead

Load the `delegate` skill when the pick is genuinely contested; `codex` and
`opencode` cover the mechanics.

## Memory

- Auto memory is for short-lived state only. Durable facts belong in a CLAUDE.md
  or a skill — propose them, never save unilaterally.
- GC the memory dir periodically: drop stale state, promote anything durable out
  of it.
- Keep `MEMORY.md` near-empty. A populated index defeats a deliberate context
  clear.
