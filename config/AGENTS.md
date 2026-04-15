# Global Instructions

## Skills

- Always load relevant skills before starting work (e.g., load rust skill before writing Rust code)

## Git Commits

- Never add Co-Authored-By lines to commit messages

## Clipboard / Screenshots

- "Screenshot" refers to the clipboard image. To read it: `xclip -selection clipboard -t image/png -o > /tmp/clipboard.png` then read the file

## Bash Commands

- Avoid compound commands with `cd` (e.g., `cd /tmp && cmd`) as they require manual approval. Use absolute paths instead.
- Quote paths with spaces instead of backslash-escaping (e.g., `'/path/my file.txt'` not `/path/my\ file.txt`).
- Avoid long one-liners with subshells, pipes, and command substitution (e.g., `git show $(git log ... | tail | cut):FILE`). Break into separate commands instead.
- For complex processing, write reusable scripts in `/workspace/.claude-sandbox/tools/` instead of one-liners that require manual approval. Check that directory first for existing tools before creating new ones.
