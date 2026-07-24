---
name: wrap
description: Run and drive interactive terminal programs (TUIs, REPLs, other agents) in detached tmux sessions using the wrap commands
---

# Wrapped Terminal Sessions

The wrap commands manage named detached tmux sessions for driving
interactive terminal programs the Bash tool can't handle directly.

## Commands

- `wrap [--session <name>] <command>` — start the program in a detached
  tmux session (default name: `claude-sandbox`)
- `wrap --list` — list running sessions
- `wrap-type [--session <name>] [--enter] <text>` — type text
- `wrap-key [--session <name>] <key>` — send one tmux key name (Enter,
  Escape, BSpace, C-c, ...)
- `wrap-read [--session <name>] [--lines N]` — print the screen;
  `--lines N` includes the last N scrollback lines above the visible screen
- `wrap --kill [--session <name>]` — stop a session

## Core loop: act → read → verify

After every `wrap-type` or `wrap-key`, run `wrap-read` to see the result
before continuing. TUIs redraw asynchronously — never assume input landed.

## Rules

- Several sessions can run at once; give each its own `--session <name>`.
  With exactly one session running, `--session` can be omitted; with
  several it is required.
- A session may already be in use: if the sandbox was started with
  `--wrap`, the `claude-sandbox` session is the user's own terminal. If a
  session exists that you did not start, do not type into or kill it
  without being asked.
- `wrap-type` sends literal text; use `wrap-key` for control keys and
  `--enter` (or `wrap-key Enter`) to submit.
