---
name: vm
description: Create, boot and drive real virtual machines through the host's Incus — launch from a public image, an ISO or a qcow2 disk, watch the screen, send keys and clicks, run commands in the guest. Load before using the `vm` command, or when a task needs a full VM rather than the sandbox container.
---

# Virtual Machines

The `vm` command asks a host-side bridge to create KVM virtual machines in
Incus. The sandbox never holds the Incus credential or `/dev/kvm`; the bridge
accepts a small set of verbs and builds every Incus request itself. `vm help`
prints the authoritative verb list.

## Check availability first

```bash
vm status
```

- "not launched with `--vm`" — the feature is off for this sandbox. Tell the
  user to relaunch with `claude-sandbox --vm`; nothing inside the container
  can enable it.
- An itemised FAIL report — the host setup is incomplete. Relay the report to
  the user as written (it names the commands to run). Do not try to work
  around it. Once the user has fixed the host, `vm status` passes without a
  relaunch.

## Facts that shape every task

- **VMs are ephemeral by default.** `vm stop`, a guest shutdown, `vm delete`
  and the sandbox exiting all delete the VM and its disk. A guest *reboot*
  keeps it. Copy anything worth keeping out of the guest before stopping it.
- **`vm launch --persistent` opts a VM out.** Its disk survives `vm stop` and
  guest shutdowns, `vm start NAME` boots it again, and only `vm delete`
  removes it (or the idle limit below). When the sandbox exits it is shut
  down, not deleted, and it is still there next session — check `vm list`
  (last column) before launching.
  Use it only when the task needs to power the VM off and keep it, or the user
  wants it kept; it holds disk quota while stopped, so delete it once it is no
  longer wanted.
- **Unused VMs are cleaned up.** Any `vm` command naming a VM (`info`,
  `exec`, `pipe`, `start`, `stop`, `restart`, `eject`, `screen`/`view`,
  `console-log`), an open `vm view` and an open `vm pipe` count as use;
  `vm list` does not. After
  6 hours without use a VM is stopped — so an ephemeral one is deleted — and a
  persistent VM is deleted with its disk after 24 hours without use. Change
  them at launch with `--idle-stop` and, for persistent VMs, `--idle-delete`
  (durations such as `12h` or `3d`; idle-stop 1h–7d, idle-delete up to
  30d). If you step away from a VM you
  still need, for example while a long job runs elsewhere, run `vm info NAME`
  now and then; it also shows how long the VM has been idle. A command that
  arrives while an idle VM is being stopped is refused with a message saying
  so; check `vm list` before going on.
- **Ephemeral VMs are deleted when the bridge starts healthy and when the
  launcher exits.** If the bridge itself is killed, they keep running until
  the next `--vm` start; the user can check with
  `incus list --project claude-sandbox`. Imported ISOs and disk images are kept
  between sessions (`vm media`).
- **One screen client at a time.** SPICE drops the current viewer when a second
  client connects, so keep a single viewer per VM.
- VMs sit on the host's Incus bridge network with normal outbound access.
  `vm info NAME` shows the guest's addresses once it has booted.
- **VMs cannot reach the sandbox.** The sandbox runs in its own network
  namespace behind Podman's outbound mapping, so connections only go one way:
  the sandbox can reach the VMs and the network, and the VMs can reach the
  host and the network, but nothing in a VM can connect to a server running
  inside the sandbox. When a guest needs a service — a web server, a
  package mirror, files to download — run it on a small separate VM and point
  the guest at that VM's address. To move files between the sandbox and a
  guest, use `vm pipe` (below); it needs no network.
- CPU, memory, disk and VM count are capped by the user's Incus project; a
  "limit" error means ask the user, not retry.

## Launch

```bash
vm launch web --image images:debian/13                 # public Incus image, boots in seconds
vm launch web --image images:ubuntu/24.04 --cpus 4 --memory 8GiB --disk 40GiB
vm launch inst --iso ubuntu-server --no-secureboot     # empty VM booting an imported ISO
vm launch app --image my-disk                          # imported qcow2
vm launch db --image images:debian/13 --persistent     # survives stop and later sessions
vm stop db && vm start db                              # persistent only: power-cycle, disk kept
vm launch soak --image images:debian/13 --idle-stop 12h  # longer idle allowance
vm list
vm info web
```

Defaults are 2 CPUs, 4GiB memory and a 20GiB disk. Names are lowercase
letters, digits and hyphens.

Prefer `images:` when any stock Linux will do: those images boot quickly and
include the Incus agent, so `vm exec` works. Use an ISO or a disk image only
when the task is about that specific system.

### Bringing your own image

Files must be inside `/workspace`.

```bash
vm import /workspace/isos/ubuntu-24.04-live-server-amd64.iso ubuntu-server
vm import /workspace/images/appliance.qcow2 my-disk
vm media
vm media-delete ubuntu-server
```

The type is detected from the file contents. Disk images must be qcow2 without
a backing file; convert anything else first:
`qemu-img convert -O qcow2 disk.raw disk.qcow2`. Imports of large files take
minutes. ISO imports are capped at 16 GiB, qcow2 imports at 64 GiB, and no more
than 16 imported media entries may be retained.

Use `--no-secureboot` for installers and systems that are not signed for UEFI
Secure Boot (most non-mainstream ISOs). If a VM sits at the firmware screen or
drops to the UEFI shell, that flag is the first thing to try.

VMs get a virtual TPM 2.0 (`/dev/tpm0` in a Linux guest) by default, which
Windows 11 installers require. Pass `--no-tpm` only when launch fails because
the host lacks `swtpm`.

## Run commands in the guest

```bash
vm exec web -- uname -a
vm exec web --timeout 600 -- sh -c 'apt-get update && apt-get install -y nginx'
```

This is non-interactive, returns the command's exit code, and is far cheaper
than driving the screen — use it whenever the guest has the Incus agent
(`images:` VMs do; ISO installs and foreign disk images usually do not). Just
after launch the agent may not be up yet: retry for up to a minute before
concluding it is missing.

### Streaming data: `vm pipe`

`vm exec` gives the guest no stdin and buffers output. `vm pipe` streams
instead: the sandbox's stdin goes to the guest command, its stdout and stderr
come back as they are written, and `vm pipe` exits with its exit code. It has
no timeout and keeps the VM in use while open. Use it for anything that moves
data or talks a protocol over stdio:

```bash
tar -C src -cf - . | vm pipe web -- tar -C /build -xf -     # copy a tree in
vm pipe web -- tar -C /build/out -cf - . | tar -C out -xf - # copy results out
vm pipe web -- sh -c 'cat > /etc/app.conf' < app.conf        # write one file
rsync -a -e 'vm pipe' src/ web:/build/                       # incremental sync
```

`vm pipe NAME -- CMD ARG...` runs the command exactly as given. Without `--`
the words are joined and run by the guest's `sh`, the way ssh runs a remote
command; that form is what makes `rsync -e 'vm pipe'` work (rsync must be
installed in the guest too). At most 8 pipes may be open at once.

Results pulled out of a guest are written by the sandbox side, so treat a guest
that ran untrusted code as untrusted: unpack its output into a fresh directory
rather than over the workspace.

`vm console-log NAME` prints the guest's serial console output, useful for
boot failures on guests that log there. It requires an Incus server newer than
6.0.0.

## The screen

```bash
vm view web        # opens the VM's display full-screen on the virtual X display (:99)
```

The viewer covers the whole 1280x800 display, so the standard GUI tools act on
the guest directly. Load the `gui` skill for the general see → act → verify
loop; the VM specifics are:

```bash
mkdir -p /workspace/.claude-sandbox/vm
scrot /workspace/.claude-sandbox/vm/screen.png          # then view it with the Read tool

xdotool type --delay 80 'root'                          # type text
xdotool key Return                                      # single keys: Return, Escape, Tab, Up, F2 ...
xdotool key ctrl+alt+Delete                             # key combinations
xdotool mousemove 640 400 click 1                       # pointer (absolute coordinates)
```

- Always screenshot after acting. Guests are slow to react during boot and
  installs; when waiting, poll with a screenshot every few seconds rather than
  sleeping blind.
- Type with `--delay 80` or more. Firmware menus and installers drop
  characters that arrive faster.
- Screenshot coordinates equal pointer coordinates only while the guest's
  resolution matches the display. If the picture is letterboxed or scaled,
  work out the offset from a screenshot before clicking, and confirm each
  click with another screenshot.
- The viewer exits when the VM stops or its display resets (some reboots do
  this). If a screenshot shows the desktop instead of the guest, run
  `vm view NAME` again.
- The user can watch the same display live from the admin portal.

`vm screen NAME` prints the raw `spice+unix://` address if another SPICE
client is needed; `vm view` is that plus `remote-viewer` in kiosk mode.

## Installing an OS from an ISO

1. `vm import` the ISO, then `vm launch NAME --iso MEDIA` (add
   `--no-secureboot` unless the ISO is known to be signed).
2. `vm view NAME` and screenshot. Catch the boot menu early: many installers
   auto-select after a few seconds.
3. Drive the installer with keys; prefer keyboard navigation over the pointer.
4. When the installer asks to reboot, run `vm eject NAME` first. It
   hot-unplugs the ISO while the guest keeps running, so the reboot starts
   the installed system rather than the installer again.
5. Then let the guest reboot itself. On an ephemeral VM, do **not**
   `vm stop` it — that deletes the freshly installed disk. Installers that
   power off instead of rebooting (or are told to) delete it the same way, so
   make sure the last step is a reboot. A `--persistent` VM may power off;
   `vm start` it afterwards.

On an ephemeral VM, an installed system lives only as long as the VM does.
If the install must survive a power-off or outlast the session, launch it
with `--persistent` from the start — an ephemeral VM cannot be converted.

## Clean up

```bash
vm delete web
```

Delete VMs as soon as the task is done; they hold host memory and count
against the project limits. The idle limits are a backstop, not a cleanup
plan: delete a persistent VM yourself unless the user asked to keep it.
