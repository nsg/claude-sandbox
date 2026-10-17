<div align="center">
  <img src="header.png" alt="Claude Sandbox" width="600">
  <p>Run Claude CLI in a containerized environment using Podman.</p>
</div>

## About

claude-sandbox wraps [Claude CLI](https://docs.anthropic.com/en/docs/claude-cli), [OpenAI Codex CLI](https://github.com/openai/codex), [opencode](https://opencode.ai), and [t3code](https://github.com/pingdotgg/t3code) in a Podman container with a full development toolchain. It mounts your current directory to `/workspace` and your `~/.claude`, `~/.codex`, and `~/.config/opencode` configs into the container, keeping your host system clean while giving each agent access to everything it needs.

The binary handles container image pulls, self-updates, and skill updates automatically.

## Features

- **[Sandboxed GitHub CLI](https://nsg.github.io/claude-sandbox/github.html)** — proxied `gh` access with an audited command allowlist and launch-snapshotted write targets
- **[SSH proxy](https://nsg.github.io/claude-sandbox/ssh.html)** — filtered SSH access without exposing keys to the container
- **[Managed fetch approvals](https://nsg.github.io/claude-sandbox/ssh.html#managed-fetch-approvals)** — portal-approved, read-only access to exact private SSH repositories
- **[Git push bridge](https://nsg.github.io/claude-sandbox/git-push.html)** — opt-in single-repository pushes plus portal-approved repositories for long-running T3 services
- **[Virtual machine bridge](https://nsg.github.io/claude-sandbox/vm.html)** — opt-in creation and control of ephemeral Incus VMs without exposing host credentials or devices
- **[Clipboard image bridge](https://nsg.github.io/claude-sandbox/clipboard.html)** — paste screenshots from your host into the container via `xclip`/`wl-paste`
- **[Managed configuration](https://nsg.github.io/claude-sandbox/environment.html#managed-configuration)** — ships default `AGENTS.md` instructions while preserving your customizations
- **[Per-project memory](https://nsg.github.io/claude-sandbox/environment.html#per-project-memory)** — auto-memory is isolated per repository, not shared across all containers
- **[MCP servers](https://nsg.github.io/claude-sandbox/environment.html#playwright-mcp)** — pre-configured Playwright with headless Chromium
- **Image builds** — build OCI images from a Containerfile or Dockerfile with Buildah, without a daemon or extra privileges
- **[Agent-controlled GUI](https://nsg.github.io/claude-sandbox/gui.html)** — a headless Xvfb virtual display with Openbox, window management, screenshots, input, and accessibility-tree tools for Claude and Codex
- **[Live display](https://nsg.github.io/claude-sandbox/t3code.html#live-display)** — view and control the sandbox's virtual X display from the T3 admin portal through the existing admin port, no extra ports
- **[GNOME usage indicator](https://nsg.github.io/claude-sandbox/plan-usage.html#gnome-usage-indicator)** — optional top-bar extension showing plan-limit usage for every provider
- **[Wrapped sessions](https://nsg.github.io/claude-sandbox/wrap.html)** — run the command in a tmux session, inject keystrokes and read the screen from outside with `wrap-type` / `wrap-key` / `wrap-read`
- **[Auto-updates](https://nsg.github.io/claude-sandbox/usage.html#auto-update)** — binary, skills, and container image updates are checked on every launch
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

## Documentation

Full documentation at [nsg.github.io/claude-sandbox](https://nsg.github.io/claude-sandbox/):

- [Command-line usage](https://nsg.github.io/claude-sandbox/usage.html) — subcommands, flags, Codex and opencode, auto-update
- [T3 Code](https://nsg.github.io/claude-sandbox/t3code.html) — web GUI, admin portal, live display, managed approvals
- [Running as a systemd service](https://nsg.github.io/claude-sandbox/service.html)
- [Host bridges](https://nsg.github.io/claude-sandbox/github.html) — GitHub CLI, SSH, Git push, virtual machines, clipboard
- [Skills](https://nsg.github.io/claude-sandbox/skills.html) and the [container environment](https://nsg.github.io/claude-sandbox/environment.html)
- [Building locally](https://nsg.github.io/claude-sandbox/building.html)

The site is built from [`docs/`](docs/) — plain HTML and CSS, published by GitHub Actions.

## License

MIT — see [LICENSE.md](LICENSE.md) for details.
