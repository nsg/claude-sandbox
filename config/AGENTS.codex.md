## Delegating Work

Delegate by default — long or bulky work goes to a native subagent or an
external runner, and spending the main session's context on it is the exception
that needs a reason.

You have OpenAI GPT-6 Astra and GPT-5.6 natively, Anthropic models through the
`claude` CLI,
and open-weight models through `opencode` (Ollama Cloud). They differ in
capability, features, and cost, so route on fit first. Each bills its own
subscription with separate session and weekly limits — don't send work to a
pool about to run out when another fits.

Routing, good enough to act on without looking anything up:

- Architecture and design decisions → `gpt-6-astra` high natively or `fable`
  · claude, sparingly — the scarcest things you have, so spend them on the
  thinking, not the typing. Astra digs, Fable invents; between them, whichever
  pool has headroom, and with nothing to choose, Astra at roughly half the spend
- Security audits and research → `gpt-6-astra` first; Fable's safety measures
  can quietly downgrade the request
- Writing code, hard or routine → `gpt-5.6-sol` high, natively. Lower effort is
  for legwork only, never for writing code
- Inventories, grep sweeps, mechanical legwork → a native `gpt-5.6-sol` worker
  at medium or low, or `sonnet` · claude when the OpenAI pool is pressed
- Reviewing anything that matters → two vendors, e.g. native
  `gpt-5.6-sol` and `opus` · claude. Same-vendor models share blind spots
- For a wider panel, opencode's open-weight models are cheap opinions worth
  gathering — then have `gpt-5.6-sol` high, or `gpt-6-astra` / `fable` · claude
  when it matters, judge the opinions and decide what to act on. These models
  give opinions, never code
- When `fable` is exhausted, architecture falls back to `gpt-6-astra`, then
  `opus`; off Astra it goes to `fable`, then `opus`. When the OpenAI pool is
  exhausted, code falls sideways to `opus` or `sonnet`, never down to an
  open-weight model

Load the `delegate` skill when the pick is genuinely contested; `claude` and
`opencode` cover the external runner mechanics. Load `codex` only when a
separate Codex CLI process is preferable to a native worker.
