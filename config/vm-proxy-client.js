#!/usr/bin/env node
"use strict";

const fs = require("fs");
const net = require("net");
const { spawn } = require("child_process");

const SOCKET_PATH = "/run/claude-sandbox/vm-proxy.sock";

if (!fs.existsSync(SOCKET_PATH)) {
  process.stderr.write("vm: sandbox was not launched with --vm\n");
  process.exit(1);
}

function request(args, done) {
  const payload = JSON.stringify({ args, cwd: process.cwd() }) + "\n";
  const socket = net.createConnection(SOCKET_PATH, () => {
    socket.write(payload);
  });

  let data = "";

  socket.on("data", (chunk) => {
    data += chunk.toString();
  });

  socket.on("end", () => {
    try {
      done(JSON.parse(data.trim()));
    } catch (error) {
      process.stderr.write(
        "vm-proxy-client: failed to parse response: " + error.message + "\n",
      );
      process.exit(1);
    }
  });

  socket.on("error", (error) => {
    if (error.code === "ENOENT") {
      process.stderr.write("vm: sandbox was not launched with --vm\n");
    } else {
      process.stderr.write(
        "vm-proxy-client: connection error: " + error.message + "\n",
      );
    }
    process.exit(1);
  });
}

function finish(response) {
  if (response.stdout) {
    process.stdout.write(response.stdout);
  }
  if (response.stderr) {
    process.stderr.write(response.stderr);
  }
  process.exit(response.exit_code);
}

// `vm view NAME` is client-side sugar: ask the host for the SPICE socket, then
// open it full-screen on the sandbox's own X display.
function view(name) {
  request(["screen", name], (response) => {
    const uri = (response.stdout || "").trim();
    if (response.exit_code !== 0 || !uri.startsWith("spice+unix://")) {
      finish(response);
      return;
    }
    const viewer = spawn(
      "remote-viewer",
      ["--kiosk", "--kiosk-quit=on-disconnect", "--title", "vm-" + name, uri],
      { detached: true, stdio: "ignore" },
    );
    viewer.on("error", (error) => {
      process.stderr.write(
        "vm: could not start remote-viewer: " + error.message + "\n",
      );
      process.exit(1);
    });
    viewer.on("spawn", () => {
      viewer.unref();
      process.stdout.write(
        "viewer started for " + name + " on DISPLAY " +
          (process.env.DISPLAY || "(unset)") + "\n",
      );
      process.exit(0);
    });
  });
}

const args = process.argv.slice(2);
if (args[0] === "view" && args.length === 2) {
  view(args[1]);
} else {
  request(args, finish);
}
