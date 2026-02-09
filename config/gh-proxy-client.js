#!/usr/bin/env node
"use strict";

const net = require("net");

const SOCKET_PATH = "/workspace/.claude-sandbox/gh-proxy.sock";

const args = process.argv.slice(2);
const request = JSON.stringify({ args }) + "\n";

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
    if (response.stdout) {
      process.stdout.write(response.stdout);
    }
    if (response.stderr) {
      process.stderr.write(response.stderr);
    }
    process.exit(response.exit_code);
  } catch (e) {
    process.stderr.write("gh-proxy-client: failed to parse response: " + e.message + "\n");
    process.exit(1);
  }
});

socket.on("error", (err) => {
  process.stderr.write("gh-proxy-client: connection error: " + err.message + "\n");
  process.exit(1);
});
