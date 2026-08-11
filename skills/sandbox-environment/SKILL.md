---
name: sandbox-environment
description: Short description of the claude-sandbox runtime and its components. Use when an agent needs context about the container, workspace and persistent state, installed development environment, safe container-level software installation, host bridges, managed configuration, or how to discover relevant specialist skills.
---

# Sandbox Environment

Use this skill as an environment map. Treat versions, permissions, available
models, open ports, and active services as session-dependent facts to verify
only when they matter.

## Container and filesystem

- Assume an Ubuntu-based Podman container with a full development toolchain.
- Treat `/workspace` as the mounted workspace root. It may contain one project
  directly, or it may contain multiple projects or workspaces as subfolders.
- Determine the active project from the current working directory and project
  markers; do not assume that `/workspace` is the Git or project root.
- Keep per-instance state and reusable scratch tools under
  `/workspace/.claude-sandbox`.
- Treat `$HOME` as shared agent configuration, not instance-local storage, and
  use `/tmp` only for disposable files.

## Agent and development tools

- Assume agents run as `root` inside the container, so system tools can be
  installed without `sudo` when they are genuinely needed for the task.
- Expect the image to include a broad selection of agent, development, build,
  browser, media, and shell tools. Inspect the current environment instead of
  relying on a fixed inventory or version list in this skill.

## Installing software

- Prefer an existing tool before installing another one, and keep additions to
  the minimum needed for the task.
- Expect container-level installations to last only for the current container.
  The user controls the image used for future containers.
- If a missing tool is likely to be useful in future sessions, tell the user
  and suggest including it in the image; do not change the image definition
  unless the user asks.
- Treat project-declared dependencies installed inside the repository as part
  of the project workflow.
- For container-level software, prefer packages from the official Ubuntu
  repositories.
- Use another source without asking only when the software and publisher are
  well-known, well-maintained, and can be confidently vetted from an official
  first-party distribution channel.
- Never trust an unfamiliar package, third-party repository, downloaded binary,
  or remote install script merely because it is convenient. If the project,
  author, publisher, or delivery chain cannot be confidently vetted, ask the
  user for permission before installing or executing it.
- Treat supply-chain security as more important than avoiding a clarification.

## Host-connected components

- Treat GitHub CLI, Git push, SSH, and clipboard access as narrow host bridges,
  not as direct access to host credentials.
- Expect bridge availability and allowed operations to depend on how the
  sandbox was launched and configured.
- Interpret “screenshot” as the host clipboard image; `xclip` and `wl-paste`
  are proxy clients for that bridge.

## Skills

- Inspect the skills available in the current session before operating a
  component or entering a specialized domain.
- Select skills by their descriptions rather than relying on a fixed catalog in
  this file.
- Load and follow the matching skill for detailed workflows, constraints, and
  current component-specific guidance.

## Managed configuration

- Expect the container entrypoint to merge managed defaults into the supported
  agent configurations while preserving user-owned configuration.
- Treat repository instructions and installed skills as the durable place for
  agent guidance; managed sections may be refreshed when the container starts.
- Prefer an available specialist skill over repeating its instructions here.
