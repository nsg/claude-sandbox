#!/usr/bin/env node

const crypto = require("node:crypto");
const http = require("node:http");
const { spawn } = require("node:child_process");

const portalPort = Number.parseInt(process.env.T3CODE_PAIR_ADMIN_PORT ?? "", 10);
const t3codePort = Number.parseInt(process.env.T3CODE_PORT ?? "", 10);
const baseDir = process.env.T3CODE_BASE_DIR;
const pin = process.env.T3CODE_PAIR_ADMIN_PIN ?? "";

if (
  !Number.isInteger(portalPort) ||
  !Number.isInteger(t3codePort) ||
  !baseDir ||
  !/^\d{4,12}$/.test(pin)
) {
  console.error(
    "T3CODE_PAIR_ADMIN_PORT, T3CODE_PORT, T3CODE_BASE_DIR, and a 4-12 digit T3CODE_PAIR_ADMIN_PIN are required",
  );
  process.exit(1);
}

const csrfToken = crypto.randomBytes(24).toString("base64url");
const sessionToken = crypto.randomBytes(32).toString("base64url");
const failedLogins = new Map();
const MAX_LOGIN_ATTEMPTS = 5;
const LOGIN_LOCKOUT_MS = 60_000;

function safeEqual(left, right) {
  const leftBuffer = Buffer.from(left);
  const rightBuffer = Buffer.from(right);
  return (
    leftBuffer.length === rightBuffer.length &&
    crypto.timingSafeEqual(leftBuffer, rightBuffer)
  );
}

function isAuthorized(request) {
  const cookies = request.headers.cookie?.split(";") ?? [];
  const sessionCookie = cookies
    .map((cookie) => cookie.trim().split("="))
    .find(([name]) => name === "pair_admin_session");
  return safeEqual(sessionCookie?.slice(1).join("=") ?? "", sessionToken);
}

function loginAttemptState(request) {
  const address = request.socket.remoteAddress ?? "unknown";
  const state = failedLogins.get(address);
  if (state?.blockedUntil > Date.now()) {
    return { address, blocked: true };
  }
  if (state?.blockedUntil) failedLogins.delete(address);
  return { address, blocked: false };
}

function recordFailedLogin(address) {
  const previous = failedLogins.get(address);
  const failures = (previous?.failures ?? 0) + 1;
  failedLogins.set(
    address,
    failures >= MAX_LOGIN_ATTEMPTS
      ? { failures: 0, blockedUntil: Date.now() + LOGIN_LOCKOUT_MS }
      : { failures, blockedUntil: 0 },
  );
}

async function readForm(request) {
  let body = "";
  for await (const chunk of request) {
    body += chunk;
    if (body.length > 4096) throw new Error("Request body too large");
  }
  return new URLSearchParams(body);
}

function requestHostname(request) {
  try {
    return new URL(`http://${request.headers.host}`).hostname;
  } catch {
    return "localhost";
  }
}

function escapeHtml(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function renderPage({ pairUrl, error, locked = false } = {}) {
  const result = pairUrl
    ? `<section class="result success">
        <span class="result-label">Link ready · expires in 5 minutes</span>
        <a class="primary" href="${escapeHtml(pairUrl)}">Pair this browser <span>↗</span></a>
        <p>The credential is single-use. Generate another link for a different browser.</p>
      </section>`
    : error
      ? `<section class="result error"><strong>${locked ? "Access denied" : "Could not create a link"}</strong><p>${escapeHtml(error)}</p></section>`
      : "";
  const action = locked
    ? `<form class="pin-form" method="post" action="/login">
        <label for="pin"><strong>Enter admin PIN</strong><span>Use the PIN configured when this service started.</span></label>
        <div class="pin-row">
          <input id="pin" name="pin" type="password" inputmode="numeric" pattern="[0-9]{4,12}" minlength="4" maxlength="12" autocomplete="current-password" autofocus required placeholder="••••••">
          <button type="submit">Unlock</button>
        </div>
      </form>`
    : `<form method="post" action="/pair">
        <input type="hidden" name="csrf" value="${csrfToken}">
        <div><strong>New browser session</strong><p>Valid for five minutes after creation.</p></div>
        <button type="submit">Create pairing link</button>
      </form>`;

  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <meta name="color-scheme" content="dark">
  <title>T3 Code · Pairing Desk</title>
  <style>
    :root { --ink:#f4f1e8; --muted:#9b9a91; --line:#353630; --acid:#d8ff4f; --bg:#11120f; }
    * { box-sizing:border-box; }
    body {
      margin:0; min-height:100vh; display:grid; place-items:center; color:var(--ink);
      background:
        linear-gradient(90deg,rgba(216,255,79,.035) 1px,transparent 1px) 0 0/56px 56px,
        linear-gradient(rgba(216,255,79,.035) 1px,transparent 1px) 0 0/56px 56px,
        radial-gradient(circle at 85% 15%,#293315 0,transparent 34%),var(--bg);
      font-family:"IBM Plex Mono","Courier New",monospace;
    }
    main { width:min(680px,calc(100% - 36px)); padding:54px 0; }
    header { border-top:1px solid var(--acid); padding-top:18px; margin-bottom:70px; }
    .eyebrow { color:var(--acid); font-size:12px; letter-spacing:.18em; text-transform:uppercase; }
    h1 { font-family:Georgia,serif; font-weight:400; font-size:clamp(46px,10vw,86px); line-height:.88; letter-spacing:-.055em; margin:20px 0 24px; }
    .lede { max-width:540px; color:var(--muted); font-size:14px; line-height:1.7; }
    form,.result { border:1px solid var(--line); background:rgba(17,18,15,.82); padding:24px; }
    form { display:flex; align-items:center; justify-content:space-between; gap:24px; }
    form p,.result p { color:var(--muted); font-size:12px; line-height:1.55; margin:5px 0 0; }
    .pin-form { display:block; }
    .pin-form label { display:flex; flex-direction:column; gap:6px; margin-bottom:18px; }
    .pin-form label span { color:var(--muted); font-size:12px; line-height:1.5; }
    .pin-row { display:grid; grid-template-columns:1fr auto; gap:12px; }
    input {
      min-width:0; width:100%; border:1px solid var(--line); outline:0; background:#0b0c09; color:var(--ink);
      padding:14px 16px; font:700 18px/1 "IBM Plex Mono","Courier New",monospace; letter-spacing:.3em;
    }
    input:focus { border-color:var(--acid); box-shadow:0 0 0 1px var(--acid); }
    button,.primary {
      appearance:none; border:0; cursor:pointer; white-space:nowrap; text-decoration:none;
      background:var(--acid); color:#15170d; padding:15px 18px; font:700 12px/1 "IBM Plex Mono","Courier New",monospace;
      letter-spacing:.06em; text-transform:uppercase; transition:transform .15s,box-shadow .15s;
    }
    button:hover,.primary:hover { transform:translate(-3px,-3px); box-shadow:5px 5px 0 #687a25; }
    .result { margin-top:14px; }
    .result-label { display:block; color:var(--acid); font-size:11px; margin-bottom:20px; text-transform:uppercase; letter-spacing:.1em; }
    .result .primary { display:flex; justify-content:space-between; align-items:center; width:100%; font-size:13px; }
    .result p { margin-top:18px; }
    .error { border-color:#8c3e35; }
    .error strong { color:#ff8c7e; }
    footer { display:flex; justify-content:space-between; margin-top:64px; color:#67685f; font-size:10px; text-transform:uppercase; letter-spacing:.12em; }
    @media (max-width:560px) { form:not(.pin-form) { align-items:stretch; flex-direction:column; } .pin-row { grid-template-columns:1fr; } button { width:100%; } footer { gap:12px; flex-direction:column; } }
  </style>
</head>
<body>
  <main>
    <header><span class="eyebrow">Private access terminal · ${portalPort}</span></header>
    <h1>Pairing<br>Desk.</h1>
    <p class="lede">${locked ? "Unlock the private pairing desk. Failed PIN attempts are temporarily limited." : "Create a short-lived key for this T3 Code environment. Nothing is generated until you ask, and every link works exactly once."}</p>
    ${action}
    ${result}
    <footer><span>T3 Code / Claude Sandbox</span><span>Private pairing surface</span></footer>
  </main>
</body>
</html>`;
}

function createPairingLink(hostname) {
  const pairBaseUrl = new URL(`http://${hostname}`);
  pairBaseUrl.port = String(t3codePort);

  return new Promise((resolve, reject) => {
    const child = spawn(
      "t3",
      [
        "auth", "pairing", "create",
        "--base-dir", baseDir,
        "--base-url", pairBaseUrl.toString(),
        "--ttl", "5m",
        "--label", "pair-admin",
        "--json",
      ],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("error", reject);
    child.on("close", (code) => {
      if (code !== 0) {
        reject(new Error(stderr.trim() || `t3 exited with status ${code}`));
        return;
      }
      try {
        const result = JSON.parse(stdout);
        if (typeof result.pairUrl !== "string") throw new Error("Pair URL missing");
        resolve(result.pairUrl);
      } catch {
        reject(new Error("T3 returned an invalid pairing response"));
      }
    });
  });
}

function sendHtml(response, status, body, extraHeaders = {}) {
  response.writeHead(status, {
    "Cache-Control": "no-store",
    "Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
    "Content-Type": "text/html; charset=utf-8",
    "Referrer-Policy": "no-referrer",
    "X-Content-Type-Options": "nosniff",
    ...extraHeaders,
  });
  response.end(body);
}

const server = http.createServer(async (request, response) => {
  if (request.method === "GET" && request.url === "/") {
    sendHtml(response, 200, renderPage({ locked: !isAuthorized(request) }));
    return;
  }

  if (request.method === "POST" && request.url === "/login") {
    const attempt = loginAttemptState(request);
    if (attempt.blocked) {
      sendHtml(
        response,
        429,
        renderPage({ locked: true, error: "Too many attempts. Wait one minute and try again." }),
      );
      return;
    }
    try {
      const form = await readForm(request);
      if (!safeEqual(form.get("pin") ?? "", pin)) {
        recordFailedLogin(attempt.address);
        sendHtml(response, 401, renderPage({ locked: true, error: "That PIN is not correct." }));
        return;
      }
      failedLogins.delete(attempt.address);
      response.writeHead(303, {
        "Cache-Control": "no-store",
        Location: "/",
        "Set-Cookie": `pair_admin_session=${sessionToken}; HttpOnly; SameSite=Strict; Path=/`,
      });
      response.end();
    } catch {
      sendHtml(response, 400, renderPage({ locked: true, error: "Invalid request." }));
    }
    return;
  }

  if (!isAuthorized(request)) {
    response.writeHead(303, { Location: "/" });
    response.end();
    return;
  }

  if (request.method === "POST" && request.url === "/pair") {
    let form;
    try {
      form = await readForm(request);
    } catch {
      sendHtml(response, 400, renderPage({ error: "Invalid request." }));
      return;
    }
    if (!safeEqual(form.get("csrf") ?? "", csrfToken)) {
      sendHtml(response, 403, renderPage({ error: "The form expired. Reload and try again." }));
      return;
    }
    try {
      const pairUrl = await createPairingLink(requestHostname(request));
      sendHtml(response, 200, renderPage({ pairUrl }));
    } catch (error) {
      console.error(`Pairing portal: ${error.message}`);
      sendHtml(response, 500, renderPage({ error: "The T3 server could not issue a token." }));
    }
    return;
  }

  response.writeHead(302, { Location: "/" });
  response.end();
});

server.listen(portalPort, "0.0.0.0", () => {
  console.error(`Pairing portal listening on http://0.0.0.0:${portalPort}`);
});
