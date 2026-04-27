#!/usr/bin/env node
"use strict";

const net = require("net");

const SOCKET_PATH = "/workspace/.claude-sandbox/ssh-proxy.sock";

const args = process.argv.slice(2);
const request = JSON.stringify({ args }) + "\n";

const socket = new net.Socket({ allowHalfOpen: true });

let handshakeDone = false;
let handshakeBuf = Buffer.alloc(0);
let frameBuf = Buffer.alloc(0);

socket.connect(SOCKET_PATH);

socket.on("connect", () => {
  socket.write(request);
});

socket.on("data", (chunk) => {
  if (!handshakeDone) {
    handshakeBuf = Buffer.concat([handshakeBuf, chunk]);
    const nlIndex = handshakeBuf.indexOf(0x0a);
    if (nlIndex === -1) return;

    const jsonBuf = handshakeBuf.slice(0, nlIndex);
    const remaining = handshakeBuf.slice(nlIndex + 1);
    handshakeBuf = Buffer.alloc(0);

    let response;
    try {
      response = JSON.parse(jsonBuf.toString("utf8"));
    } catch (e) {
      process.stderr.write(
        "ssh-proxy-client: failed to parse handshake: " + e.message + "\n"
      );
      process.exit(1);
    }

    if (response.status !== "ok") {
      process.stderr.write(
        "ssh-proxy: " +
          (response.reason ||
            "denied (ask the user to update ssh-proxy.json to allow this command)") +
          "\n"
      );
      process.exit(1);
    }

    handshakeDone = true;

    process.stdin.pipe(socket, { end: true });
    process.stdin.resume();

    if (remaining.length > 0) {
      processFrames(remaining);
    }
    return;
  }

  processFrames(chunk);
});

function processFrames(data) {
  frameBuf = Buffer.concat([frameBuf, data]);

  while (frameBuf.length >= 5) {
    const type = frameBuf[0];
    const length = frameBuf.readUInt32BE(1);

    if (frameBuf.length < 5 + length) break;

    const payload = frameBuf.slice(5, 5 + length);
    frameBuf = frameBuf.slice(5 + length);

    if (type === 1) {
      process.stdout.write(payload);
    } else if (type === 2) {
      process.stderr.write(payload);
    } else if (type === 0) {
      const exitCode = payload.readInt32BE(0);
      process.exit(exitCode);
    }
  }
}

socket.on("end", () => {
  process.exit(1);
});

socket.on("error", (err) => {
  if (err.code === "ENOENT" || err.code === "ECONNREFUSED") {
    process.stderr.write(
      "ssh-proxy: not running (configure ssh-proxy.json to enable it)\n"
    );
  } else {
    process.stderr.write(
      "ssh-proxy-client: connection error: " + err.message + "\n"
    );
  }
  process.exit(255);
});
