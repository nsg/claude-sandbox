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

const FRAME_STDIN = 0;
const FRAME_STDOUT = 1;
const FRAME_STDERR = 2;
const FRAME_EXIT = 3;
const FRAME_STDIN_EOF = 4;
const FRAME_CHUNK = 32 * 1024;

function frame(kind, payload) {
  const header = Buffer.alloc(5);
  header[0] = kind;
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

// `vm pipe` keeps the connection open: stdin goes up as frames, and the
// guest command's stdout, stderr and exit code come back as frames.
function pipe(args) {
  const socket = net.createConnection(SOCKET_PATH);
  let pending = Buffer.alloc(0);
  let exited = false;

  const exit = (code) => {
    exited = true;
    process.stdin.pause();
    socket.destroy();
    process.stdout.write("", () => process.exit(code));
  };

  const sendStdinEof = () => {
    if (!socket.destroyed) {
      socket.write(frame(FRAME_STDIN_EOF, Buffer.alloc(0)));
    }
  };

  socket.on("connect", () => {
    socket.write(JSON.stringify({ args, cwd: process.cwd() }) + "\n");
    process.stdin.on("data", (chunk) => {
      let flowing = true;
      for (let offset = 0; offset < chunk.length; offset += FRAME_CHUNK) {
        const piece = chunk.subarray(offset, offset + FRAME_CHUNK);
        flowing = socket.write(frame(FRAME_STDIN, piece)) && flowing;
      }
      if (!flowing) {
        process.stdin.pause();
        socket.once("drain", () => process.stdin.resume());
      }
    });
    process.stdin.on("end", sendStdinEof);
    process.stdin.on("error", sendStdinEof);
  });

  socket.on("data", (chunk) => {
    pending = pending.length ? Buffer.concat([pending, chunk]) : chunk;
    while (pending.length >= 5) {
      const length = pending.readUInt32BE(1);
      if (pending.length < 5 + length) {
        break;
      }
      const kind = pending[0];
      const payload = pending.subarray(5, 5 + length);
      pending = pending.subarray(5 + length);
      if (kind === FRAME_STDOUT) {
        if (!process.stdout.write(payload)) {
          socket.pause();
          process.stdout.once("drain", () => socket.resume());
        }
      } else if (kind === FRAME_STDERR) {
        process.stderr.write(payload);
      } else if (kind === FRAME_EXIT && length === 4) {
        exit(payload.readInt32BE(0));
        return;
      }
    }
  });

  socket.on("close", () => {
    if (!exited) {
      process.stderr.write("vm: lost the connection to the VM bridge\n");
      exit(255);
    }
  });

  socket.on("error", (error) => {
    if (exited) {
      return;
    }
    exited = true;
    if (error.code === "ENOENT") {
      process.stderr.write("vm: sandbox was not launched with --vm\n");
      process.exit(1);
    }
    process.stderr.write("vm-proxy-client: connection error: " + error.message + "\n");
    process.exit(255);
  });
}

const args = process.argv.slice(2);
if (args[0] === "view" && args.length === 2) {
  view(args[1]);
} else if (args[0] === "pipe") {
  pipe(args);
} else {
  request(args, finish);
}
