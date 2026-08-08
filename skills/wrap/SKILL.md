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

## Driving the user's own Claude session

The `claude-sandbox` session is the user's terminal. When you have been asked
to drive it:

- Submit with `wrap-type --enter '<text>'` in one call, not a separate
  `wrap-key Enter`. A trailing Enter can be swallowed by the slash-command
  autocomplete menu, which consumes the first press to accept its highlighted
  suggestion.
- Close whatever you opened. A panel or dialog left up blocks the user's input
  box until they dismiss it themselves. `wrap-key Escape`, then `wrap-read` to
  confirm the prompt is back before ending the turn.
- Escape is also Claude Code's interrupt key — it is safe only while a dialog
  holds focus.
- Only type into an empty input box, and remember the box is not the whole
  story: text composed in Remote Control (the mobile app) never appears in the
  pane, so an empty box does not prove the user is idle.
