<div align="center">
  <img src="header.png" alt="Claude Sandbox" width="600">
  <p>Run Claude CLI in a containerized environment using Podman.</p>
</div>

## About

claude-sandbox wraps [Claude CLI](https://docs.anthropic.com/en/docs/claude-cli) and [OpenAI Codex CLI](https://github.com/openai/codex) in a Podman container with a full development toolchain. It mounts your current directory to `/workspace` and your `~/.claude` and `~/.codex` configs into the container, keeping your host system clean while giving each agent access to everything it needs.

The binary handles container image pulls, self-updates, and skill updates automatically.

## Features

- **Sandboxed GitHub CLI** — proxied `gh` access with an audited allowlist of safe commands
- **Clipboard image bridge** — paste screenshots from your host into the container via `xclip`/`wl-paste`
- **Managed configuration** — ships default `AGENTS.md` instructions while preserving your customizations
- **Per-project memory** — auto-memory is isolated per repository, not shared across all containers
- **MCP servers** — pre-configured Playwright with headless Chromium
- **Auto-updates** — binary, skills, and container image updates are checked on every launch
- **Port exposure** — forward ports from the container with `-p`

## Quick Start

Requires [Podman](https://podman.io/getting-started/installation).

Download the binary and place it in your PATH:

```bash
curl -fsSL https://github.com/nsg/claude-sandbox/releases/latest/download/claude-sandbox -o ~/bin/claude-sandbox
chmod +x ~/bin/claude-sandbox
```

Run it:

```bash
claude-sandbox
```

## Usage

```bash
# Run Claude CLI (image is pulled automatically on first run)
claude-sandbox

# Pass a prompt directly
claude-sandbox "explain this code"

# Expose ports from the container
claude-sandbox -p 8080
claude-sandbox -p 8080 -p 3000 -p 5173

# Open an interactive shell
claude-sandbox shell

# Install skills
claude-sandbox install skills

# Run OpenAI Codex CLI instead of Claude
claude-sandbox codex
claude-sandbox codex "explain this code"
claude-sandbox codex exec "fix the failing test"

# Run the t3code web GUI (auto-publishes port 3773 to the host)
claude-sandbox t3code
# Then open http://localhost:3773
```

Use `--` to pass arguments to claude instead of claude-sandbox:

```bash
claude-sandbox -p 8080 -- -p
```

The same top-level flags (`-p`/`--port`, `--quiet`, `--auto-update`, `--host-env`, `--ssh`, `--no-audio`, …) work with the `codex` subcommand. Flags after `codex` are forwarded to the Codex CLI:

```bash
claude-sandbox -p 8080 codex -m gpt-5
```

Symlink the binary as `codex-sandbox` to make Codex the default when no subcommand is given:

```bash
ln -s ~/bin/claude-sandbox ~/bin/codex-sandbox
codex-sandbox             # runs codex
codex-sandbox "fix bug"   # runs: codex "fix bug"
```

### Auto-update

Skip the interactive update prompt and update automatically:

```bash
claude-sandbox --auto-update
```

### Quiet mode

Suppress informational output, only show errors:

```bash
claude-sandbox --quiet
```

This is useful when launching from editors or scripts where stdout noise is unwanted.

### Host environment

Override environment variables for the Podman process itself (not the container). Useful when the calling environment injects unwanted paths, e.g. VS Code snap overriding `XDG_DATA_HOME`:

```bash
claude-sandbox --host-env XDG_DATA_HOME=/home/user/.local/share
```

Pass without a value to unset a variable:

```bash
claude-sandbox --host-env XDG_DATA_HOME
```

---

## GitHub CLI Proxy

The container includes a sandboxed `gh` proxy that gives Claude safe access to GitHub without exposing your credentials directly. The proxy runs on the host and communicates with the container over a Unix socket.

**Read commands** work against any repository:

| Group | Commands |
|-------|----------|
| `pr` | `list`, `view`, `diff`, `checks` |
| `issue` | `list`, `view` |
| `repo` | `view` |
| `release` | `list`, `view` |
| `run` | `list`, `view` |

**Write commands** are restricted to the workspace repository (no `--repo`/`-R` flag):

| Group | Commands |
|-------|----------|
| `pr` | `create`, `comment` |
| `issue` | `create`, `comment`, `close`, `edit` |

**Extension commands** add custom functionality:

| Command | Description |
|---------|-------------|
| `gh ext run-logs <run-id>` | Download workflow run logs as a zip file |
| `gh ext milestone-create <title>` | Create a milestone (supports `--description`, `--due-on`) |
| `gh ext milestone-list` | List milestones (supports `--state open\|closed\|all`) |

All commands are flag-validated against a strict allowlist. Every request is logged to `.claude-sandbox/gh-proxy.log`.

Run `gh -h` inside the container to see available commands.

## Clipboard Image Bridge

Claude Code inside the container can paste images from your host clipboard. The host-side proxy finds the newest screenshot from `~/Pictures/Screenshots/` (must be less than 2 minutes old) and bridges it into the container.

Inside the container, both `xclip` and `wl-paste` are shimmed to transparently use the proxy:

```bash
# These work inside the container as Claude Code expects
xclip -selection clipboard -t image/png -o
wl-paste --type image/png
```

Set `CLIPBOARD_SCREENSHOTS_DIR` on the host to override the default screenshot directory.

## Managed Configuration

The container ships default `AGENTS.md` instructions (skills guidance, commit conventions) at `/etc/AGENTS.md`. At startup, that managed block is merged into both `~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md`, preserving any content you keep outside the `<!-- MANAGED START -->` / `<!-- MANAGED END -->` markers in either file.

Claude MCP server config (`/etc/claude/mcp.json`) is merged into the project's `.mcp.json` — image defaults take precedence for shared server names, project-level config is preserved otherwise.

Managed Codex config is shipped separately at `/etc/codex/config.toml` and merged into `~/.codex/config.toml` inside `# MANAGED START` / `# MANAGED END` markers, preserving user-owned Codex config outside that block. Today that managed block only configures MCP, but it can be extended with other Codex settings later.

## Per-Project Memory

All containers mount at `/workspace`, which means Claude's auto-memory would normally be shared across every project. The entrypoint symlinks the memory directory into `.claude-sandbox/memory` inside each repository, giving every project its own isolated memory.

## Skills

Install optional skills to both `~/.claude/skills/` for Claude Code and `~/.agents/skills/` for Codex. Updates are checked automatically on each launch.

```bash
claude-sandbox install skills
```

| Skill | Description |
|-------|-------------|
| `/rust` | Rust development guidelines and workflow |
| `/git` | Git operations with atomic commits following conventional commit standards |
| `/github-actions` | GitHub Actions workflow development with official actions preference |
| `/readme` | README writing and maintenance guidelines |

Invoke skills manually with `/skill-name` inside Claude. Codex discovers the same skills from `~/.agents/skills/`.

## MCP Servers

### Playwright

[Playwright MCP](https://github.com/anthropics/playwright-mcp) gives Claude and Codex a headless Chromium browser. They can navigate websites, take screenshots, fill forms, and interact with web pages.

Browser sessions are recorded to `.playwright-output/videos/` as `.webm` files at 1280x720.

## What's Included

The container includes:

- Claude CLI
- OpenAI Codex CLI
- [t3code](https://github.com/pingdotgg/t3code) web GUI for coding agents
- Node.js & npm
- Rust (via rustup) + cargo-audit
- Playwright MCP with Chromium and ffmpeg
- Zola
- Starship prompt
- Git, curl, jq, tree, build-essential, patchutils, unzip

## Building Locally

Build the container image:

```bash
podman build \
  --build-arg GIT_USER_NAME="$(git config user.name)" \
  --build-arg GIT_USER_EMAIL="$(git config user.email)" \
  -t localhost/claude:latest .
```

Build and install the binary:

```bash
cd claude-sandbox
cargo build --release
mkdir -p ~/bin
cp target/release/claude-sandbox ~/bin/claude-sandbox
```

## License

MIT — see [LICENSE.md](LICENSE.md) for details.
