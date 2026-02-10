#!/usr/bin/env node
"use strict";

const net = require("net");
const path = require("path");

const SOCKET_PATH = "/workspace/.claude-sandbox/clipboard-proxy.sock";

const invocation = path.basename(process.argv[1]);
const args = process.argv.slice(2);

function validate() {
  if (invocation === "xclip") {
    const expected = ["-selection", "clipboard", "-t", "image/png", "-o"];
    if (
      args.length === expected.length &&
      args.every((a, i) => a === expected[i])
    ) {
      return;
    }
    process.stderr.write(
      "xclip (proxy): only 'xclip -selection clipboard -t image/png -o' is supported\n"
    );
    process.exit(1);
  }

  if (invocation === "wl-paste") {
    const expected = ["--type", "image/png"];
    if (
      args.length === expected.length &&
      args.every((a, i) => a === expected[i])
    ) {
      return;
    }
    process.stderr.write(
      "wl-paste (proxy): only 'wl-paste --type image/png' is supported\n"
    );
    process.exit(1);
  }

  process.stderr.write(
    "clipboard-proxy-client: unsupported invocation: " + invocation + "\n"
  );
  process.exit(1);
}

validate();

const request = JSON.stringify({ command: "read_image" }) + "\n";

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
    if (response.stdout_b64) {
      const buf = Buffer.from(response.stdout_b64, "base64");
      process.stdout.write(buf);
    }
    if (response.stderr) {
      process.stderr.write(response.stderr);
    }
    process.exit(response.exit_code);
  } catch (e) {
    process.stderr.write(
      "clipboard-proxy-client: failed to parse response: " + e.message + "\n"
    );
    process.exit(1);
  }
});

socket.on("error", (err) => {
  process.stderr.write(
    "clipboard-proxy-client: connection error: " + err.message + "\n"
  );
  process.exit(1);
});
