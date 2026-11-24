// End-to-end checks of lattice-gateway in front of a stand-in browser host.
// Build first: cargo build --manifest-path gateway/Cargo.toml
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";

const binary = process.env.LATTICE_GATEWAY_BINARY || new URL("../gateway/target/debug/lattice-gateway", import.meta.url).pathname;
const skip = existsSync(binary) ? false : `build the gateway first (${binary})`;

let upstream, gateway, dir, base, origin, token;
const seen = [];

function freePort() {
  return new Promise((resolve) => {
    const server = net.createServer().listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

async function waitFor(url) {
  for (let i = 0; i < 100; i++) {
    try { await fetch(url); return; } catch { await new Promise((r) => setTimeout(r, 50)); }
  }
  throw new Error(`${url} never answered`);
}

before(async () => {
  if (skip) return;
  dir = mkdtempSync(join(tmpdir(), "lattice-gateway-"));
  mkdirSync(join(dir, "open-requests"), { mode: 0o700 });
  mkdirSync(join(dir, "update"), { mode: 0o700 });
  mkdirSync(join(dir, "projects/paper"), { recursive: true });
  writeFileSync(join(dir, "projects/paper/main.tex"), "");
  const upstreamPort = await freePort();
  upstream = http.createServer((req, res) => {
    seen.push({ url: req.url, headers: req.headers });
    if (req.url === "/") {
      res.writeHead(200, { "Content-Type": "text/html" });
      return res.end('<!doctype html><html><head><script type="module" src="/assets/app.js"></script></head><body></body></html>');
    }
    if (req.url === "/assets/app.js") {
      res.writeHead(200, { "Content-Type": "text/javascript", ETag: '"lattice"' });
      return res.end("let r=new URL(`http://127.0.0.1:${e}/__lattice_session`);let w=new URL(`ws://127.0.0.1:${e.bridgePort}/__lattice_bridge`);n=window.location.hostname===`127.0.0.1`&&window.location.port===t;");
    }
    if (req.url === "/redirect") {
      res.writeHead(302, { Location: `http://127.0.0.1:${upstreamPort}/landed`, "Set-Cookie": "lattice=1; Path=/; Domain=127.0.0.1" });
      return res.end();
    }
    res.writeHead(200, { "Content-Type": "text/plain" });
    res.end(`lattice ${req.url}`);
  });
  upstream.on("upgrade", (req, socket) => {
    seen.push({ url: req.url, headers: req.headers });
    socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
    socket.on("data", (chunk) => socket.write(chunk));
  });
  await new Promise((r) => upstream.listen(upstreamPort, "127.0.0.1", r));
  const port = await freePort();
  base = `http://127.0.0.1:${port}`;
  origin = base;
  gateway = spawn(binary, [
    "--listen", `127.0.0.1:${port}`, "--public-origin", origin, "--upstream", `127.0.0.1:${upstreamPort}`,
    "--state-dir", join(dir, "state"), "--token-file", join(dir, "access-token"), "--insecure-dev-cookies",
    "--rewrite-loopback-page", "--open-requests", join(dir, "open-requests"), "--projects-root", join(dir, "projects"),
    "--update-dir", join(dir, "update"),
  ], { stdio: ["ignore", "inherit", "inherit"] });
  await waitFor(`${base}/__gateway/health`);
  token = readFileSync(join(dir, "access-token"), "utf8").trim();
});

after(() => {
  gateway?.kill();
  upstream?.close();
  if (dir) rmSync(dir, { recursive: true, force: true });
});

const post = (path, body, cookie, from = origin) => fetch(`${base}${path}`, {
  method: "POST",
  headers: { "Content-Type": "application/json", Origin: from, ...(cookie ? { Cookie: cookie } : {}) },
  body: JSON.stringify(body),
});
const cookieOf = (response) => response.headers.get("set-cookie").split(";")[0];

test("creates a private owner token and turns strangers away", { skip }, async () => {
  assert.equal(statSync(join(dir, "access-token")).mode & 0o777, 0o600);
  const page = await fetch(`${base}/project?x=1`, { headers: { Accept: "text/html" }, redirect: "manual" });
  assert.equal(page.status, 303);
  assert.equal(page.headers.get("location"), "/__gateway/login?next=/project%3Fx%3D1");
  const api = await fetch(`${base}/api/anything`);
  assert.equal(api.status, 401);
  assert.equal(seen.length, 0, "nothing reached Lattice");
});

test("owner signs in with the token and reaches Lattice through the gateway", { skip }, async () => {
  assert.equal((await post("/__gateway/api/login", { token: "wrong".repeat(10) })).status, 401);
  const login = await post("/__gateway/api/login", { token });
  assert.equal(login.status, 200);
  const owner = cookieOf(login);
  const response = await fetch(`${base}/hello`, { headers: { Cookie: `${owner}; app=kept`, Origin: origin } });
  assert.equal(await response.text(), "lattice /hello");
  const forwarded = seen.at(-1).headers;
  assert.equal(forwarded.cookie, "app=kept", "the gateway's own cookie stays with the gateway");
  assert.match(forwarded.origin, /^http:\/\/127\.0\.0\.1:\d+$/);
  const redirect = await fetch(`${base}/redirect`, { headers: { Cookie: owner }, redirect: "manual" });
  assert.equal(redirect.headers.get("location"), `${origin}/landed`);
  assert.equal(redirect.headers.get("set-cookie"), "lattice=1; Path=/");
});

test("rejects requests that come from another site", { skip }, async () => {
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const before = seen.length;
  const response = await fetch(`${base}/hello`, { headers: { Cookie: owner, Origin: "https://evil.example" } });
  assert.equal(response.status, 403);
  assert.equal((await post("/__gateway/api/invites", {}, owner, "https://evil.example")).status, 403);
  assert.equal(seen.length, before);
});

function openTunnel(cookie) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(new URL(base).port, "127.0.0.1");
    let buffer = "";
    socket.on("data", function onData(chunk) {
      buffer += chunk;
      if (buffer.includes("\r\n\r\n")) {
        socket.off("data", onData);
        resolve({ socket, head: buffer });
      }
    });
    socket.on("error", reject);
    socket.write(`GET /__lattice_bridge HTTP/1.1\r\nHost: ${new URL(base).host}\r\nOrigin: ${origin}\r\nCookie: ${cookie}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n`);
  });
}

test("pairs a device once, tunnels its WebSocket, and cuts it off on revoke", { skip }, async () => {
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const invite = await (await post("/__gateway/api/invites", {}, owner)).json();
  assert.match(invite.url, /\/__gateway\/pair#[0-9a-f]{48}$/);
  assert.match(invite.qrSvg, /^<\?xml|<svg/);
  const code = invite.url.split("#")[1];
  const paired = await post("/__gateway/api/redeem", { code, label: "Test phone" });
  assert.equal(paired.status, 200);
  const device = cookieOf(paired);
  assert.equal((await post("/__gateway/api/redeem", { code, label: "Again" })).status, 410, "an invite works once");

  const who = await (await fetch(`${base}/__gateway/api/whoami`, { headers: { Cookie: device } })).json();
  assert.deepEqual(who, { kind: "device", label: "Test phone" });
  assert.equal((await fetch(`${base}/__gateway/api/devices`, { headers: { Cookie: device } })).status, 401, "a device cannot manage devices");

  const { socket, head } = await openTunnel(device);
  assert.match(head, /^HTTP\/1\.1 101/);
  const echoed = new Promise((resolve) => socket.once("data", (chunk) => resolve(String(chunk))));
  socket.write("ping");
  assert.equal(await echoed, "ping");

  const { devices } = await (await fetch(`${base}/__gateway/api/devices`, { headers: { Cookie: owner } })).json();
  const closed = new Promise((resolve) => socket.once("close", resolve));
  assert.equal((await post(`/__gateway/api/devices/${devices[0].id}/revoke`, {}, owner)).status, 200);
  await closed;
  assert.equal((await fetch(`${base}/hello`, { headers: { Cookie: device } })).status, 401);
});

test("adapts Lattice's page to the public origin and counts each rewrite", { skip }, async () => {
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const html = await (await fetch(`${base}/`, { headers: { Cookie: owner } })).text();
  assert.match(html, /<head><script src="\/__gateway\/bridge\.js"><\/script><link rel="icon"/);
  assert.match(html, /data-lattice-gateway="brand"><script type="module"/);
  const script = await fetch(`${base}/assets/app.js`, { headers: { Cookie: owner, "Accept-Encoding": "gzip" } });
  assert.equal(script.headers.get("etag"), null, "a rewritten script drops Lattice's ETag");
  const text = await script.text();
  assert.doesNotMatch(text, /127\.0\.0\.1/);
  assert.match(text, /new URL\(`\$\{location\.origin\}\/__lattice_session`\)/);
  assert.match(text, /n=!0&&!0;/);
  assert.equal(seen.at(-1).headers["accept-encoding"], undefined, "Lattice is asked for an uncompressed script");
  const health = await (await fetch(`${base}/__gateway/health`)).json();
  assert.deepEqual(health.rewrites, { http: 1, ws: 1, protocol: 0, hostname: 1, port: 1 });
  const bridge = await fetch(`${base}/__gateway/bridge.js`);
  assert.match(bridge.headers.get("content-type"), /javascript/);
});

test("hands the address Lattice asked to open to one signed-in tab, once", { skip }, async () => {
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  assert.equal((await post("/__gateway/api/open-request", {})).status, 401, "strangers cannot claim it");
  assert.equal((await post("/__gateway/api/open-request", {}, owner)).status, 204);
  writeFileSync(join(dir, "open-requests/1.url"), "http://127.0.0.1:18452/?entry=n0nce_1\n");
  const claimed = await post("/__gateway/api/open-request", {}, owner);
  assert.deepEqual(await claimed.json(), { url: "/?entry=n0nce_1" });
  assert.equal((await post("/__gateway/api/open-request", {}, owner)).status, 204);
  const projects = await (await fetch(`${base}/__gateway/api/projects`, { headers: { Cookie: owner } })).json();
  assert.deepEqual(projects.projects.map((p) => [p.name, p.hasTex]), [["paper", true]]);
  // The page script needs the folder to tell a deleted project from one elsewhere.
  assert.equal(typeof projects.root, "string");
  assert.ok(projects.projects.every((p) => p.path.startsWith(`${projects.root.replace(/\/+$/, "")}/`)));
  assert.equal((await fetch(`${base}/__gateway/api/projects`)).status, 401);
});

test("the gateway adds no pages beyond sign-in and pairing", { skip }, async () => {
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const config = await (await fetch(`${base}/__gateway/api/config`, { headers: { Cookie: owner } })).json();
  assert.deepEqual(config, { agentOrigin: null, tex: false });
  assert.equal((await fetch(`${base}/__gateway/api/config`)).status, 401);
  for (const removed of ["projects", "screen", "terminal", "tex"]) {
    assert.equal((await fetch(`${base}/__gateway/${removed}`, { headers: { Cookie: owner } })).status, 404, removed);
  }
  assert.equal((await post("/__gateway/api/agent-port", { port: 1 }, owner)).status, 404);
  assert.equal((await post("/__gateway/api/tex-install", { name: "lipsum" }, owner)).status, 404);
});

test("a forged Tailscale identity over TCP is ignored", { skip }, async () => {
  const forged = await fetch(`${base}/__gateway/api/whoami`, {
    headers: { "Tailscale-User-Login": "owner@example.com", "X-Lattice-Gateway-Tailnet": "1" },
  });
  assert.deepEqual(await forged.json(), { kind: "anonymous" });
});

test("only the owner sees the update on offer and starts it", { skip }, async () => {
  const update = join(dir, "update");
  assert.equal((await fetch(`${base}/__gateway/api/update`)).status, 401);
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const get = async (cookie = owner) => (await fetch(`${base}/__gateway/api/update`, { headers: { Cookie: cookie } })).json();
  assert.equal((await get()).available, false, "nothing found yet");

  writeFileSync(join(update, "available.json"), JSON.stringify({ latest: "99.0.0", tag: "v99.0.0", lattice: { current: "0.19.0", latest: "0.20.0" } }));
  const state = await get();
  assert.equal(state.available, true);
  assert.equal(state.latest, "99.0.0");
  assert.equal(state.lattice.latest, "0.20.0");

  // A paired device cannot start or dismiss it.
  const invite = await (await post("/__gateway/api/invites", {}, owner)).json();
  const device = cookieOf(await post("/__gateway/api/redeem", { code: invite.url.split("#")[1], label: "Update phone" }));
  assert.equal((await fetch(`${base}/__gateway/api/update`, { headers: { Cookie: device } })).status, 401);
  assert.notEqual((await post("/__gateway/api/update/start", { version: "99.0.0" }, device)).status, 202);
  assert.equal(existsSync(join(update, "request")), false);

  // Another site cannot either, even with the owner's cookie.
  assert.equal((await post("/__gateway/api/update/start", { version: "99.0.0" }, owner, "https://evil.example")).status, 403);

  assert.equal((await post("/__gateway/api/update/dismiss", { version: "99.0.0" }, owner)).status, 202);
  assert.equal((await get()).dismissed, true);
  assert.equal((await post("/__gateway/api/update/start", { version: "98.0.0" }, owner)).status, 409, "only the version on offer");
  assert.equal((await post("/__gateway/api/update/start", { version: "99.0.0" }, owner)).status, 202);
  assert.equal(readFileSync(join(update, "request"), "utf8"), "99.0.0");
  assert.equal(statSync(join(update, "request")).mode & 0o777, 0o600);
  assert.equal((await get()).running, true);
  assert.equal((await post("/__gateway/api/update/start", { version: "99.0.0" }, owner)).status, 409, "one run at a time");
});

test("Phone and app: the owner's address code, the one-time hint, and the devices address", { skip }, async () => {
  assert.equal((await fetch(`${base}/__gateway/api/phone`)).status, 401);
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const get = async () => (await fetch(`${base}/__gateway/api/phone`, { headers: { Cookie: owner } })).json();
  const phone = await get();
  assert.equal(phone.address, origin);
  assert.match(phone.qrSvg, /<svg/);
  assert.equal(phone.signedIn, true, "signed in with the token, so the dialog offers Sign out");
  assert.equal(phone.hintDismissed, false);
  assert.equal((await post("/__gateway/api/phone/hint", {}, owner)).status, 200);
  assert.equal((await get()).hintDismissed, true, "the hint is answered once, for good");

  const asOwner = await fetch(`${base}/__gateway/devices`, { headers: { Cookie: owner }, redirect: "manual" });
  assert.equal(asOwner.status, 303);
  assert.equal(asOwner.headers.get("location"), "/?lattice-gateway=phone");
  const stranger = await fetch(`${base}/__gateway/devices`, { redirect: "manual" });
  assert.match(stranger.headers.get("location"), /^\/__gateway\/login\?next=/);

  const printed = spawnSync(binary, ["--print-qr", "https://w.example.ts.net:8443/"], { encoding: "utf8" });
  assert.equal(printed.status, 0);
  assert.match(printed.stdout, /[█▀▄]/);
});
