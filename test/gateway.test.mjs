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
import zlib from "node:zlib";

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
    if (req.url.includes("frames=1")) return echoFrames(socket);
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
  ], { stdio: ["ignore", "inherit", "inherit"], env: { ...process.env, HOME: dir } });
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

// A WebSocket frame: masked (as a browser sends it) when `mask` is given.
function wsFrame(opcode, payload, { fin = true, rsv1 = false, mask } = {}) {
  const len = payload.length;
  const head = [(fin ? 0x80 : 0) | (rsv1 ? 0x40 : 0) | opcode];
  const bit = mask ? 0x80 : 0;
  if (len < 126) head.push(bit | len);
  else if (len < 65536) head.push(bit | 126, len >> 8, len & 255);
  else {
    const big = Buffer.alloc(8);
    big.writeBigUInt64BE(BigInt(len));
    head.push(bit | 127, ...big);
  }
  const body = Buffer.from(payload);
  if (mask) {
    head.push(...mask);
    for (let i = 0; i < body.length; i++) body[i] ^= mask[i % 4];
  }
  return Buffer.concat([Buffer.from(head), body]);
}

// Whole frames at the front of `buffer`, and the bytes left over.
function parseFrames(buffer) {
  const frames = [];
  for (;;) {
    if (buffer.length < 2) break;
    let len = buffer[1] & 0x7f, at = 2;
    if (len === 126) { if (buffer.length < 4) break; len = buffer.readUInt16BE(2); at = 4; }
    else if (len === 127) { if (buffer.length < 10) break; len = Number(buffer.readBigUInt64BE(2)); at = 10; }
    const mask = buffer[1] & 0x80 ? buffer.subarray(at, at + 4) : null;
    if (mask) at += 4;
    if (buffer.length < at + len) break;
    const payload = Buffer.from(buffer.subarray(at, at + len));
    if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
    frames.push({ fin: !!(buffer[0] & 0x80), rsv1: !!(buffer[0] & 0x40), opcode: buffer[0] & 0x0f, masked: !!mask, payload });
    buffer = buffer.subarray(at + len);
  }
  return { frames, rest: buffer };
}

// A stand-in for Lattice's bridge that speaks WebSocket: it echoes each
// message unmasked and uncompressed, answers pings, and records what it got.
const upstreamFrames = [];
function echoFrames(socket) {
  let buffer = Buffer.alloc(0), message = null;
  socket.on("data", (chunk) => {
    const parsed = parseFrames(Buffer.concat([buffer, chunk]));
    buffer = parsed.rest;
    for (const frame of parsed.frames) {
      upstreamFrames.push(frame);
      if (frame.opcode === 0x9) socket.write(wsFrame(0xa, frame.payload));
      else if (frame.opcode === 0x8) { socket.write(wsFrame(0x8, frame.payload)); socket.end(); }
      else {
        message = message ? { ...message, data: [...message.data, frame.payload] } : { opcode: frame.opcode, data: [frame.payload] };
        if (frame.fin) { socket.write(wsFrame(message.opcode, Buffer.concat(message.data))); message = null; }
      }
    }
  });
}

// A raw WebSocket client: `extensions` is the Sec-WebSocket-Extensions offer.
function openSocket(cookie, path, extensions) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(new URL(base).port, "127.0.0.1");
    let buffer = Buffer.alloc(0);
    socket.on("data", function onData(chunk) {
      buffer = Buffer.concat([buffer, chunk]);
      const end = buffer.indexOf("\r\n\r\n");
      if (end < 0) return;
      socket.off("data", onData);
      const client = { socket, head: String(buffer.subarray(0, end)), wire: 0, frames: [], waiters: [] };
      let rest = buffer.subarray(end + 4);
      const take = (chunk) => {
        client.wire += chunk.length;
        const parsed = parseFrames(Buffer.concat([rest, chunk]));
        rest = parsed.rest;
        client.frames.push(...parsed.frames);
        client.waiters = client.waiters.filter((w) => !w());
      };
      socket.on("data", take);
      if (rest.length) { const early = rest; rest = Buffer.alloc(0); take(early); }
      client.next = (count) => new Promise((done) => {
        const check = () => client.frames.length >= count && (done(client.frames.splice(0, count)), true);
        if (!check()) client.waiters.push(check);
      });
      resolve(client);
    });
    socket.on("error", reject);
    socket.write(`GET ${path} HTTP/1.1\r\nHost: ${new URL(base).host}\r\nOrigin: ${origin}\r\nCookie: ${cookie}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n${extensions ? `Sec-WebSocket-Extensions: ${extensions}\r\n` : ""}\r\n`);
  });
}

const SYNC_TAIL = Buffer.from([0, 0, 0xff, 0xff]);
const deflateMessage = (data) => {
  const out = zlib.deflateRawSync(data, { finishFlush: zlib.constants.Z_SYNC_FLUSH });
  assert.deepEqual(out.subarray(-4), SYNC_TAIL);
  return out.subarray(0, -4);
};
const mask = [0x37, 0xfa, 0x21, 0x3d];

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

test("Open panels browse the home folder, make folders and take uploads, signed in only", { skip }, async () => {
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  writeFileSync(join(dir, "projects/refs.bib"), "@book{x}");
  writeFileSync(join(dir, "projects/notes.txt"), "x");
  assert.equal((await post("/__gateway/api/files", { start: true })).status, 401);
  assert.equal((await post("/__gateway/api/files", { start: true }, owner, "https://evil.example")).status, 403);

  // It starts in the projects folder, lists folders then the files asked for.
  const start = await (await post("/__gateway/api/files", { start: true, files: true, extensions: ["bib"] }, owner)).json();
  assert.equal(start.path, join(dir, "projects"));
  assert.deepEqual(start.entries.map((e) => `${e.kind}:${e.name}`), ["folder:paper", "file:refs.bib"]);
  assert.equal(start.entries[0].project, true);
  assert.equal((await post("/__gateway/api/files", { path: "/etc" }, owner)).status, 400, "nothing outside home");
  assert.equal((await post("/__gateway/api/files", { path: join(dir, "projects/../..") }, owner)).status, 400);

  const made = await post("/__gateway/api/files/folder", { path: join(dir, "projects"), name: "thesis" }, owner);
  assert.equal(made.status, 200);
  assert.equal((await made.json()).path, join(dir, "projects/thesis"));
  assert.equal((await post("/__gateway/api/files/folder", { path: join(dir, "projects"), name: "../x" }, owner)).status, 400);

  // An upload lands in the folder shown, under a free name.
  const upload = (name, body, folder = join(dir, "projects"), from = origin, cookie = owner) => fetch(`${base}/__gateway/api/upload`, {
    method: "POST", body,
    headers: { Origin: from, Cookie: cookie, "X-Upload-Folder": encodeURIComponent(folder), "X-Upload-Name": encodeURIComponent(name) },
  });
  const sent = await upload("refs.bib", "@book{y}");
  assert.equal(sent.status, 200);
  const { path } = await sent.json();
  assert.equal(path, join(dir, "projects/refs (2).bib"));
  assert.equal(readFileSync(path, "utf8"), "@book{y}");
  assert.equal((await upload("论文.zip", "PK")).status, 200);
  assert.ok(existsSync(join(dir, "projects/论文.zip")));
  assert.equal((await upload("x.zip", "PK", "/tmp")).status, 400, "only inside home");
  assert.equal((await upload("../x.zip", "PK")).status, 400);
  assert.equal((await upload("x.zip", "PK", join(dir, "projects"), "https://evil.example")).status, 403);
  assert.equal((await upload("x.zip", "PK", join(dir, "projects"), origin, "")).status, 401);
  assert.ok(!existsSync(join(dir, "projects/x.zip")));
});

test("Check now: only the owner asks lattice-host to look again", { skip }, async () => {
  const update = join(dir, "update");
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const invite = await (await post("/__gateway/api/invites", {}, owner)).json();
  const device = cookieOf(await post("/__gateway/api/redeem", { code: invite.url.split("#")[1], label: "Check phone" }));
  rmSync(join(update, "check"), { force: true });
  assert.notEqual((await post("/__gateway/api/update/check", {}, device)).status, 202);
  assert.equal((await post("/__gateway/api/update/check", {}, owner, "https://evil.example")).status, 403);
  assert.equal(existsSync(join(update, "check")), false);

  writeFileSync(join(update, "available.json"), JSON.stringify({ latest: "99.0.0", checkedAt: 1, releases: [{ version: "99.0.0", changes: ["A version page"] }, { version: "0.0.1" }] }));
  writeFileSync(join(update, "check-failed.json"), JSON.stringify({ error: "offline", at: 2 }));
  const asked = await post("/__gateway/api/update/check", {}, owner);
  assert.equal(asked.status, 202);
  const state = await asked.json();
  assert.equal(state.checking, true);
  assert.equal(state.checkFailed, null, "a new request clears the last failure");
  assert.deepEqual(state.releases.map((r) => r.version), ["99.0.0"], "only versions newer than this gateway");
  assert.equal(statSync(join(update, "check")).mode & 0o777, 0o600);
  rmSync(join(update, "check"));
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

test("compresses a WebSocket's browser leg with permessage-deflate, and leaves Lattice's leg plain", { skip }, async () => {
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const path = "/__lattice_bridge?frames=1";
  const client = await openSocket(owner, path, "permessage-deflate; client_max_window_bits");
  assert.match(client.head, /^HTTP\/1\.1 101/);
  assert.match(client.head, /\r\nsec-websocket-extensions: permessage-deflate\r?$/im);
  const asked = seen.findLast((s) => s.url === path).headers;
  assert.equal(asked["sec-websocket-extensions"], undefined, "Lattice is not offered the extension");

  // A bridge reply the size of a small PDF, base64 inside JSON.
  const pdf = Buffer.from(Array.from({ length: 300_000 }, (_, i) => (i * 7919) % 251));
  const big = Buffer.from(JSON.stringify({ id: 7, ok: true, result: { path: "main.pdf", data: pdf.toString("base64") } }));
  const plain = JSON.stringify(Array.from({ length: 2000 }, (_, i) => ({ id: i, method: "read_file", path: `chapters/${i % 9}.tex` })));
  upstreamFrames.length = 0;
  // Compressed from the browser, in two fragments, with a ping between them.
  const sent = deflateMessage(plain);
  const half = sent.length >> 1;
  client.socket.write(wsFrame(0x1, sent.subarray(0, half), { fin: false, rsv1: true, mask }));
  client.socket.write(wsFrame(0x9, Buffer.from("hi"), { mask }));
  client.socket.write(wsFrame(0x0, sent.subarray(half), { mask }));
  const [pong, echoed] = await client.next(2);
  assert.deepEqual([pong.opcode, String(pong.payload)], [0xa, "hi"], "the ping reaches Lattice and its pong comes back");
  const reached = upstreamFrames.filter((f) => f.opcode !== 0x9);
  assert.ok(upstreamFrames.every((f) => f.masked && !f.rsv1), "Lattice gets masked, uncompressed frames");
  assert.equal(Buffer.concat(reached.map((f) => f.payload)).toString(), plain, "inflated whole");
  assert.equal(echoed.rsv1, true, "the echo comes back compressed");
  assert.equal(echoed.masked, false);

  // A big message from Lattice, compressed on the way out; a small one is not.
  const before = client.wire;
  client.socket.write(wsFrame(0x1, deflateMessage(big), { rsv1: true, mask }));
  client.socket.write(wsFrame(0x1, Buffer.from("tiny"), { mask }));
  const [bigBack, tiny] = await client.next(2);
  const wire = client.wire - before;
  assert.equal(String(tiny.payload), "tiny");
  assert.equal(tiny.rsv1, false, "a message under 256 bytes goes out as it came");
  // The gateway keeps its deflate window across messages: inflate both in one stream.
  const inflated = zlib.inflateRawSync(Buffer.concat([echoed.payload, SYNC_TAIL, bigBack.payload, SYNC_TAIL]), { finishFlush: zlib.constants.Z_SYNC_FLUSH });
  assert.equal(inflated.toString(), plain + big.toString());
  assert.ok(wire < big.length / 1.25, `${wire} bytes on the wire for a ${big.length}-byte message`);

  // Close passes through both ways.
  client.socket.write(wsFrame(0x8, Buffer.from([0x03, 0xe8]), { mask }));
  const [closed] = await client.next(1);
  assert.deepEqual([closed.opcode, [...closed.payload]], [0x8, [0x03, 0xe8]]);
  await new Promise((resolve) => client.socket.once("close", resolve));
});

test("a browser that offers no extension, or one the gateway declines, gets a plain tunnel", { skip }, async () => {
  const owner = cookieOf(await post("/__gateway/api/login", { token }));
  const path = "/__lattice_bridge?frames=1";
  for (const offer of [undefined, "permessage-deflate; server_max_window_bits=9"]) {
    const client = await openSocket(owner, path, offer);
    assert.match(client.head, /^HTTP\/1\.1 101/);
    assert.doesNotMatch(client.head, /sec-websocket-extensions/i);
    assert.equal(seen.findLast((s) => s.url === path).headers["sec-websocket-extensions"], offer, "passed through as before");
    const text = "x".repeat(5000);
    client.socket.write(wsFrame(0x1, Buffer.from(text), { mask }));
    const [echoed] = await client.next(1);
    assert.deepEqual([echoed.rsv1, echoed.payload.toString()], [false, text]);
    client.socket.destroy();
  }
});
