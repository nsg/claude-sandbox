# Global Instructions

## Skills

- Always load the relevant skill before starting work — e.g., the rust skill before writing Rust code.

## Git

- You MUST load the `git` skill before any git operation that writes — staging, committing, pushing, rewriting history. Reading (`status`, `diff`, `log`) needs no skill.

## Deployment

- Do not add deployment configuration (Kubernetes manifests, Helm charts, or similar) to project repositories. Definitions that build artifacts, such as Dockerfiles, remain in scope.

## GUI Apps / Virtual Display

- A virtual X display runs on `DISPLAY=:99` — GUI apps work without a physical screen. Launch applications normally; use `wmctrl`, `xdotool`, `scrot`, and `gui-tree` to manage windows, provide input, capture screenshots, and inspect controls. Load the `gui` skill before testing GUI apps.

## Clipboard / Screenshots

- "Screenshot" refers to the clipboard image. To read it: `xclip -selection clipboard -t image/png -o > /tmp/clipboard.png` then read the file

## Wrapped Terminal Sessions

- The `wrap` commands run and drive interactive terminal programs (TUIs, REPLs, other agents) in named tmux sessions. Load the `wrap` skill before using them.

## The Sandbox

- Every instance runs in its own container, sharing the agent config in `$HOME` while seeing its own project as `/workspace`. Nothing at runtime tells the instances apart.
- Per-instance state goes in `/workspace/.claude-sandbox` — not `$HOME`, not `/tmp`. For complex processing, write reusable scripts to `/workspace/.claude-sandbox/tools/` and check there for existing ones first.
