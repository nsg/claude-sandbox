---
name: delegate
description: Load before any piece of work big enough to hand off — a multi-step build, a broad sweep, a review worth a second opinion — and whenever the job could fan out to several models or vendors in parallel, the model or vendor pick is contested, needs a judge or a cross-vendor pair, or needs a fallback because the intended model is unavailable.
---

# Model Selection for Delegated Work

Ordered best first, by the four scores summed; ties broken on capability alone.

Scores 0–10, higher is better

| Model | Via | Affordability | Taste | Design | Science | Use |
|---|---|---|---|---|---|---|
| `gpt-6-astra` high reasoning | codex | 4 | 8 | 8 | 10 | hardest science and correctness; hunting facts through a large codebase; architecture with a rigorous flavour; first pick for security work; never volume work |
| `fable` | Agent / claude | 0 | 10 | 10 | 9 | creativity and taste bottlenecks; judging; architecture with an inventive flavour; second opinion at the top; never volume work |
| `gpt-5.6-sol` high reasoning | codex | 5 | 7 | 5 | 10 | algorithmically hard, correctness-critical code; heavy lifting off the Anthropic budget |
| `gpt-5.6-sol` medium reasoning | codex | 6 | 6 | 5 | 8 | smart everyday work |
| `opus` | Agent / claude | 3 | 6 | 8 | 7 | code where design/structure/idiom is the hard part |
| `gpt-5.6-sol` low reasoning | codex | 8 | 5 | 4 | 6 | quick work that still needs some smarts |
| `sonnet` | Agent / claude | 5 | 6 | 5 | 5 | Anthropic style at low cost; legwork |
| `ollama-cloud/glm-5.2` | opencode | 7 | 4 | 3 | 6 | long agentic runs over non-code material |
| `ollama-cloud/deepseek-v4-pro` | opencode | 6 | 2 | 3 | 7 | math, knowledge, deep reasoning — never code |
| `gpt-5.6-luna` | codex | 9 | 3 | 2 | 3 | simple fast non-code operations |
| `haiku` | Agent / claude | 7 | 4 | 3 | 2 | simple fast operations |
| `ollama-cloud/deepseek-v4-flash` | opencode | 8 | 2 | 2 | 4 | high-volume non-code work at speed |

Runner mechanics: use a native same-vendor subagent when the harness provides
one; otherwise load the `claude`, `codex`, or `opencode` skill for that CLI.

## Live Headroom

Affordability above is a baseline; it cannot say which pool is pressed today, and
the best baseline is the wrong pick when that pool is the one running dry.
**Routing without the live numbers is a guess. Read them first.**

### Check before you spend

Read usage through the `plan-usage` skill **before every delegation to a
metered pool** — any `codex` or `opencode` launch, any Fable or Opus subagent
doing more than a quick lookup, every fan-out, every review round. It is one
local `curl`; there is no delegation too ordinary for it. No routing table,
here or in a CLAUDE.md, overrides what the numbers say.

- **Reuse figures already in the conversation** when they are under about an
  hour old and no heavy job has run on that pool since. Otherwise read again.
- **Say what you saw.** When announcing a delegation, name the pool and its
  relevant bucket in one line ("Codex weekly 34%, resets in 5 days"), so the
  user can veto the pick before the quota is spent.
- If the endpoint is unavailable, select on the baseline, say the live numbers
  were unavailable, and treat the scarcer pools (OpenAI, Fable) as pressed.
  Never query provider APIs or infer headroom from missing data.

### When a pool is pressed

Judge a bucket by pace, not by percentage alone: compare `used_percent` with how
much of its window has elapsed.

- **Roomy** — usage is at or behind the clock (40% used, 60% of the week gone).
  Route on fit from the table.
- **Pressed** — usage is ahead of the clock, or above roughly 60% with more than
  a day to the reset. 71% with three days of a week left is pressed. A model
  bucket (Fable) and the overall bucket both count; the tighter one decides.
- **Nearly dry** — above roughly 90% with the reset not imminent. Nothing goes
  there without the user's say-so.
- A high figure that resets within the hour is not pressed; read the reset.

A pressed pool is **reserved for work where its models are distinctly the best
option**, and everything generic moves to a roomy pool:

| Pressed pool | Still worth spending it on | Moves elsewhere |
|---|---|---|
| OpenAI | `sol` high on algorithmically hard, correctness-critical code (parsers, concurrency, numerics); `gpt-6-astra` on the hardest science, security work, or a fact hunt through a huge codebase; the second vendor of a genuinely high-stakes review | ordinary feature code, refactors, glue, UI, tests → `opus` (or `sonnet` for the routine parts); legwork → `sonnet` / `haiku`; ordinary reviews → Anthropic only, and say it was single-vendor |
| Anthropic (or the Fable bucket) | `fable` for judging, taste and inventive architecture; `opus` where design and idiom are the hard part | code → `sol`; architecture → `gpt-6-astra`; legwork → `sol` low / `luna` |
| Ollama | nothing is unique to it | bulk non-code work → `sonnet`, `haiku` or `luna` |

Ask "would a roomy pool's model do this nearly as well?" If yes, the pressed
pool does not get the job — a feature in an ordinary TypeScript or Python
codebase is generic work, however large. When both big pools are pressed, do
less in parallel, prefer the one whose reset is nearer, and tell the user.

- **Scheduling needs it most.** Queued work spends a quota unwatched, so read the
  resets over the percentages — 95% resetting within the hour is safe to schedule
  behind, 60% with six days left and a heavy job already queued is not.

### Long-running usage guard

Every worker brief for a job expected to remain active for roughly 30 minutes
or longer must carry the operational guard from `plan-usage`; copy it into the
actual brief rather than assuming an external runner can load the local skill.
State the worker's provider key and require a check at startup and about every
30 minutes while work is active. The worker considers only its own provider
and applicable model/tier buckets, predicts whether it can reach the next check
or finish with margin, and checkpoints before pausing, waiting for a nearby
reset, or ending cleanly.

Propagate this requirement through nested delegation, adapting the provider at
each edge. For example, an Opus parent is guarded against `anthropic`, while a
GPT-5.6-Sol child it launches is separately guarded against `openai`; neither
needs to reason about the other's buckets. Include permission to reduce or
defer optional work when necessary, but do not let quota handling silently
change vendors or expand scope. If a runner sandbox cannot reach the endpoint,
keep the sandbox intact and arrange orchestrator-side checks with provider-only
updates through a follow-up channel or shared status file.

## Parallel Fan-Out

The three subscriptions are three independent lanes: concurrent runs on
different vendors cost no more than sequential ones and finish in a fraction of
the wall-clock. When a job splits into pieces that do not consume each other's
output, fan it out — several models, several vendors, side by side — instead of
feeding the pieces through one delegate in turn.

- **Split on independence, then route each piece on fit.** Partition along
  boundaries where outputs don't feed each other — separate modules, separate
  review dimensions, code vs. docs vs. research. A fan-out is several routing
  decisions from the table above, not one model asked several times; it is also
  how a review gets its cross-vendor pair for free.
- **Launch pattern.** Start each runner from the orchestrator's own background
  Bash per its runner skill and let completion notifications drive assembly;
  never chain a fan-out through a subagent that dies before its children finish.
- **Disjoint files can share the checkout.** Workers editing non-overlapping
  paths may all point at the working tree, though shared build artifacts and
  lockfiles can still collide (two concurrent `cargo` runs fight over `target/`).
  When in doubt, isolate.
- **Overlapping files → one git worktree per worker.** Give each worker a
  private, clean checkout: `git worktree add /workspace/.claude-sandbox/worktrees/<task> -b <branch>`,
  then point the runner's cwd at it. Nothing in a worktree can step on another
  worker; the branch is merged back or pushed to the remote when done, and the
  worktree removed (`git worktree remove`) after integration.
- **The main checkout is the merge point, nothing else.** While a fan-out is
  live, no worker — and no orchestrator editing — touches the primary checkout;
  it stays clean so integration stays trivial. Working there alongside the
  workers is how fan-outs end in conflict archaeology.
- **One orchestrator integrates, by rebase.** A single agent owns assembly: as
  branches finish, rebase each onto the current tip, resolve conflicts there,
  and fast-forward the main checkout — no merge commits, so the history reads
  as one clean linear series. Load the `git` skill before this phase; it is all
  write operations.

## Selection Logic

- **Three subscriptions, spent independently.** All the same shape — a quota refilling on its own clock: an Anthropic plan behind a native `Agent` or the `claude` CLI, an OpenAI one behind a native Codex worker or the `codex` CLI, and an Ollama one behind `opencode`, metered in GPU-time rather than tokens. One running dry says nothing about the other two; the work moves to whichever still has room rather than stopping — and moves early: a pressed pool is saved for the work only its models do best (see **Live Headroom**).
- **Affordability is one currency: headroom.** Not a dollar price — how much of a quota a run really eats, already reconciled across the three: how generous each plan is, and how many tokens the model spends reaching the same finish line. Codex wins on both, which is why `sol` outscores its Anthropic peers. Compare it freely across rows; it is a baseline, and the live numbers — see **Live Headroom** above — say which pool is pressed today. Astra is billed near Fable per token but thinks in far fewer of them, so it lands at roughly half Fable's spend on the roomier plan — and two to three times `sol` high on the same one. It draws from the same OpenAI buckets as `sol` and is not reported separately by the usage API; it simply drains them faster, so a heavy Astra run shrinks the headroom every Codex worker sees.
- **Two frontier brains, different flavours.** Fable and Astra are the baseline of tens; both carry the most knowledge unaided and both can hold something large whole — a codebase, a mission, an objective — and see the pattern running through it. They differ in kind. Astra digs: large patterns, complex science, hunting a fact down through a big codebase, holding rigorously to the brief. Fable invents: creative reframing, taste, going off the brief when the brief is wrong. Both architect well, in different styles — pick by whether the job is finding the shape that already exists or inventing one that doesn't, and for the biggest decisions ask both and let one judge.
- **Frontier models are for judgment, not throughput.** Use Fable or Astra to arbitrate between competing designs, settle disagreements between other models' reviews, crack problems where taste or depth is the bottleneck, and lay out the overall architecture of a project — then hand the individual parts to `sol` to implement. On anything lookup-able `sol` arrives at the same place by working the tools harder; what it cannot substitute for is the overview. Every mechanical task run on a frontier model is a judging call unavailable later.
- **Fable vs Astra: headroom first, then flavour.** These are subscriptions, not per-token bills, so unused quota is the cheapest thing there is. Plenty of Fable left → use Fable. Fable pressed and the OpenAI pool roomy → Astra. Nothing to choose between them → Astra, at roughly half the spend, purely because it thinks less. That efficiency holds at `high`; `xhigh` burns it away, so reserve `xhigh` for a problem `high` has already failed on.
- **Design is two skills, and the column blends them.** Architectural design — module boundaries, API shape, the structure of a large system — is where Astra matches Fable. Aesthetic design — a beautiful web page, a polished document, anything that has to look good — is Anthropic's ballpark: Fable first, then Opus; Astra is only about Opus or Sonnet level there. Astra's Design 8 is the blend. Read it as 10 for architecture and route anything that must be beautiful to an Anthropic model.
- **Brief them differently.** Astra follows the brief literally: give it the spec, the scope, and the acceptance criteria. Fable treats the brief as a goal and will deviate when it sees a better path: give it the problem and only the constraints that actually matter, and expect it to reinterpret the rest.
- **Security work goes to Astra first.** Fable's safety measures can quietly downgrade a security request to Opus or even Sonnet, so a Fable security audit may not be a Fable audit. Astra is usually more permissive but not guaranteed — its sandbox escalations pass through Codex's automatic approval reviewer. Route audits and security research to Astra by default, use Fable as the second opinion, and if either balks fall back to the other, never down a tier.
- **Sol-high vs Opus is capability, not just cost.** Sol-high is the stronger implementer for parsers, concurrency, numerics, and other correctness-critical work (Science 10). Opus wins when the hard part is API shape, module boundaries, or idiomatic fit — but it does not have to justify itself to write ordinary code; while the plan has room, that is a fine use of it.
- **The open-weight models do not write code.** Not a quick fix, not a mechanical loop, not the boring half of a refactor. They are cheap because they are worse, and code is the place where worse compounds — it gets committed, and someone reads it for years. Long grinds that write code go to `sol`, or to `opus` when the OpenAI pool is pressed (see **Live Headroom**) — never to an open-weight model.
- **What the open-weight models are for.** Bulk work on material that is not code and whose output nobody commits: trawling logs, summarising long output, answering research questions, drafting prose. `deepseek-v4-pro` when the question needs real reasoning, `glm-5.2` for long agentic runs, `deepseek-v4-flash` when volume and latency dominate.
- **Fall back sideways, not downwards.** When the intended pick is unavailable, replace it with the nearest model doing the same kind of work: architecture off `fable` goes to `gpt-6-astra`, then `opus`; architecture off Astra goes to `fable`, then `opus`; implementation off codex goes to `opus` or `sonnet` through a native Anthropic agent or the `claude` CLI. Code never falls through to an open-weight model — write less of it instead.
- **Cross-vendor diversity has independent value.** Same-vendor models share blind spots; for high-stakes reviews or decisions, get opinions from two vendors so failures are uncorrelated. Opus + Sol is the pair for ordinary reviews; Fable + Astra is the pair for the decisions that matter most, since they disagree in useful ways.
