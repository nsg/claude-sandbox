---
name: opencode
description: Delegate work to open-weight models via the opencode CLI on the Ollama cloud subscription — invocation, model/variant selection, permission model, availability caveats
---

# opencode Delegation

Run open-weight models non-interactively through `opencode run`, billed to the Ollama subscription (separate from both the Anthropic plan and the OpenAI/codex one). Which model to pick is ranked in the `delegate` skill; this skill is the how; behavior below was verified live on opencode 1.18.15.

These models do not write code — send them bulk non-code work (logs, summaries, research, prose) and route anything that ends up committed to codex or an Anthropic model instead.

## Invocation

```bash
opencode run -m ollama-cloud/<model> "prompt"           # e.g. ollama-cloud/glm-5.2
opencode run -m ollama-cloud/deepseek-v4-pro --variant high "prompt"
```

- `-m provider/model` selects the model; `opencode models` lists every installed provider/model pair.
- `--variant high|medium|low` sets provider-specific reasoning effort. CAUTION: unknown variant strings are silently accepted (verified: `--variant bogus` runs without error) — a typo means you silently get the default, so double-check the spelling.
- Long prompts — three working options, in order of preference (all verified on 1.18.15):
  1. Inline the file: `opencode run -m … "$(cat brief.md)"` — simplest, no attachment semantics to get wrong.
  2. Mention it in the message as `@path` (relative to cwd): `opencode run -m … "Follow the brief in @brief.md"` — the agent reads it with its Read tool.
  3. Attach with `-f`, but ONLY as `-f brief.md -- "message"`. `-f` is variadic: without `--` it swallows the following positional message as another file path and fails with `File not found: <your message>`. And `"message" -f brief.md` (file after message) parses without error but the attachment silently never reaches the model — verified: the model reports no file. Attachment failures are silent, so prefer options 1–2.
- `--dir <path>` sets the working directory without cd'ing; `--format json` emits raw event JSON for parsing.
- Long tasks: launch from the orchestrator's own background Bash, same pattern and rationale as the `codex` skill (a child process of a subagent dies with that subagent's turn). Capture stdout to a file; there is no `-o` report flag, the final answer is just the last stdout text.
- Sessions persist: `-c` continues the last one, `-s <id>` a specific one, `--fork` branches it. `opencode stats` shows usage; `opencode export` dumps a session.

## Permission model (verified by probing)

Headless `run` defaults, no flags:

- File writes INSIDE the working directory: allowed without prompting (a Write tool call just runs).
- Access OUTSIDE the working directory (e.g. `cat /etc/hostname`): triggers `permission requested: external_directory` and is AUTO-REJECTED — the agent sees a tool failure and continues.
- `--auto` auto-approves everything not explicitly denied (verified: the same /etc read then succeeds). Dangerous — use only when the task genuinely needs to roam, and prefer pointing `--dir` at the right tree instead.
- Fine-grained rules (allow/ask/deny per tool) can be set in `~/.config/opencode/opencode.json` under `permission`; that file also carries MCP server config. Note "ask" is useless headless — nothing can answer; keep rules to allow/deny.
- So: run it with cwd = the repo it should touch, and its blast radius is naturally that repo.

## Availability caveat — check before relying on a model

Some listed models are NOT in the subscription: they bill "extra usage" and fail at request time ("this model uses extra usage only … balance is empty") if that balance is empty. The listing (`opencode models`) does NOT reveal this — the only test is a cheap ping:

```bash
opencode run -m ollama-cloud/<model> "Reply with exactly: PONG"
```

Ping before committing a real task to a model you haven't used recently, and report unavailability to the owner rather than silently substituting.

## Briefing and verification

Open-weight models are weaker instruction-followers than GPT-5.6-Sol: keep tasks smaller and more mechanical than a codex brief, state hard constraints early and repeat the critical one at the end, and always verify the result yourself (diff checks, gates) — same verification split as the `codex` skill. Project-level `AGENTS.md` in the working directory is honored as standing instructions.

## Capability calibration — high recall, low precision

These models are below the frontier tier (Sol/Opus/Fable) but see from different angles and do real work. Calibration from a live 3-model code review (camon, 2026-08) where every finding was then adversarially triaged by Sol:

- Findings ran ~10% accepted: 19 findings → 2 real fixes; most rejections were pattern-matched risk shapes where the model hadn't read the nearby guard/invariant (e.g. a "subtraction underflow" already capped by the loop above it). A confident, specific-sounding finding is NOT evidence it's real.
- The value is angle diversity and cheap volume, not precision — the cheapest model (deepseek-v4-flash) found both real bugs, and also authored the most convincing false positive; the most disciplined brief-follower (deepseek-v4-pro) found zero.
- Therefore: never act on their findings or merge their patches directly. Route the output through a frontier model (Sol via codex, or Opus) briefed to be critical — free to reject, fix differently, or escalate to the owner — then verify gates yourself. Reviewer → frontier judge → own verification is the proven pipeline.
