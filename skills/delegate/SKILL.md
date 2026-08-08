---
name: delegate
description: Full capability/cost ranking of every delegation target across the three budget pools (Anthropic plan, OpenAI/codex, Ollama/opencode), with the tradeoffs behind each pick. Global CLAUDE.md carries routing good enough for the common case — load this when the choice is genuinely contested: Sol-high vs Opus on a hard implementation, which model judges or arbitrates, pairing reviewers across vendors for uncorrelated blind spots, re-routing a big job because a pool is pressed, or the user asks what should run where.
---

# Model Selection for Delegated Work

Ordered best first, by the four scores summed; ties broken on capability alone.

Scores 0–10, higher is better

| Model | Via | Affordability | Taste | Design | Science | Use |
|---|---|---|---|---|---|---|
| `fable` | Agent | 0 | 9 | 10 | 8 | judging, taste-bottleneck problems, laying out a project's architecture; never volume work |
| `gpt-5.6-sol` high reasoning | codex | 5 | 7 | 5 | 10 | algorithmically hard, correctness-critical code; heavy lifting off the Anthropic budget |
| `gpt-5.6-sol` medium reasoning | codex | 6 | 6 | 5 | 8 | smart everyday work |
| `opus` | Agent | 3 | 6 | 8 | 7 | code where design/structure/idiom is the hard part |
| `gpt-5.6-sol` low reasoning | codex | 8 | 5 | 4 | 6 | quick work that still needs some smarts |
| `sonnet` | Agent | 5 | 6 | 5 | 5 | Anthropic style at low cost; legwork |
| `ollama-cloud/glm-5.2` | opencode | 7 | 4 | 3 | 6 | long agentic runs over non-code material |
| `ollama-cloud/deepseek-v4-pro` | opencode | 6 | 2 | 3 | 7 | math, knowledge, deep reasoning — never code |
| `gpt-5.6-luna` | codex | 9 | 3 | 2 | 3 | simple fast non-code operations |
| `haiku` | Agent | 7 | 4 | 3 | 2 | simple fast operations |
| `ollama-cloud/deepseek-v4-flash` | opencode | 8 | 2 | 2 | 4 | high-volume non-code work at speed |

Runner mechanics: load the `codex` or `opencode` skill.

## Selection Logic

- **Three subscriptions, spent independently.** All the same shape — a quota refilling on its own clock: an Anthropic plan behind `Agent`, an OpenAI one behind codex, an Ollama one behind opencode, metered in GPU-time rather than tokens. One running dry says nothing about the other two; the work moves to whichever still has room rather than stopping.
- **Affordability is one currency: headroom.** Not a dollar price — how much of a quota a run really eats, already reconciled across the three: how generous each plan is, and how many tokens the model spends reaching the same finish line. Codex wins on both, which is why `sol` outscores its Anthropic peers. Compare it freely across rows; it is a baseline, and the live numbers say which pool is pressed today.
- **Fable is for judgment, not throughput.** Use it to arbitrate between competing designs, settle disagreements between other models' reviews, crack problems where taste is the bottleneck, and lay out the overall architecture of a project — then hand the individual parts to gpt-5.6 to implement. Every mechanical task run on Fable is a judging call unavailable later.
- **Fable is the biggest brain; `sol` compensates with tool calls.** Size buys two things: the most knowledge carried unaided, and the reach to hold something large whole — a codebase, a mission, an objective — and see the pattern running through it. On anything lookup-able `sol` arrives at the same place by working the tools harder, on the roomier plan; what it cannot substitute for is the overview. Spend Fable where there is nothing to look up, or the thing is too big to see in pieces.
- **Sol-high vs Opus is capability, not just cost.** Sol-high is the stronger implementer for parsers, concurrency, numerics, and other correctness-critical work (Science 10). Opus wins when the hard part is API shape, module boundaries, or idiomatic fit — but it does not have to justify itself to write ordinary code; while the plan has room, that is a fine use of it.
- **The open-weight models do not write code.** Not a quick fix, not a mechanical loop, not the boring half of a refactor. They are cheap because they are worse, and code is the place where worse compounds — it gets committed, and someone reads it for years. Long grinds that write code go to `sol`, even when codex is the pressed pool.
- **What the open-weight models are for.** Bulk work on material that is not code and whose output nobody commits: trawling logs, summarising long output, answering research questions, drafting prose. `deepseek-v4-pro` when the question needs real reasoning, `glm-5.2` for long agentic runs, `deepseek-v4-flash` when volume and latency dominate.
- **Fall back sideways, not downwards.** When the intended pick is unavailable, replace it with the nearest model doing the same kind of work: architecture off `fable` goes to `opus`, not to codex; implementation off codex goes to `opus` or `sonnet`. Code never falls through to an open-weight model — write less of it instead.
- **Cross-vendor diversity has independent value.** Same-vendor models share blind spots; for high-stakes reviews or decisions, get opinions from two vendors (e.g. Opus + Sol) so failures are uncorrelated.
