<div align="center">
  <img src="header.png" alt="Claude Sandbox" width="600">
  <p>Run Claude CLI in a containerized environment using Podman.</p>
</div>

## About

claude-sandbox wraps [Claude CLI](https://docs.anthropic.com/en/docs/claude-cli) in a Podman container with a full development toolchain. It mounts your current directory to `/workspace` and your `~/.claude` config into the container, keeping your host system clean while giving Claude access to everything it needs.

The binary handles container image pulls, self-updates, and skill updates automatically.

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
```

Use `--` to pass arguments to claude instead of claude-sandbox:

```bash
claude-sandbox -p 8080 -- -p
```

## Skills

Install optional Claude Code skills to `~/.claude/skills/`. Updates are checked automatically on each launch.

```bash
claude-sandbox install skills
```

| Skill | Description |
|-------|-------------|
| `/rust` | Rust development guidelines and workflow |
| `/git` | Git operations with atomic commits following conventional commit standards |
| `/github-actions` | GitHub Actions workflow development with official actions preference |
| `/readme` | README writing and maintenance guidelines |

Invoke skills manually with `/skill-name` inside Claude.

## What's Included

The container includes:

- Claude CLI
- Node.js & npm
- Rust (via rustup) + cargo-audit
- Zola
- Starship prompt
- Git, curl, jq, tree, build-essential, patchutils

## Building Locally

Build the container image:

```bash
make build
```

Build and install the binary:

```bash
make install
```

## License

MIT — see [LICENSE.md](LICENSE.md) for details.
