# Global Instructions

## Skills

- Always load relevant skills before starting work (e.g., load rust skill before writing Rust code)

## Git Commits

- Never add Co-Authored-By, Claude-Session, or any other Claude/AI metadata
  trailers to commit messages — subject and body only

## GUI Apps / Virtual Display

- A virtual X display (Xvfb + openbox) runs on `DISPLAY=:99` — GUI apps work
  without a physical screen. Load the `gui` skill before testing GUI apps
  (screenshots via `scrot`, mouse/keyboard via `xdotool`).

## Clipboard / Screenshots

- "Screenshot" refers to the clipboard image. To read it: `xclip -selection clipboard -t image/png -o > /tmp/clipboard.png` then read the file

## Git Push

- The exact commands `git push` and `git push --tags` may be bridged to the
  host and run with the user's credentials. No other arguments or flags are
  allowed — not even global ones like `-C`. Anything else (`git -C x push`,
  `git push origin main`, `git push --force`, …) runs the container's git,
  which has no credentials and will fail. The bridged push always operates on
  the workspace repository, so `-C` is never needed.
- If a push fails with a hint about `--allow-push`, pushing is disabled for
  this session — ask the user to relaunch with `claude-sandbox --allow-push`.

## Bash Commands

- Avoid compound commands with `cd` (e.g., `cd /tmp && cmd`) as they require manual approval. Use absolute paths instead.
- Quote paths with spaces instead of backslash-escaping (e.g., `'/path/my file.txt'` not `/path/my\ file.txt`).
- Avoid long one-liners with subshells, pipes, and command substitution (e.g., `git show $(git log ... | tail | cut):FILE`). Break into separate commands instead.
- For complex processing, write reusable scripts in `/workspace/.claude-sandbox/tools/` instead of one-liners that require manual approval. Check that directory first for existing tools before creating new ones.
