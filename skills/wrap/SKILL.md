---
name: wrap
description: Run and drive interactive terminal programs (TUIs, REPLs, other agents) in a detached tmux session using the wrap commands
---

# Wrapped Terminal Sessions

The wrap commands manage a single detached tmux session for driving
interactive terminal programs the Bash tool can't handle directly.

## Commands

- `wrap <command>` — start the program in a detached tmux session
- `wrap-type [--enter] <text>` — type text with a human-like cadence
  (25–120 ms per keystroke; tune with `--delay-min-ms` / `--delay-max-ms`)
- `wrap-key <key>` — send one tmux key name (Enter, Escape, BSpace, C-c, ...)
- `wrap-read [--lines N]` — print the screen; `--lines N` includes the last
  N scrollback lines above the visible screen
- `wrap --kill` — stop the session

## Core loop: act → read → verify

After every `wrap-type` or `wrap-key`, run `wrap-read` to see the result
before continuing. TUIs redraw asynchronously — never assume input landed.

## Rules

- Only one wrapped session exists at a time.
- The session may already be in use: if the sandbox was started with
  `--wrap`, it is the user's own terminal. If `wrap` reports a session
  already running and you did not start it, do not type into or kill it
  without being asked.
- `wrap-type` sends literal text; use `wrap-key` for control keys and
  `--enter` (or `wrap-key Enter`) to submit.
