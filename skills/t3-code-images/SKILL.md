---
name: t3-code-images
description: Display local screenshots and other workspace images inline in assistant messages by issuing T3 Code signed asset URLs. Use when a Codex, Claude Code, or OpenCode session is directly hosted by T3 Code—as confirmed by injected T3 runtime context or the product-native t3-code MCP server—and an image must be shown in the conversation; never apply this procedure in another UI harness or a nested external runner.
---

# T3 Code Images

Use T3 Code's signed workspace-asset route to make a local image visible in an
assistant Markdown message.

## Hard boundary

Apply this workflow only when the current session is directly hosted by T3
Code. Confirm that with at least one in-band signal:

- Injected runtime context explicitly identifies T3 Code. Codex sessions
  currently receive this signal.
- The product-native `t3-code` MCP server or its preview tools are configured
  for the current session. T3-hosted Claude Code and OpenCode sessions
  currently use this signal.

Do not infer T3 Code from a repository name, directory, process, available
source tree, or the mere presence of `~/.t3`.

Outside the T3 Code harness, stop using this skill and use that environment's
native image or attachment mechanism. Do not inspect `~/.t3`, read a T3 signing
key, or construct `/api/assets/...` URLs in another harness.

A Claude Code or OpenCode process launched from inside another agent's terminal
is a nested runner, not a T3-hosted session. It may be able to execute the
helper, but it must return its findings to the parent instead of presenting the
URL as its own visible T3 attachment.

## Provider compatibility

Use the same helper and Markdown output for T3-hosted Codex, Claude Code, and
OpenCode sessions. T3 Code projects all three providers' assistant text through
the same Markdown renderer, so the signed asset route is provider-neutral.

This does not make the procedure UI-harness-neutral: `/api/assets/...` belongs
to T3 Code. In a different UI, use that UI's native attachment mechanism.

## Why this is necessary

In the T3 Code implementation for which this skill was written, assistant
Markdown does not translate local filesystem paths into browser-accessible
URLs, and tool-emitted image blocks are not promoted into visible assistant
attachments. A Markdown image whose source is a signed T3 Code
`/api/assets/...` URL renders correctly.

Treat this as T3 Code implementation knowledge that may change. If the helper
reports that the T3 instance layout or asset route is incompatible, inspect the
current T3 Code implementation instead of applying the procedure elsewhere.

## Show an image

1. Confirm the image is one of: AVIF, GIF, ICO, JPEG, PNG, SVG, or WebP.
2. Choose a canonical workspace root containing the image. Prefer the current
   project root. For an image in shared workspace state, such as
   `/workspace/.claude-sandbox/shot.png`, use `/workspace`.
3. Resolve `scripts/issue-image-url.mjs` relative to this `SKILL.md`, then run:

   ```bash
   node <skill-directory>/scripts/issue-image-url.mjs \
     /absolute/path/to/shot.png \
     --workspace-root /absolute/workspace/root \
     --alt "Concise image description"
   ```

4. Put the script's single Markdown line directly in the assistant's final
   response. Do not wrap it in a code block and do not replace its URL with the
   local path.
5. Keep any explanatory text and clickable local-file fallback separate from
   the Markdown image.

The helper discovers the active T3 Code instance, reads its signing key without
printing it, creates an exact-file capability valid for one hour, and verifies
that the running T3 server returns an image before printing Markdown. If more
than one live instance is found, pass the intended instance directory with
`--instance-dir /path/to/~/.t3/instances/<instance>`.

## Safety and failure handling

- Treat the signed URL as a one-hour bearer capability for exactly that image.
  Share it only in the intended conversation.
- Never print, copy, inspect, or expose the contents of
  `asset-access-signing-key.bin`.
- Never sign an image outside the chosen workspace root.
- Re-run the helper to issue a fresh URL after expiration.
- If verification fails, do not send the URL. Check that the image still
  exists, the selected T3 instance is live, and the workspace root contains the
  image.
- A local Markdown path such as `![shot](/workspace/project/shot.png)` and a
  tool image result are not substitutes for this workflow in the affected T3
  Code implementation.
