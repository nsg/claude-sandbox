---
name: delegate
description: Load before any piece of work big enough to hand off — a multi-step build, a broad sweep, a review worth a second opinion — and whenever the job could fan out to several models or vendors in parallel, the model or vendor pick is contested, needs a judge or a cross-vendor pair, or needs a fallback because the intended model is unavailable.
---

# Model Selection for Delegated Work

Ordered best first, by the four scores summed; ties broken on capability alone.

Scores 0–10, higher is better

| Model | Via | Affordability | Taste | Design | Science | Use |
|---|---|---|---|---|---|---|
| `gpt-6-astra` high reasoning | codex | 4 | 8 | 8 | 10 | the advanced end only: deep bug and fact hunts through a large codebase; hardest science and correctness; first pick for security work; architecture when the Anthropic buckets are pressed; never volume work |
| `fable` | Agent / claude | 0 | 10 | 10 | 9 | architecture and design decisions; judging and arbitration; creativity and taste bottlenecks; the best eye for visual design; never volume work |
| `opus` high effort | Agent / claude | 4 | 8 | 9 | 8 | default code writer when the brief leaves decisions open; code where design/structure/idiom is the hard part; critic for Sol's code |
| `gpt-6.1-sol` high reasoning | codex | 5 | 7 | 7 | 10 | default code writer when the brief is precise; algorithmically hard, correctness-critical code; critic for Opus's code; stands in for Astra when the extra smarts aren't needed |
| `gpt-6.1-sol` medium reasoning | codex | 6 | 6 | 6 | 8 | smart everyday work |
| `gpt-6.1-sol` low reasoning | codex | 8 | 5 | 5 | 6 | quick work that still needs some smarts |
| `sonnet` | Agent / claude | 5 | 6 | 5 | 7 | Anthropic style at low cost; legwork |
| `ollama-cloud/kimi-k3` | opencode | 4 | 4 | 8 | 6 | a design mock-up as an alternative to a Fable or Opus one, when a different viewpoint is wanted — handed to another model to build; a single hard question; expensive on a small pool, never volume work |
| `haiku` | Agent / claude | 9 | 4 | 3 | 5 | simple fast operations at volume: classification, extraction, sweeps |
| `ollama-cloud/glm-5.3-flash` | opencode | 9 | 3 | 2 | 5 | long or high-volume agentic runs over non-code material; image and video input |
| `ollama-cloud/deepseek-v4.1-flash` | opencode | 8 | 2 | 2 | 6 | boilerplate and simple code from a narrow brief, verified before commit; high-volume non-code work at speed |
| `gpt-6-luna` | codex | 9 | 3 | 2 | 3 | simple fast non-code operations |

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
| OpenAI | `sol` high on algorithmically hard, correctness-critical code (parsers, concurrency, numerics); `gpt-6-astra` on the hardest science, security work, or a deep hunt through a huge codebase; the second vendor of a genuinely high-stakes review | ordinary feature code, refactors, glue, UI, tests → `opus`; legwork → `sonnet` / `haiku`; ordinary reviews → Anthropic only, and say it was single-vendor |
| Anthropic (or the Fable bucket) | `fable` for judging, taste and inventive architecture; `opus` where the brief leaves real decisions open, or design and idiom are the hard part | code → `sol`, with a brief precise enough to follow to the letter; architecture → `gpt-6-astra`; legwork → `sol` low / `luna` |
| Ollama | a `kimi-k3` design mock-up, when a viewpoint from outside Fable and Opus is the point | boilerplate → `sol` or `opus` with the rest of the code; bulk non-code work → `sonnet`, `haiku` or `luna` |

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
GPT-6.1-Sol child it launches is separately guarded against `openai`; neither
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

- **Whoever loaded this skill is the router.** The routing is yours: the model reading this — usually Opus, under Codex often Astra or Sol — makes the picks in its own context and needs no row in the table to do it. Do not hand the routing itself to another model.
- **Three subscriptions, spent independently.** All the same shape — a quota refilling on its own clock: an Anthropic plan behind a native `Agent` or the `claude` CLI, an OpenAI one behind a native Codex worker or the `codex` CLI, and an Ollama one behind `opencode`, metered in tokens at each model's own rate — so `kimi-k3` drains it twenty to thirty times faster than the flash models. One running dry says nothing about the other two; the work moves to whichever still has room rather than stopping — and moves early: a pressed pool is saved for the work only its models do best (see **Live Headroom**).
- **Affordability is one currency: headroom.** Not a dollar price — how much of a quota a run really eats, already reconciled across the three: how generous each plan is, and how many tokens the model spends reaching the same finish line. Codex wins on both, which is why `sol` outscores its Anthropic peers. Compare it freely across rows; it is a baseline, and the live numbers — see **Live Headroom** above — say which pool is pressed today. Astra is billed at Fable's per-token rate but thinks in fewer tokens, so it lands at roughly half Fable's spend on the roomier plan — and, on published per-token and per-task prices, four to five times `sol` high on the same one. It draws from the same OpenAI buckets as `sol` and is not reported separately by the usage API; it simply drains them faster, so a heavy Astra run shrinks the headroom every Codex worker sees.
- **Two frontier brains, different flavours.** Fable and Astra are the baseline of tens; both carry the most knowledge unaided and both can hold something large whole — a codebase, a mission, an objective — and see the pattern running through it. They differ in kind. Astra digs: large patterns, complex science, hunting a fact down through a big codebase, holding rigorously to the brief. Fable invents: creative reframing, taste, going off the brief when the brief is wrong. Both architect well, in different styles, and for the biggest decisions ask both and let one judge.
- **Frontier models are for judgment, not throughput.** Use them to arbitrate between competing designs, settle disagreements between other models' reviews, crack problems where taste or depth is the bottleneck, and lay out the overall architecture of a project — then hand the individual parts to `opus` or `sol` to implement. On anything lookup-able those two arrive at the same place by working the tools harder; what they cannot substitute for is the overview. Every mechanical task run on a frontier model is a judging call unavailable later.
- **Fable decides, Astra digs.** Architecture, design decisions and judging go to Fable by default, and move to Astra when the Fable or Anthropic bucket is pressed. Astra's own ground is the advanced end — a deep bug or fact hunt through a large codebase, the hardest science, security — and it is kept for that, because every run costs four to five `sol` runs from the same pool. When a problem of Astra's kind does not need the extra smarts, or the OpenAI pool needs sparing, `sol` at `high` stands in — at `xhigh` when the extra thinking is motivated. Astra itself stays at `high`: `xhigh` burns away its token efficiency, so reserve it for a problem `high` has already failed on.
- **Design is two skills, and the column blends them.** Architectural design — module boundaries, API shape, the structure of a large system — is where Astra matches Fable. Aesthetic design — a beautiful web page, a polished document, anything that has to look good — is Fable first, then Opus; Astra is only about Sonnet level there, and its Design 8 is the blend: read it as 10 for architecture and route anything that must be beautiful elsewhere. `kimi-k3` earns its 8 on the aesthetic side, as an alternative to a Fable or Opus mock-up rather than an upgrade on one: neither is better, but it comes from a different lab and sees differently, so reach for it when the design needs more than one viewpoint. Have it make a mock-up or design suggestions, then feed those to another model to implement and carry forward. It burns a small pool fast, so use it for the mock-up, not the build.
- **Brief the vendors differently.** OpenAI models follow the brief to the letter, so the brief has to be precise: give Astra or Sol the spec, the scope, and the acceptance criteria, and expect exactly that back. Anthropic models treat the brief as a goal and fill in the blanks: give Fable or Opus the problem and only the constraints that actually matter, and expect sound decisions on the rest — and a deviation when they see a better path.
- **Security work goes to Astra first.** Fable's safety measures can quietly downgrade a security request to Opus or even Sonnet, so a Fable security audit may not be a Fable audit. Astra is usually more permissive but not guaranteed — its sandbox escalations pass through Codex's automatic approval reviewer. Route audits and security research to Astra by default, use Fable as the second opinion, and if either balks fall back to the other, never down a tier.
- **Opus and Sol write the code, both at high.** They are the two default implementers; lower effort is for legwork, never for code. Pick on headroom first (see **Live Headroom**), then on the brief. A precise spec with nothing left to decide → `sol`, which follows it exactly and is the stronger hand on parsers, concurrency, numerics, and other correctness-critical work (Science 10). Open questions, decisions left to the implementer, or a brief you are not sure how to write → `opus`, which makes the creative calls well and wins where API shape, module boundaries, or idiomatic fit is the hard part.
- **The other vendor critiques the code.** Code written by Opus is reviewed by `sol` high, and code written by Sol by `opus`: same-vendor models share blind spots, so a cross-vendor pair fails in uncorrelated ways. Opus + Sol is the pair for code and ordinary reviews; Fable + Astra is the pair for the decisions that matter most, since they disagree in useful ways. When the reviewer's pool is pressed, **Live Headroom** decides whether the review stays cross-vendor — and a single-vendor review is reported as one.
- **Open-weight models write simple code only, and none of it is committed unchecked.** They are there to save tokens on the two main accounts: boilerplate, scaffolding to fill in, simple code that takes little thought — pick the model from the table. Anything complex or correctness-critical stays with `opus` or `sol`. The pattern that works: an Anthropic or OpenAI model writes a narrow set of instructions and the scaffold, the open-weight model fills it in, and an Anthropic or OpenAI model checks the result and runs the gates before anything is committed. That check is a far cheaper pass than writing the code was, which is where the saving comes from — and skipping it is how worse code gets committed and read for years.
- **What else the open-weight models are for.** Bulk work on material that is not code: trawling logs, summarising long output, answering research questions, drafting prose. `glm-5.3-flash` for long or high-volume agentic runs and anything with images or video in it, `deepseek-v4.1-flash` when volume and latency dominate, `kimi-k3` for a single hard question or an outside view — sparingly.
- **Fall back sideways, not downwards.** When the intended pick is unavailable, replace it with the nearest model doing the same kind of work: architecture off `fable` goes to `gpt-6-astra`, then `opus`; architecture off Astra goes to `fable`, then `opus`, and Astra's digging goes to `sol` at `high`; implementation moves between `opus` and `sol`, whichever pool has room. Complex code never falls through to an open-weight model — write less of it instead.
