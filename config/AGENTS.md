# Global Instructions

## Skills

- Always load the relevant skill before starting work — e.g., the rust skill before writing Rust code.

## Git Commits

- Keep commit messages short and focused on the change at hand. The subject line alone should give a good understanding of what happened; add a body only when there is genuinely more to explain.
- Describe the problem and the fix, not the journey — no troubleshooting narratives, discoveries along the way, or other session context.
- Write timelessly: the message should still make sense in 100 years. Never reference CI runs, URLs to ephemeral systems, or anything outside the repository.
- Do not use conventional-commit prefixes like `fix(docs):` — plain imperative sentences.
- Do not restate the diff; a reader who wants details will open it.
- Never add Co-Authored-By, "Generated with", or any other AI/agent metadata trailers to commit messages unless explicitly asked — subject and body only.

## GUI Apps / Virtual Display

- A virtual X display runs on `DISPLAY=:99` — GUI apps work without a physical screen. Load the `gui` skill before testing GUI apps.

## Clipboard / Screenshots

- "Screenshot" refers to the clipboard image. To read it: `xclip -selection clipboard -t image/png -o > /tmp/clipboard.png` then read the file

## Git Push

- Only the exact commands `git push` and `git push --tags` are bridged to the host with the user's credentials. Any other form (extra args or flags, even `-C`) runs the container's credential-less git and fails. The bridged push always targets the workspace repository.
- If a push fails with a hint about `--allow-push`, pushing is disabled for this session — ask the user to relaunch with `claude-sandbox --allow-push`.

## Wrapped Terminal Sessions

- The `wrap` commands run and drive interactive terminal programs (TUIs, REPLs, other agents) in named tmux sessions. Load the `wrap` skill before using them.

## Bash Commands

- For complex processing, write reusable scripts in `/workspace/.claude-sandbox/tools/` — check there for existing tools first.
