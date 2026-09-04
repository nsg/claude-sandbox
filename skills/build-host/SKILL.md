---
name: build-host
description: Load when the user asks to use a build host, or when a build or test needs native host capabilities unavailable in the current sandbox, but only if this installation explicitly defines and configures a build host. Do not load for ordinary local builds or when no build host is defined.
---

# Shared build host

Use a remote build host only when this skill explicitly defines how its hostname
and access details are configured. Do not assume that a build host exists in
environments where it is not defined. This installation reads the endpoint from
the repository-local configuration described below.

The host is regularly reset, so its filesystem, installed packages, host key,
and runtime state are ephemeral. Root access inside it is intentional.

It is also a shared resource. Other users or agents may have work running at
the same time, and root access is not an ownership boundary inside the guest.
Touch only workspaces, processes, tmux sessions, and systemd units created for
the current job. Never stop processes by broad name, reuse an unfamiliar
directory, or inspect, modify, or clean up another job's files. System-wide
package changes are allowed when required, but do not remove packages or
restart unrelated services to resolve contention.

Keep the current local checkout as the source of truth. Never leave the only
copy of source changes, logs, or artifacts on the VM.

## Repository-local identity

Store access material under the current repository root:

```text
.build-host/id_ed25519
.build-host/id_ed25519.pub
.build-host/known_hosts
.build-host/host
```

The repository `.gitignore` must contain `/.build-host/` before creating these
files. Never display, copy to the VM, or include the private key in a workspace
transfer.

Resolve the paths from the repository rather than assuming the current working
directory:

```bash
repo_root=$(git rev-parse --show-toplevel)
key_dir="$repo_root/.build-host"
key_file="$key_dir/id_ed25519"
known_hosts="$key_dir/known_hosts"
host_file="$key_dir/host"
```

The `host` file contains the DNS name of the build host on one line. If it is
missing, ask the user to create it and stop. Read and validate it before use:

```bash
IFS= read -r build_host < "$host_file"
if [[ ! "$build_host" =~ ^[A-Za-z0-9][A-Za-z0-9.-]*$ ]]; then
  echo 'Invalid build host name' >&2
  exit 1
fi
target="root@$build_host"
```

If the private key does not exist, create an Ed25519 key without a passphrase so
non-interactive remote builds work:

```bash
mkdir -p "$key_dir"
chmod 0700 "$key_dir"
ssh-keygen -t ed25519 -a 64 -N '' -C 't3-build-host' -f "$key_file"
chmod 0600 "$key_file"
chmod 0644 "$key_file.pub"
```

Show the user only `$key_file.pub` and ask them to append it to
`/root/.ssh/authorized_keys` on the VM. Stop until they confirm it is installed.
If the private key already exists, reuse it. If only the public file is missing,
derive it with `ssh-keygen -y -f "$key_file" > "$key_file.pub"`.

## Connecting

Resolve `$build_host` through the container's local resolver. Do not pin a
previously observed address because the disposable VM's address may change.
`getent ahostsv4 "$build_host"` is a useful diagnostic when resolution fails.

Call `/usr/bin/ssh` explicitly. In the sandbox, plain `ssh` resolves to the
`/usr/local/bin/ssh` host-bridge wrapper, which does not use this
repository-local identity. Use the repository-specific key and host database
on every connection:

```bash
/usr/bin/ssh -i "$key_file" \
  -o IdentitiesOnly=yes \
  -o UserKnownHostsFile="$known_hosts" \
  -o StrictHostKeyChecking=accept-new \
  "$target"
```

For file transfer, likewise use `/usr/bin/scp -S /usr/bin/ssh` or configure
rsync's remote shell as `/usr/bin/ssh` with the same identity and host-key
options. Never let `.build-host/` enter the transfer.

The host key is expected to change after the host is reset. On an explicit
host-identification-changed error, remove only this host's stale entries from
the repository-specific file, report the rotation, and retry once with
`StrictHostKeyChecking=accept-new`:

```bash
ssh-keygen -R "$build_host" -f "$known_hosts"
ssh-keygen -R "[$build_host]:22" -f "$known_hosts"
```

Do not disable host-key checking globally and do not treat authentication,
timeout, or DNS failures as host-key rotation.

## Preparing a fresh VM

Assume every connection may reach a fresh image. Before a task, inspect the OS,
free disk and memory, and relevant tool versions. Read the project's own build
instructions, then install only the tooling required for that project. Prefer
official Ubuntu packages and first-party distribution channels.

Do not expose or request the hypervisor's container/VM management socket, host
filesystem, SSH agent, or unrelated credentials. The SSH session grants broad
authority inside the disposable guest only.

## Transferring and building

Transfer the working tree rather than reconstructing it from the Git remote, so
tracked, untracked, and uncommitted inputs are preserved. Exclude at least
`.build-host/` and `.claude-sandbox/`. Allocate one collision-resistant remote
directory at `/root/workspaces/<unique-name>` for each independent build job or
repository checkout, and reuse that directory for all steps of the same job.
Never assume a fixed repository directory is yours and never reuse a directory
unless this session created it for the current job. A reliable remote-side
allocation is:

```bash
install -d -m 0700 /root/workspaces
mktemp -d /root/workspaces/job-XXXXXXXXXX
```

Capture and validate the returned absolute path before transferring files or
running commands. Do not use an unresolved variable or broad path with
`--delete`.

Run ordinary commands directly over SSH. For a long build that must survive a
connection interruption, use a collision-resistant tmux session or systemd unit
name belonging to the current job and record the remote log path. Never attach
to or stop an unfamiliar session or unit. Stream or poll progress often enough
to keep the user informed, and preserve the real exit status.

Copy requested artifacts and useful failure logs back into the local workspace
before concluding. Make fixes in the local source of truth and transfer again;
if remote source was intentionally edited, synchronize and review those changes
locally before the VM resets.

Never assume yesterday's workspace, packages, caches, SSH host key, or running
processes still exist.
