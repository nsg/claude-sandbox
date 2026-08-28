---
name: t3-code-images
description: Display local workspace images inline in T3 Code conversations using Markdown paths that T3 resolves through its own asset service. Use when a directly T3-hosted Codex, Claude Code, or OpenCode session needs to show a local screenshot or image; do not use this workflow in another UI harness or a nested external runner.
---

# T3 Code Images

Show a local image with ordinary Markdown that points to the image file. Current
T3 Code clients recognize workspace paths in assistant Markdown and obtain a
thread-scoped signed asset URL from the T3 server themselves.

## Hard boundary

Apply this workflow only when the current session is directly hosted by T3
Code. Confirm that with at least one in-band signal:

- Injected runtime context explicitly identifies T3 Code.
- The product-native `t3-code` MCP server or its preview tools are configured
  for the current session.

Do not infer T3 Code from a repository name, directory, process, available
source tree, or the presence of `~/.t3`.

A Claude Code, Codex, or OpenCode process launched inside another agent's
terminal is a nested runner, not a directly T3-hosted session. It should return
the image path to its parent; the parent can present the image if its own
session is hosted by T3 Code.

Outside T3 Code, use that harness's native image or attachment mechanism.

## Show an image

1. Keep the image inside the active thread's workspace or worktree. The file
   must remain inside that root after canonical and symlink resolution.
2. Use one of the workspace image formats T3 Code previews: AVIF, GIF, ICO,
   JPEG, PNG, SVG, or WebP.
3. Put a Markdown image with an absolute path directly in the assistant
   response:

   ```markdown
   ![Concise image description](/absolute/path/to/image.png)
   ```

   A relative path is also supported and resolves against the thread's working
   directory. Prefer a canonical absolute path when there is any ambiguity;
   `~` is not expanded. If the path contains spaces or other Markdown-sensitive
   characters, enclose the destination in angle brackets:

   ```markdown
   ![Concise image description](</absolute/path/with spaces/image.png>)
   ```

   Percent-encode a literal `#` or `?` in a filename as `%23` or `%3F`. On
   Windows, prefer forward slashes in drive-absolute paths.

4. Do not wrap the actual Markdown image in a code block. Keep explanatory text
   and any clickable file link separate.

The web, desktop, and mobile clients resolve the workspace path and perform the
signed asset exchange. Do not construct an `/api/assets/...` URL, read T3
instance files, or access an asset signing key.

Use a local image-viewing tool only when you need to inspect the pixels before
describing the image; it is not required to make the image visible to the user.

## Failure handling

- Check that the file still exists, is a regular file with a supported
  extension, and remains inside the active thread's workspace or worktree after
  symlink resolution.
- Prefer a canonical absolute path if a relative path renders as unavailable.
- A missing thread context, disconnected environment, asset request failure, or
  unsupported image bytes can also make the image unavailable. Report the
  actual condition and provide a separate clickable local-file fallback.
- If the client or server predates workspace-image Markdown support, report that
  T3 Code must be updated. Do not fall back to minting a signed URL.
