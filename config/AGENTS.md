# Global Instructions

## Skills

- Always load the relevant skill before starting work — e.g., the rust skill before writing Rust code.

## Git

- You MUST load the `git` skill before any git operation that writes — staging, committing, pushing, rewriting history. Reading (`status`, `diff`, `log`) needs no skill.

## GUI Apps / Virtual Display

- A virtual X display runs on `DISPLAY=:99` — GUI apps work without a physical screen. Load the `gui` skill before testing GUI apps.

## Clipboard / Screenshots

- "Screenshot" refers to the clipboard image. To read it: `xclip -selection clipboard -t image/png -o > /tmp/clipboard.png` then read the file

## Wrapped Terminal Sessions

- The `wrap` commands run and drive interactive terminal programs (TUIs, REPLs, other agents) in named tmux sessions. Load the `wrap` skill before using them.

## Bash Commands

- For complex processing, write reusable scripts in `/workspace/.claude-sandbox/tools/` — check there for existing tools first.
