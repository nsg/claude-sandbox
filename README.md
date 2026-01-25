# claude-sandbox

Run [Claude CLI](https://docs.anthropic.com/en/docs/claude-cli) in a containerized environment using Podman.

This is mainly written for my own needs, but I try to keep it somewhat generic so it may be useful for others.

## Installation

Download the `claude-sandbox` script and place it in your PATH:

```bash
curl -fsSL https://github.com/nsg/claude-sandbox/releases/latest/download/claude-sandbox -o ~/bin/claude-sandbox
chmod +x ~/bin/claude-sandbox
```

## Prerequisites

- [Podman](https://podman.io/getting-started/installation)

## Usage

```bash
# Run Claude CLI (image is pulled automatically on first run)
claude-sandbox

# Run with arguments
claude-sandbox --help
claude-sandbox "explain this code"

# Open an interactive shell
claude-sandbox shell
```

The script mounts your current directory to `/workspace` and your `~/.claude` config directory into the container.

## What's Included

The container includes:

- Claude CLI
- Node.js & npm
- Rust (via rustup)
- Zola
- Git, curl, jq, tree, build-essential

## Building Locally

To build the container image yourself:

```bash
make build
```

## License

See [LICENSE](LICENSE) for details.
