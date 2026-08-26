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
- **Git push bridge** — opt-in single-repository pushes plus portal-approved repositories for long-running T3 services
- **Clipboard image bridge** — paste screenshots from your host into the container via `xclip`/`wl-paste`
- **Managed configuration** — ships default `AGENTS.md` instructions while preserving your customizations
- **Per-project memory** — auto-memory is isolated per repository, not shared across all containers
- **MCP servers** — pre-configured Playwright with headless Chromium
- **Agent-controlled GUI** — a headless Xvfb virtual display with Openbox, window management, screenshots, input, and accessibility-tree tools for Claude and Codex
- **Wrapped sessions** — run the command in a tmux session, inject keystrokes and read the screen from outside with `wrap-type` / `wrap-key` / `wrap-read`
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
# For a long-running T3 service, approve push repositories from that portal
T3CODE_PAIR_ADMIN_PIN=123456 claude-sandbox --t3-managed-push t3code

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

### Agent-controlled GUI

Claude and Codex can operate graphical applications on a headless virtual display. Every sandbox starts Xvfb with the lightweight Openbox window manager and a session D-Bus. Standard X11 tools operate directly on that session, while `gui-tree` exposes controls through AT-SPI:

```bash
# Start any installed graphical application
mkdir -p .claude-sandbox/gui
xterm >.claude-sandbox/gui/xterm.log 2>&1 &
google-chrome --no-sandbox --force-renderer-accessibility https://example.com \
  >.claude-sandbox/gui/chrome.log 2>&1 &

# Discover and manage its windows
wmctrl -lpxG
timeout 10 xdotool search --sync --name 'Google Chrome'
wmctrl -a 'Google Chrome'
scrot -u .claude-sandbox/gui/chrome.png

# Inspect named controls, actions, and bounds when the app supports AT-SPI
gui-tree --application Chrome --depth 8
# Invoke an advertised action using the path printed by the tree
gui-tree --invoke 0/2/1 click

# Close the window when finished
wmctrl -c 'Google Chrome'
```

Store application logs and screenshots under `.claude-sandbox/gui/`. The accessibility tree is preferable to coordinate clicks when an application exposes it; invoke actions immediately after reading a fresh tree because node paths can change. Screenshot → input → screenshot remains the fallback.

### T3 Code admin portal

The admin portal is disabled by default. Set `T3CODE_PAIR_ADMIN_PIN` to a
4–12 digit PIN when starting T3 Code to enable it:

```bash
T3CODE_PAIR_ADMIN_PIN=123456 claude-sandbox t3code
```

The host-side portal uses a distinct port, defaulting to 3774. Open the exact
URL printed at startup and enter the PIN in its sign-in page. It creates
five-minute, single-use pairing links on demand and automatically uses the
running server's instance database. Open a generated link in the current
browser, or copy it from the read-only field to another client such as the
mobile app. Creating or copying the link does not consume it. For another
device, open the admin portal through a hostname or IP address that device can
reach before generating the link; a link created through `localhost` only
works on the host. The PIN stays on the host, is neither generated nor stored
by claude-sandbox, and must be provided again on every launch.

The same host-side server exposes account-level plan-limit usage without
authentication. Append `/api/usage` to the admin URL printed at startup:

```bash
curl http://localhost:3774/api/usage
```

The response reports each provider's data freshness, RFC 3339 UTC `updated_at`
time, and available usage buckets under the fixed `anthropic`, `openai`, and
`ollama` keys. Each bucket contains an integer `used_percent` and an RFC 3339
UTC `resets_at` value, or `null` when the reset time is unknown or not
reported. Missing buckets are omitted rather than reported as 0%.

The host admin service elects one collector across all running sandbox
instances. It refreshes each provider independently at startup and every 30
minutes by running a fixed, tool-less probe in one managed container. Failed
refreshes retain the last good snapshot and retry with bounded backoff; data
becomes `stale` after 40 minutes. The sanitized account-global cache is stored
under `${XDG_STATE_HOME:-$HOME/.local/state}/claude-sandbox/usage/usage-v1.json`,
outside the agent-mounted workspace.

The HTTP request itself reads that cache only and never contacts a provider.
The collector obtains Anthropic limits through the built-in `/usage` view in
an isolated Claude session without sending a model prompt, OpenAI limits from
the Codex account rate-limit interface, and Ollama limits from its usage
endpoint. Neither the cache nor the public response contains account, plan,
model, cost, credit, credential, or raw provider-error data. The endpoint
returns HTTP 503 only when every provider is unknown. Treat the result as
advisory telemetry.

This collector supersedes locally customized status-line or hook-based usage
refreshers. Because those files are user-owned rather than managed by this
repository, remove any old refresh trigger separately after upgrading; a
status line may remain as a display-only consumer.

For a long-running T3 service whose workspace contains several repositories,
enable managed pushes:

```bash
T3CODE_PAIR_ADMIN_PIN=123456 claude-sandbox --t3-managed-push t3code
```

The first `git push` from an unapproved repository is denied and adds a pending
candidate to the portal. The page shows the repository's workspace-relative
path, branch, and `origin`; choose **Approve once**, **Always allow**, or
**Dismiss**. An origin change suspends an existing approval and requires a new
review. Approvals are stored outside the mounted workspace under
`~/.claude-sandbox/projects/`, where the container cannot modify them.

The portal uses plain HTTP. Anyone able to observe the traffic can recover both
the PIN and generated pairing token, and a short PIN can be guessed. Never
expose it to the internet; use it over an encrypted trusted path such as a VPN
or SSH tunnel. `GET /api/usage` is intentionally unauthenticated, so anyone who
can reach the admin port can observe usage percentages and reset schedules.

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

# Print the current screen contents
claude-sandbox wrap-read

# Include the last 200 scrollback lines above the visible screen
claude-sandbox wrap-read --lines 200
```

`wrap-type` types character by character with a random delay between keystrokes (25–120 ms by default, adjustable with `--delay-min-ms` / `--delay-max-ms`). The target container is derived from the current directory, so `wrap-type` and `wrap-key` must be run from the same directory the wrapped session was started in.

The same commands are available inside the container as `wrap-type`, `wrap-key` and `wrap-read` — the host-side commands are thin forwarders to them. This lets an agent running in the sandbox drive a wrapped session too. Inside the container an agent can also start and stop sessions itself:

```bash
# Start a command in a detached wrapped session
wrap opencode

# Interact with it
wrap-type --enter "explain this repo"
wrap-read

# Stop the session
wrap --kill
```

Several sessions can run at once — give each one a name with `--session` and pass the same flag to the other commands to pick a target. When only one session is running the flag can be omitted:

```bash
wrap --session repl python3 -i
wrap-type --session repl --enter "1+2"
wrap-read --session repl

# List running sessions (also available as: claude-sandbox wrap-list)
wrap --list

wrap --kill --session repl
```

A session started with `claude-sandbox --wrap` uses the default name `claude-sandbox`.

## GitHub CLI Proxy

The container includes a sandboxed `gh` proxy that gives Claude safe access to GitHub without exposing your credentials directly. The proxy runs on the host and communicates with the container over a Unix socket.

Each launch gets a private set of host proxy sockets, mounted read-only at `/run/claude-sandbox` inside that container. The launcher waits until every enabled proxy accepts connections and aborts startup if one fails, preventing stale sockets or another session's permissions from being reused. Proxy logs live in private host-side project state under `~/.claude-sandbox/projects/<project>/logs/`.

**Read commands** work against any repository:

| Group | Commands |
|-------|----------|
| `pr` | `list`, `view`, `diff`, `checks` |
| `issue` | `list`, `view` |
| `repo` | `view` |
| `release` | `list`, `view` |
| `run` | `list`, `view`, `watch` |

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

All commands are flag-validated against a strict allowlist. Every request is logged to `~/.claude-sandbox/projects/<project>/logs/gh-proxy.log`.

Run `gh -h` inside the container to see available commands.

## SSH Proxy

The container includes an SSH proxy that gives filtered SSH access without exposing your SSH keys to the container. The proxy runs on the host and communicates with the container over a Unix socket, the same pattern as the GitHub CLI proxy. Your SSH keys never enter the container.

**How it works:** The SSH proxy is opt-in. When a non-empty SSH proxy config exists, `/usr/local/bin/ssh` inside the container forwards SSH invocations through the proxy. The host-side proxy validates each request against a typed rule set and only spawns the real `/usr/bin/ssh` if there's a match. Everything else is denied. SSH flags (like `-L`, `-D`, `-o`) are never accepted from the container.

**Default config** is empty, so the SSH proxy is disabled by default and no SSH proxy process is started. To enable it, create a non-empty config at `~/.claude-sandbox/projects/<project>/ssh-proxy.json`. Once enabled, a convenience symlink is placed at `.claude-sandbox/ssh-proxy.json`.

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

After the SSH proxy is enabled, all proxy requests are logged to `~/.claude-sandbox/projects/<project>/logs/ssh-proxy.log`:

```bash
grep DENIED ~/.claude-sandbox/projects/*/logs/ssh-proxy.log

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
- The worktree, Git directory, and shared Git common directory must remain beneath the approved repository or workspace. The proxy keeps each directory open and runs Git through those pinned handles, so replacing a path or pointing `.git` outside the approved root cannot redirect an authorized request
- The approved `origin` URL is pinned (at launch for `--allow-push` and by portal approval in managed mode). Each push uses it as the push URL of an unguessable, command-scoped remote; validated `origin` fetch mappings are attached so successful pushes refresh matching `refs/remotes/origin/*` tracking refs without persisting the temporary remote
- The final Git process reads immutable snapshots of the audited system, global, local, and per-worktree config, so replacing a config file after validation cannot redirect an approved push
- Host Git writes tracking changes into private refs. The container client then applies those updates with compare-and-swap and without dereferencing symbolic refs, so an agent-controlled nested ref symlink cannot turn tracking updates into host filesystem writes
- The push is refused if repository config could execute host-side code or redirect the push (`credential.*`, `core.sshCommand`, `core.worktree`, `url.*`, `http.*`, `remote.*.pushurl`, `remote.pushDefault`, `branch.*.pushRemote`, …). `origin` fetch mappings outside `refs/remotes/origin/` are also rejected, and `push.autoSetupRemote` is suppressed for the bridged command
- Lazy promisor fetches and recursive submodule pushes are disabled, preventing a push from initiating secondary network requests to repository-controlled remotes
- Credential helpers are reset on the push command line and rebuilt from the host's system/global git config only, so a helper injected into the workspace repo's config is never executed
- Terminal credential prompts are disabled (`GIT_TERMINAL_PROMPT=0`) — pushes that would require interactive auth fail fast instead of hanging

The grant applies to that launch only and is never persisted — start the next session without the flag and pushes are off again. Every request is logged to `~/.claude-sandbox/projects/<project>/logs/git-proxy.log`.

`--allow-push` requires the working directory to be a git repository with an `origin` remote.

T3's `--t3-managed-push` mode instead routes each request from its container
working directory to a canonical repository beneath the mounted workspace.
Paths outside that workspace and repositories not approved in the host-side
admin portal are rejected. Persistent approvals survive service restarts;
one-time approvals are consumed by the next push attempt.

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

The container ships default `AGENTS.md` instructions (skills guidance, commit conventions) at `/etc/AGENTS.md`, plus optional per-harness overlays at `/etc/AGENTS.claude.md`, `/etc/AGENTS.codex.md`, and `/etc/AGENTS.opencode.md` (sourced from `config/AGENTS.md` and `config/AGENTS.<harness>.md`). At startup, each harness gets the shared base with its overlay appended, merged into `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, and `~/.config/opencode/AGENTS.md` respectively. The managed part is the `# Global Instructions` H1 section — it is replaced on every start, and any H1 sections you add below it are preserved. Overlays must therefore contain only `##` sections (no H1), so their content stays inside the managed section; CI enforces this.

Claude MCP server config (`/etc/claude/mcp.json`) is merged into the project's `.mcp.json` — image defaults take precedence for shared server names, project-level config is preserved otherwise.

Managed Codex config is shipped separately at `/etc/codex/config.toml` and merged into `~/.codex/config.toml` inside `# MANAGED START` / `# MANAGED END` markers, preserving user-owned Codex config outside that block. Today that managed block only configures MCP, but it can be extended with other Codex settings later.

Managed opencode config (`/etc/opencode/opencode.json`) is merged into `~/.config/opencode/opencode.json` using the same JSON deep-merge as Claude — image defaults win for shared keys (e.g. `mcp.playwright`), the rest of your opencode config is preserved.

## Per-Project Memory

All containers mount at `/workspace`, which means Claude's auto-memory would normally be shared across every project. The entrypoint symlinks the memory directory into `.claude-sandbox/memory` inside each repository, giving every project its own isolated memory.

## Skills

Install optional skills to `~/.claude/skills/` and `~/.agents/skills/` — between them, all three harnesses discover the skills natively (Claude Code reads the former; Codex and opencode read the latter, and opencode reads the former too). Updates are checked automatically on each launch.

```bash
claude-sandbox install skills
```

| Skill | Description |
|-------|-------------|
| `/rust` | Rust development guidelines and workflow |
| `/git` | Git operations with small, atomic commits and clean history |
| `/github-actions` | GitHub Actions workflow development with official actions preference |
| `/readme` | README writing and maintenance guidelines |
| `/plan-usage` | Check plan-limit headroom and reset timing before routing substantial agent work |
| `/gui` | Run and test GUI applications on the virtual X display |
| `/wrap` | Run and drive interactive terminal programs in a tmux session |
| `/claude` | Delegate work to Anthropic models through the Claude Code CLI |
| `/codex` | Delegate work to OpenAI models through the Codex CLI |
| `/opencode` | Delegate non-code work to open-weight models through opencode |
| `/delegate` | Choose between Anthropic, OpenAI, and open-weight model pools |
| `/consensus-review` | Run a cross-vendor code review and reconcile the findings |
| `/sandbox-environment` | Describe the container environment and safe tool installation |

Invoke skills manually with `/skill-name` inside Claude, `$skill-name` (or the `/skills` picker) inside Codex, and via the `skill` tool in opencode; all three also load skills on their own when a task matches a skill's description.

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
- Headless Xvfb virtual display with Openbox, AT-SPI/`gui-tree`, xdotool, wmctrl, and scrot
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
