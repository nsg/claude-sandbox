---
name: blender
description: Drive a real Blender 5.3 running in a `vm` virtual machine through the official Blender Lab MCP server (the `mcp__blender__*` tools) — build and edit scenes, run bpy code, inspect .blend files, render, take screenshots, and move files in and out. Load before using any `mcp__blender__*` tool, when those tools are missing or fail to connect, when a project needs the Blender MCP server installed, or whenever a task needs Blender (3D modelling, scene setup, rendering, .blend inspection).
---

# Blender in a VM

Blender runs on the display of an ephemeral VM named `blender`. The
[Blender Lab MCP server](https://www.blender.org/lab/mcp-server/), the Blender
Foundation's own (projects.blender.org/lab/blender_mcp), runs next to it in the
guest. The project's MCP config (`.mcp.json` for Claude Code) exposes it as the
`blender` server:

```
Claude Code ⇐ stdio ⇒ blender-vm mcp ⇐ vm pipe ⇒ blender-mcp ⇐ TCP 127.0.0.1:9876 ⇒ MCP add-on in Blender
```

Everything Blender-related lives in the guest. The add-on listens only on the
guest's loopback, and code the tools run (`execute_blender_code` runs any
Python) cannot touch the sandbox.

`scripts/blender-vm` (shortened to `blender-vm` below) manages the VM. It sits
in this skill's directory, which may be the project's `.claude/skills/blender`
or a global one such as `~/.claude/skills/blender` or `~/.agents/skills/blender`.
Run it by the absolute path of the base directory the harness showed when it
loaded this skill, for example
`~/.claude/skills/blender/scripts/blender-vm status`. It needs the `vm` command,
which exists only in a sandbox launched with `--vm`.

## Getting the tools working

1. `blender-vm status`. If it says the VM does not exist, or the provisioned
   revision differs from the local one, run `blender-vm up` (fresh VM: about
   2 minutes; when already up to date: under a second). Load the `vm` skill
   first if `vm` itself misbehaves. The VM does not outlive the session, so
   expect to run `up` at the start of each session that needs Blender.
2. If the project has no `blender` MCP server registered, install it as below.
3. If the `mcp__blender__*` tools are missing, because the server was just
   registered or the VM did not exist when the session started, ask the user
   to reconnect it (in Claude Code: `/mcp`, then reconnect or enable
   `blender`; a server new to the project may first need approving there, or a
   restart of the session). You cannot reconnect it yourself.

`blender-vm mcp` (what the MCP config runs) never creates or provisions a VM,
so the handshake stays fast. It fails when the VM does not exist, which is the
case at the start of most sessions: run `blender-vm up`, then reconnect as in
step 3. Blender finishes booting a few seconds after `up` returns. A tool call
that fails to connect right after that can be retried.

`blender-vm up` re-provisions only when `scripts/provision.sh` has changed,
and doing so **restarts Blender and loses unsaved work in it**. Save first.

## Installing the MCP server in a project

The server is registered per project, so a project needs it once. Check first:
for Claude Code, look for a `blender` entry under `mcpServers` in the project
root's `.mcp.json`. If it is missing, or its `command` points at a
`blender-vm` that no longer exists (the skill moved), register it from the
project root:

```bash
~/.claude/skills/blender/scripts/blender-vm install-mcp            # Claude Code: ./.mcp.json
~/.agents/skills/blender/scripts/blender-vm install-mcp codex      # Codex: ./.codex/config.toml
~/.agents/skills/blender/scripts/blender-vm install-mcp opencode   # opencode: ./opencode.json
```

Use the `blender-vm` of the skill copy you loaded. `install-mcp` takes an
optional project directory after the harness name (default: the current
directory), writes that `blender-vm`'s absolute path into the config, keeps
every other server in the file, and prints the file it wrote. Re-running it
updates the entry in place. Set `BLENDER_VM_NAME` when running it to bind the
project to a VM other than `blender`.

The entry looks like this, and can be written by hand the same way:

```json
{
  "mcpServers": {
    "blender": {
      "type": "stdio",
      "command": "/root/.claude/skills/blender/scripts/blender-vm",
      "args": ["mcp"]
    }
  }
}
```

Tell the user you added it, since `.mcp.json` is a project file they may
commit. Codex reads `.codex/config.toml` only in a project it trusts. Then
reconnect as in step 3 above.

## Using it

- Read the server's instructions and use `search_api_docs`,
  `get_python_api_docs` and `search_manual_docs`. They bundle the API reference
  and manual for the installed version, so look things up there rather than
  guessing at bpy, which changes between releases.
- `execute_blender_code` runs Python in the live session. Its `stdout` comes
  back in the result, so `print` what you need to see.
- To check visual results, use `get_screenshot_of_window_as_image` /
  `get_screenshot_of_area_as_image`. `render_viewport_to_path` and
  `render_thumbnail_to_path` write images inside the guest; pull them out to
  look at them.
- The `*_for_cli` variants run a separate background Blender on a .blend file
  in the guest. They leave the live session alone, which suits inspecting files
  you don't want to open.
- Rendering is CPU only, with Mesa llvmpipe for the viewport and no GPU. Keep
  Cycles samples and resolution modest, or use EEVEE or Workbench for previews.
- Blender's working directory is `/root/work`. Save .blend files and outputs
  there.
- For anything scripted or long (scene builds, multi-frame renders), a bpy
  script run in a separate background Blender is often simpler than the live
  session: `vm pipe blender -- sh -c 'cd /root/work/x && blender -b -P build.py -- ARGS'`.
  Use `vm pipe` for jobs that may exceed an hour, because `vm exec --timeout`
  is capped at 3600 s. Pack textures (`bpy.ops.file.pack_all()`) before saving
  a .blend you will copy out.

## Render only as much as was asked

- **Never start a long render (a video, an animation, a batch of high-res
  stills) unless the user explicitly asks for that render.** A request to
  improve or fix a scene is not such a request.
- While iterating, use low-res, low-sample previews freely, show them, and ask
  questions.
- When asking the user to judge textures, paint or a final look, render one
  single frame at high resolution for that, and keep the other views low-res.
  (A 1506×2000, 128-sample still of the kitchen takes about 13 minutes on 6
  CPUs, longer than the Bash tool's 10-minute foreground limit, so run it in
  the background.)

## Long-running jobs: report at least every 30 minutes

The user wants a progress update at least every 30 minutes during a long job
(a render, an animation, a bake). Silence for longer is not acceptable, even
when nothing has gone wrong.

- Make long jobs resumable before starting them. For an animation, set
  `use_overwrite = False` and `use_placeholder = True`, so a re-run skips
  frames already on disk. Time a few frames first and tell the user the
  estimate (frames × seconds per frame) before launching.
- Run the job in the background (Bash `run_in_background`, with `vm pipe` for
  anything over an hour). Then arm a **Monitor** whose command prints one
  progress line about every 25 minutes and exits on completion or failure, for
  example:

  ```bash
  while :; do
    n=$(vm exec blender -- sh -c 'ls /root/work/x/out | wc -l' 2>/dev/null || echo "?")
    echo "progress: $n/$TOTAL frames at $(date -u +%H:%M) UTC"
    [ "$n" = "$TOTAL" ] && exit 0
    grep -qE 'Error|Traceback' "$BG_OUTPUT_FILE" 2>/dev/null && { echo "FAILED"; exit 1; }
    sleep 1500
  done
  ```

  Use `timeout_ms` 1800000, which is the maximum. When the monitor emits or
  expires, give the user a one- or two-line update (done/total, rate, ETA,
  anything wrong), then re-arm it until the job ends.
- On completion, deliver the result straight away. Do not wait for the next
  scheduled check.

## Files in and out

```bash
blender-vm push model.blend textures/     # → /root/work/model.blend, /root/work/textures
blender-vm pull render.png out/            # /root/work/render.png → out/render.png
blender-vm pull /tmp/x.png                 # absolute guest path; into a new ./blender-out.XXXX
```

Each command prints the resulting path. Treat pulled files as guest output:
pull into a fresh directory, not over project files.

The guest's disk goes away with the VM, so `/root/work` is scratch space. Pull
out every .blend, render and script worth keeping as soon as it is done, not
at the end of the session.

## Watching

`blender-vm view` opens the VM's screen full-screen on the virtual display
(`:99`), where the user can also watch it live. Screenshot it with `scrot`
(load the `gui` skill). Only one viewer works at a time, and an open viewer
does not disturb the MCP tools.

## Lifecycle and limits

- **Use ephemeral VMs only. Never create a persistent Blender VM**: do not
  launch one with `vm launch --persistent`, and do not add `--persistent` to
  `blender-vm`. Each session builds a fresh VM with `blender-vm up`, which
  launches an ephemeral one.
- The VM and its disk are deleted when it is stopped (`vm stop`, a guest
  shutdown), after 6 h without use, and when the sandbox exits. An open MCP
  connection or `vm pipe` counts as use. A guest reboot keeps it.
- `blender-vm destroy` deletes it. Do so once the Blender work is done and its
  results are copied out; it holds host memory and project quota until then.
- `blender-vm` refuses to use a persistent VM under its name, which an older
  version of this skill created. Do not delete it yourself: ask the user
  whether anything in it should be copied out and whether to delete it.
- Defaults are 2 CPUs, 4 GiB and 20 GiB, on `images:ubuntu/24.04`. These fit
  the Incus project limits alongside other VMs. Override them on first launch
  with `BLENDER_VM_CPUS`, `BLENDER_VM_MEMORY`, `BLENDER_VM_DISK` and
  `BLENDER_VM_IMAGE`. A "Reached maximum aggregate value" error means other VMs
  hold the quota (`vm list`). Ask the user rather than stopping them.
- `BLENDER_VM_NAME` selects a different VM. Re-run `install-mcp` with it set,
  so the project's MCP entry uses that VM too.

## Troubleshooting

- `blender-vm status` shows the VM state, `blender-gui.service`, and whether
  the add-on is listening.
- Blender's log: `vm exec blender -- journalctl -u blender-gui -n 80 --no-pager`.
  Xkb keysym warnings and the GHOST/Wayland probe are harmless noise.
- Restart Blender (unsaved work is lost): `vm exec blender -- systemctl restart blender-gui`.
- The add-on refuses to start without online access. The service passes
  `--online-mode`, so keep that flag if you edit the session script.

## Upgrading

Versions are pinned, with checksums, at the top of `scripts/provision.sh`:
Blender from download.blender.org (`blender-X.Y.Z.sha256` next to the
tarball), the add-on zip, and the server commit from the
projects.blender.org release and tag. Do not install the PyPI package
`blender-mcp`: that name belongs to an unrelated third-party project. After
bumping the pins, run `blender-vm up`.
