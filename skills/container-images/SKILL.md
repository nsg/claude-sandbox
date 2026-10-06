---
name: container-images
description: Build OCI/Docker container images inside the sandbox with Buildah — Containerfile and Dockerfile builds, multi-stage builds, tagging, inspecting, exporting archives, pushing to a registry. Load before building or changing a container image, or whenever a task says `docker build` or `podman build`.
---

# Building Container Images

The sandbox is itself an unprivileged container, so nothing here can *run*
containers: there is no Docker daemon, no Podman, no BuildKit. Images are
built with **Buildah**, which needs neither a daemon nor a container runtime.

`buildah build` takes the same arguments as `docker build`:

```bash
buildah build -t registry.example.com/team/app:1.2.3 .
buildah build -f deploy/Containerfile --target runtime --build-arg VERSION=1.2.3 -t app:dev .
buildah build --secret id=npmrc,src=$HOME/.npmrc -t app:dev .
```

It reads `Containerfile` or `Dockerfile` from the build context. Two settings
are preconfigured and need no flags: `RUN` steps execute with chroot isolation
(`BUILDAH_ISOLATION=chroot`), and the storage driver is chosen when the
sandbox starts.

## Check the result

```bash
buildah images
buildah inspect --format '{{.OCIv1.Config.Entrypoint}} {{.OCIv1.Config.Cmd}} {{.OCIv1.Config.User}}' app:dev

# Run a command inside the image's filesystem
c=$(buildah from app:dev)
buildah run "$c" -- /usr/local/bin/app --version
buildah rm "$c"
```

`buildah run` is a chroot, not a container. The process shares the sandbox's
network and process list, so a server started this way binds the sandbox's own
ports, and the image's `ENTRYPOINT`, `HEALTHCHECK` and resource limits are not
applied. It proves that a binary starts and files are in place; it does not
prove the image behaves correctly under a real runtime. Say so when reporting.

## Get the image out

Built images live in the sandbox's image store and disappear with the sandbox.
Export or push whatever must survive:

```bash
buildah push app:dev oci-archive:/workspace/app.oci.tar:app:dev        # OCI layout tarball
buildah push app:dev docker-archive:/workspace/app.tar:app:dev          # for `docker load`
buildah push app:dev docker://registry.example.com/team/app:dev         # a registry
```

The sandbox holds no registry credentials. Do not search for any. If a push
needs authentication, ask the user for a token and use
`buildah login --username USER --password-stdin REGISTRY`; in most projects CI
pushes the image and a local build only has to prove the build works. A
plain-HTTP registry needs `--tls-verify=false`.

## Limits

- **No heredocs.** This Buildah (1.33, from Ubuntu 24.04) rejects
  `RUN <<EOF` and `COPY <<EOF`; the error names a line of the heredoc body as
  an "Unknown instruction". Use `RUN set -e; cmd1; cmd2` with line
  continuations, or `COPY` a script and run it.
- **No BuildKit-only syntax.** `# syntax=` lines are ignored and `COPY --link`
  is an error. `RUN --mount=type=cache|bind|secret`, `COPY --chown/--chmod`
  and multi-stage `COPY --from` work.
- **No other architectures for `RUN`.** `--platform linux/arm64` can assemble
  an image from `FROM` and `COPY` steps, but a `RUN` step fails with
  "exec format error" because there is no emulation.
- **`RUN` steps are not isolated from the sandbox.** They share its network,
  hostname and process list. Do not build a Containerfile you would not run
  directly in the sandbox.
- **No layer cache by default.** Each build re-runs every step. Add `--layers`
  while iterating on a Containerfile to reuse unchanged steps.

## Disk space

```bash
buildah info | grep GraphDriverName      # overlay or vfs
buildah rm --all && buildah rmi --prune   # drop working containers and dangling images
buildah rmi --all                         # drop everything
```

With `overlay`, layers are shared and a build costs roughly the image size.
With `vfs` (the fallback when the image store is not on a suitable
filesystem), every layer is a full copy: expect several times the image size,
more with `--layers`, and prune between builds of large images.
