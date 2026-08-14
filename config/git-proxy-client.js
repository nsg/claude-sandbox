#!/usr/bin/env node
"use strict";

const net = require("net");
const { spawnSync } = require("child_process");
const fs = require("fs");

const RUNTIME_DIR = "/run/claude-sandbox";
const SOCKET_PATH = fs.existsSync(RUNTIME_DIR)
  ? `${RUNTIME_DIR}/git-proxy.sock`
  : "/workspace/.claude-sandbox/git-proxy.sock";

const args = process.argv.slice(2);
const request = JSON.stringify({ args, cwd: process.cwd() }) + "\n";

const socket = net.createConnection(SOCKET_PATH, () => {
  socket.write(request);
});

let data = "";

socket.on("data", (chunk) => {
  data += chunk.toString();
});

socket.on("end", () => {
  try {
    const response = JSON.parse(data.trim());
    let exitCode = response.exit_code;
    let trackingError = "";
    if (exitCode === 0) {
      const commands = [];
      for (const update of response.tracking_updates || []) {
        const validRef =
          typeof update.reference === "string" &&
          update.reference.startsWith("refs/remotes/origin/") &&
          !update.reference.includes("\0");
        const validOid = (value) =>
          typeof value === "string" && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(value);
        if (!validRef || !validOid(update.new_oid) ||
            (update.old_oid !== null && !validOid(update.old_oid))) {
          exitCode = 1;
          trackingError = "git-proxy-client: host returned an invalid tracking-ref update\n";
          break;
        }
        const oldOid = update.old_oid || "0".repeat(update.new_oid.length);
        commands.push(
          `update ${update.reference} ${update.new_oid} ${oldOid}\n`,
        );
      }
      if (!trackingError && commands.length > 0) {
        const result = spawnSync(
          "/usr/bin/git",
          ["update-ref", "--no-deref", "--stdin"],
          { cwd: process.cwd(), encoding: "utf8", input: commands.join("") },
        );
        if (result.status !== 0) {
          exitCode = 1;
          trackingError =
            "git-proxy-client: push succeeded, but the local tracking ref could not be updated\n" +
            (result.stderr || result.error?.message || "");
        }
      }
    }
    if (response.stdout) {
      process.stdout.write(response.stdout);
    }
    if (response.stderr) {
      process.stderr.write(response.stderr);
    }
    if (trackingError) {
      process.stderr.write(trackingError);
    }
    process.exit(exitCode);
  } catch (e) {
    process.stderr.write("git-proxy-client: failed to parse response: " + e.message + "\n");
    process.exit(1);
  }
});

socket.on("error", (err) => {
  process.stderr.write("git-proxy-client: connection error: " + err.message + "\n");
  process.exit(1);
});
