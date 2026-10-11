## Delegating Work

Delegate by default — long or bulky work goes to a native subagent or an
external runner, and spending the main session's context on it is the exception
that needs a reason.

You have OpenAI GPT-6 Astra and GPT-6.1 Sol natively, Anthropic models through the
`claude` CLI,
and open-weight models through `opencode` (Ollama Cloud). They differ in
capability, features, and cost, so route on fit first. Each bills its own
subscription with separate session and weekly limits — don't send work to a
pool about to run out when another fits.

Routing, good enough to act on without looking anything up:

- Architecture, design decisions and judging → `fable` · claude, sparingly —
  the scarcest thing you have, so spend it on the thinking, not the typing.
  `gpt-6-astra` high natively takes over when the Anthropic buckets are pressed
- Deep bug hunts, the hardest science, security audits and research →
  `gpt-6-astra` high natively, kept for that advanced end; `gpt-6.1-sol` high
  stands in when the extra smarts aren't needed. Fable's safety measures can
  quietly downgrade a security request
- Writing code → native `gpt-6.1-sol` or `opus` · claude, both at high,
  whichever pool has headroom. Sol follows a precise brief to the letter; Opus
  fills in the blanks, so a brief with open questions goes to Opus. Lower
  effort is for legwork only, never for writing code
- Code written by one vendor is critiqued by the other
- Boilerplate and simple code may go to an open-weight model · opencode to save
  tokens — from a narrow brief, and checked by an OpenAI or Anthropic model
  before it is committed
- Inventories, grep sweeps, mechanical legwork → a native `gpt-6.1-sol` worker
  at medium or low, or `sonnet` · claude when the OpenAI pool is pressed
- Reviewing anything that matters → two vendors, e.g. native
  `gpt-6.1-sol` and `opus` · claude. Same-vendor models share blind spots
- For a wider panel, opencode's open-weight models are cheap opinions worth
  gathering — then have `gpt-6.1-sol` high, or `gpt-6-astra` / `fable` · claude
  when it matters, judge the opinions and decide what to act on
- When `fable` is exhausted, architecture falls back to `gpt-6-astra`, then
  `opus`; off Astra it goes to `fable`, then `opus`. Code moves between
  `gpt-6.1-sol` and `opus`; complex code never falls down to an open-weight
  model

Load the `delegate` skill when the pick is genuinely contested; `claude` and
`opencode` cover the external runner mechanics. Load `codex` only when a
separate Codex CLI process is preferable to a native worker.
