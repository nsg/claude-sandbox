<div align="center">
  <img src="header.png" alt="Claude Sandbox" width="600">
  <p>Run Claude CLI in a containerized environment using Podman.</p>
</div>

## About

claude-sandbox wraps [Claude CLI](https://docs.anthropic.com/en/docs/claude-cli), [OpenAI Codex CLI](https://github.com/openai/codex), [opencode](https://opencode.ai), and [t3code](https://github.com/pingdotgg/t3code) in a Podman container with a full development toolchain. It mounts your current directory to `/workspace` and your `~/.claude`, `~/.codex`, and `~/.config/opencode` configs into the container, keeping your host system clean while giving each agent access to everything it needs.

The binary handles container image pulls, self-updates, and skill updates automatically.

## Features

- **Sandboxed GitHub CLI** — proxied `gh` access with an audited allowlist of safe commands
- **SSH proxy** — filtered SSH access without exposing keys to the container
- **Git push bridge** — opt-in `--allow-push` lets the agent trigger `git push` / `git push --tags`, executed on the host with your credentials
- **Clipboard image bridge** — paste screenshots from your host into the container via `xclip`/`wl-paste`
- **Managed configuration** — ships default `AGENTS.md` instructions while preserving your customizations
- **Per-project memory** — auto-memory is isolated per repository, not shared across all containers
- **MCP servers** — pre-configured Playwright with headless Chromium
- **Virtual X display** — headless Xvfb + openbox on `DISPLAY=:99`, so agents can run and test GUI apps (screenshots via `scrot`, input via `xdotool`)
- **Wrapped sessions** — run the command in a tmux session and inject keystrokes from outside with `wrap-type` / `wrap-key`
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

# Allow the agent to git push (executed on the host, see "Git Push Bridge")
claude-sandbox --allow-push

# Open an interactive shell
claude-sandbox shell

# Install skills
claude-sandbox install skills

# Run OpenAI Codex CLI instead of Claude
claude-sandbox codex
claude-sandbox codex "explain this code"
claude-sandbox codex exec "fix the failing test"

# Run the t3code web GUI
claude-sandbox t3code
# Optionally enable its pairing portal with a PIN
T3CODE_PAIR_ADMIN_PIN=123456 claude-sandbox t3code

# Run opencode TUI
claude-sandbox opencode
claude-sandbox opencode "explain this code"
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

### T3 Code pairing portal

The pairing portal is disabled by default. Set `T3CODE_PAIR_ADMIN_PIN` to a
4–12 digit PIN when starting T3 Code to enable it:

```bash
T3CODE_PAIR_ADMIN_PIN=123456 claude-sandbox t3code
```

The portal uses a distinct port, defaulting to 3774. Open the exact URL printed
at startup and enter the PIN in its sign-in page. It creates five-minute,
single-use pairing links on demand and automatically uses the running server's
instance database. The PIN is neither generated nor stored by claude-sandbox;
provide it again on every launch.

The portal uses plain HTTP. Anyone able to observe the traffic can recover both
the PIN and generated pairing token, and a short PIN can be guessed. Never
expose it to the internet; use it over an encrypted trusted path such as a VPN
or SSH tunnel.

### Wrapped sessions

Pass `--wrap` to run the command inside a named tmux session in the container, so keystrokes can be injected from another terminal:

```bash
claude-sandbox --wrap shell
```

Then, from a second terminal in the same project directory:

```bash
# Type text with a human-like typing cadence, then press Enter
claude-sandbox wrap-type --enter "ls -la"

# Send a single tmux key name (Enter, Escape, BSpace, C-c, ...)
claude-sandbox wrap-key C-c
```

`wrap-type` types character by character with a random delay between keystrokes (25–120 ms by default, adjustable with `--delay-min-ms` / `--delay-max-ms`). The target container is derived from the current directory, so `wrap-type` and `wrap-key` must be run from the same directory the wrapped session was started in.

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
| `run` | `rerun` |

**Extension commands** add custom functionality:

| Command | Description |
|---------|-------------|
| `gh ext run-logs <run-id>` | Download workflow run logs as a zip file |
| `gh ext milestone-create <title>` | Create a milestone (supports `--description`, `--due-on`) |
| `gh ext milestone-list` | List milestones (supports `--state open\|closed\|all`) |

All commands are flag-validated against a strict allowlist. Every request is logged to `.claude-sandbox/gh-proxy.log`.

Run `gh -h` inside the container to see available commands.

## SSH Proxy

The container includes an SSH proxy that gives filtered SSH access without exposing your SSH keys to the container. The proxy runs on the host and communicates with the container over a Unix socket, the same pattern as the GitHub CLI proxy. Your SSH keys never enter the container.

**How it works:** The SSH proxy is opt-in. When a non-empty SSH proxy config exists, `/usr/local/bin/ssh` inside the container forwards SSH invocations through the proxy. The host-side proxy validates each request against a typed rule set and only spawns the real `/usr/bin/ssh` if there's a match. Everything else is denied. SSH flags (like `-L`, `-D`, `-o`) are never accepted from the container.

**Default config** is empty, so the SSH proxy is disabled by default and no SSH proxy process is started. To enable it, create a non-empty config at `~/.config/claude-sandbox/projects/<project>/ssh-proxy.json`. Once enabled, a convenience symlink is placed at `.claude-sandbox/ssh-proxy.json`.

The config has three rule types:

```json
{
  "git": [
    "github.com",
    "github.com/myorg/*"
  ],
  "command": [
    "deploy@prod.example.com uptime"
  ],
  "host": [
    "admin@staging.internal"
  ]
}
```

### `git` — allow git operations to a host

Each entry is a hostname. The proxy structurally validates that the SSH invocation matches the exact shape git uses (`git-receive-pack`, `git-upload-pack`, `git-upload-archive`). Only `git@<host>` destinations are accepted.

- `github.com` — all repos on GitHub
- `github.com/myorg/*` — only repos under that org
- `github.com/myorg/specific-repo` — only that repo
- `*.gitlab.com` — any GitLab subdomain

### `command` — allow a specific command on a host

Each entry is an exact `user@host command` string. No wildcards. The full invocation must match exactly.

- `deploy@prod.example.com uptime`
- `deploy@prod.example.com sudo systemctl restart myapp`

Remote commands with dash-prefixed arguments must be passed as a single quoted string: `ssh deploy@host "ls -la /tmp"`, not `ssh deploy@host ls -la /tmp`. The proxy rejects any argument starting with `-` to prevent SSH flag injection.

### `host` — allow any command on a host

Each entry is a `user@host` destination. Any remote command is allowed (but a command is always required — interactive shells are denied). This is the broadest permission — prefer `command` rules when you know the specific commands needed.

- `admin@staging.internal`

### Discovering what to allow

After the SSH proxy is enabled, all proxy requests are logged to `.claude-sandbox/ssh-proxy.log`:

```bash
grep DENIED .claude-sandbox/ssh-proxy.log

# 2026-04-26T12:00:01Z DENIED  git@gitlab.com git-receive-pack '/org/repo.git'
# 2026-04-26T12:05:30Z DENIED  deploy@prod.example.com uptime
```

Use the denied command line to determine which rule type and entry to add. If the proxy is disabled because the config is empty or missing, no deny log is written. The proxy must be restarted for config changes to take effect (restart the container).

## Git Push Bridge

The container has no git credentials, so pushes fail by default. Launch with `--allow-push` to let the agent trigger a push that is executed **on the host** with your credentials:

```bash
claude-sandbox --allow-push
```

Only two exact commands are bridged, with no arguments accepted from the container:

- `git push`
- `git push --tags`

The container's `git` is a thin shim that forwards those two invocations to the host proxy and `exec`s the real `/usr/bin/git` for everything else — rebases, `git push --force`, `git push origin main`, and all other git commands behave exactly as normal (a force push simply fails inside the container, since it has no credentials).

The workspace is agent-writable, so the host-side proxy treats the repository as untrusted when pushing:

- Hooks are disabled (`core.hooksPath=/dev/null`, `--no-verify`), so a planted `.git/hooks/pre-push` never runs on the host
- The `origin` URL is snapshotted at launch; the push is refused if `origin` has been repointed since, and the push always targets `origin` explicitly (`remote.pushDefault` / `branch.*.pushRemote` are ignored)
- The push is refused if the repo's local config sets keys the host would execute or that would redirect the push (`credential.*`, `core.sshCommand`, `core.fsmonitor`, `url.*`, `http.*`, `remote.origin.pushurl`, …)
- Credential helpers are reset on the push command line and rebuilt from the host's system/global git config only, so a helper injected into the workspace repo's config is never executed
- Terminal credential prompts are disabled (`GIT_TERMINAL_PROMPT=0`) — pushes that would require interactive auth fail fast instead of hanging

The grant applies to that launch only and is never persisted — start the next session without the flag and pushes are off again. Every request is logged to `.claude-sandbox/git-proxy.log`.

`--allow-push` requires the working directory to be a git repository with an `origin` remote.

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

The container ships default `AGENTS.md` instructions (skills guidance, commit conventions) at `/etc/AGENTS.md`. At startup, that managed block is merged into `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, and `~/.config/opencode/AGENTS.md`, preserving any content you keep outside the `<!-- MANAGED START -->` / `<!-- MANAGED END -->` markers in each file.

Claude MCP server config (`/etc/claude/mcp.json`) is merged into the project's `.mcp.json` — image defaults take precedence for shared server names, project-level config is preserved otherwise.

Managed Codex config is shipped separately at `/etc/codex/config.toml` and merged into `~/.codex/config.toml` inside `# MANAGED START` / `# MANAGED END` markers, preserving user-owned Codex config outside that block. Today that managed block only configures MCP, but it can be extended with other Codex settings later.

Managed opencode config (`/etc/opencode/opencode.json`) is merged into `~/.config/opencode/opencode.json` using the same JSON deep-merge as Claude — image defaults win for shared keys (e.g. `mcp.playwright`), the rest of your opencode config is preserved.

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
- [opencode](https://opencode.ai) TUI coding agent
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
