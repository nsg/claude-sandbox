#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const TOKEN_TTL_MS = 60 * 60 * 1000;
const IMAGE_EXTENSIONS = new Set([
  ".avif",
  ".gif",
  ".ico",
  ".jpeg",
  ".jpg",
  ".png",
  ".svg",
  ".webp",
]);

function usage() {
  return [
    "Usage:",
    "  issue-image-url.mjs IMAGE [--workspace-root DIR] [--alt TEXT]",
    "                      [--instance-dir DIR]",
  ].join("\n");
}

function parseArguments(argv) {
  let image;
  let workspaceRoot = process.cwd();
  let alt;
  let instanceDir;

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (!argument.startsWith("--") && image === undefined) {
      image = argument;
      continue;
    }

    if (["--workspace-root", "--alt", "--instance-dir"].includes(argument)) {
      const value = argv[index + 1];
      if (value === undefined) {
        throw new Error(`Missing value for ${argument}.`);
      }
      index += 1;
      if (argument === "--workspace-root") workspaceRoot = value;
      if (argument === "--alt") alt = value;
      if (argument === "--instance-dir") instanceDir = value;
      continue;
    }

    throw new Error(`Unknown argument: ${argument}`);
  }

  if (image === undefined) throw new Error("Missing image path.");
  return { image, workspaceRoot, alt, instanceDir };
}

async function isFile(filePath) {
  try {
    return (await fs.stat(filePath)).isFile();
  } catch {
    return false;
  }
}

async function readCandidate(instanceDir) {
  const userdataDir = path.join(instanceDir, "userdata");
  const runtimePath = path.join(userdataDir, "server-runtime.json");
  const signingKeyPath = path.join(
    userdataDir,
    "secrets",
    "asset-access-signing-key.bin",
  );
  if (!(await isFile(runtimePath)) || !(await isFile(signingKeyPath))) return null;

  try {
    const runtime = JSON.parse(await fs.readFile(runtimePath, "utf8"));
    if (typeof runtime.origin !== "string" || !runtime.origin.startsWith("http")) {
      return null;
    }
    return { instanceDir, signingKeyPath, origin: runtime.origin };
  } catch {
    return null;
  }
}

async function serverIsReachable(origin) {
  try {
    const response = await fetch(origin, {
      redirect: "manual",
      signal: AbortSignal.timeout(2_000),
    });
    await response.body?.cancel();
    return true;
  } catch {
    return false;
  }
}

async function findInstance(explicitInstanceDir) {
  if (explicitInstanceDir !== undefined) {
    const candidate = await readCandidate(path.resolve(explicitInstanceDir));
    if (candidate === null || !(await serverIsReachable(candidate.origin))) {
      throw new Error("The selected T3 Code instance is not live or has no signing key.");
    }
    return candidate;
  }

  const instancesRoot = path.join(os.homedir(), ".t3", "instances");
  let entries;
  try {
    entries = await fs.readdir(instancesRoot, { withFileTypes: true });
  } catch {
    throw new Error("No T3 Code instances directory was found.");
  }

  const candidates = (
    await Promise.all(
      entries
        .filter((entry) => entry.isDirectory())
        .map((entry) => readCandidate(path.join(instancesRoot, entry.name))),
    )
  ).filter((candidate) => candidate !== null);
  const liveCandidates = [];
  for (const candidate of candidates) {
    if (await serverIsReachable(candidate.origin)) liveCandidates.push(candidate);
  }

  if (liveCandidates.length === 0) {
    throw new Error("No live T3 Code instance with an asset signing key was found.");
  }
  if (liveCandidates.length > 1) {
    const choices = liveCandidates.map((candidate) => candidate.instanceDir).join(", ");
    throw new Error(
      `Multiple live T3 Code instances were found (${choices}); pass --instance-dir.`,
    );
  }
  return liveCandidates[0];
}

function markdownAlt(value) {
  return value.replaceAll("\\", "\\\\").replaceAll("]", "\\]").replaceAll("\n", " ");
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  const imagePath = await fs.realpath(path.resolve(options.image));
  const workspaceRoot = await fs.realpath(path.resolve(options.workspaceRoot));
  const imageStat = await fs.stat(imagePath);
  if (!imageStat.isFile()) throw new Error("The image path is not a file.");

  const extension = path.extname(imagePath).toLowerCase();
  if (!IMAGE_EXTENSIONS.has(extension)) {
    throw new Error(`Unsupported image extension: ${extension || "(none)"}.`);
  }

  const relativePath = path.relative(workspaceRoot, imagePath);
  if (
    relativePath.length === 0 ||
    relativePath === ".." ||
    relativePath.startsWith(`..${path.sep}`) ||
    path.isAbsolute(relativePath)
  ) {
    throw new Error("The image must be inside the selected workspace root.");
  }

  const instance = await findInstance(options.instanceDir);
  const expiresAt = Date.now() + TOKEN_TTL_MS;
  const claims = JSON.stringify({
    version: 1,
    kind: "workspace-file-exact",
    workspaceRoot,
    relativePath: relativePath.split(path.sep).join("/"),
    expiresAt,
  });
  const payload = Buffer.from(claims).toString("base64url");
  const signingKey = await fs.readFile(instance.signingKeyPath);
  const signature = crypto.createHmac("sha256", signingKey).update(payload).digest("base64url");
  const relativeUrl = `/api/assets/${payload}.${signature}/${encodeURIComponent(path.basename(imagePath))}`;

  const response = await fetch(`${instance.origin}${relativeUrl}`, {
    redirect: "error",
    signal: AbortSignal.timeout(5_000),
  });
  const contentType = response.headers.get("content-type") ?? "";
  await response.body?.cancel();
  if (!response.ok || !contentType.toLowerCase().startsWith("image/")) {
    throw new Error(
      `T3 Code rejected the signed image URL (${response.status}, ${contentType || "no content type"}).`,
    );
  }

  const alt = markdownAlt(options.alt ?? path.basename(imagePath, extension));
  process.stdout.write(`![${alt}](${relativeUrl})\n`);
}

main().catch((error) => {
  process.stderr.write(`issue-image-url: ${error.message}\n${usage()}\n`);
  process.exitCode = 1;
});
