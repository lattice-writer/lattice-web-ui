// Loaded by lattice-gateway before Lattice's own scripts on the workspace page.
// It adds nothing to the page; it only adapts Lattice to running on a remote host:
// 1. Lattice's "open in a browser" requests (a new workspace, the plugin's
//    Remote devices command) reach the host's opener; the gateway keeps them
//    for a minute and the tab the person was just using follows them.
// 2. Lattice's Agent runs on its own loopback port. The gateway serves it on a
//    second public origin; the address Lattice reports is swapped for that one.
// 3. Lattice has no one-click TeX package install on Linux. For the owner, its
//    install command is answered by the gateway, which runs tlmgr on the host.
// 4. On a phone, Lattice's columns get one panel with every view as a tab.
// 5. The host's projects folder appears in Lattice's recent projects, so they
//    open from Lattice's own project menu.
// 6. Lattice gives a workspace to one tab at a time: a tab opened on another
//    device takes it over. The tab it left keeps showing the workspace, and
//    takes it back as soon as it is used again.
// 7. The owner sees a new lattice-web-ui release as one of Lattice's own
//    notifications, and can update the host from it. Lattice's own "new Linux
//    version" notice is answered with nothing: lattice-web-ui pins Lattice.
//    Settings has a Version page too: the versions this host runs, what each
//    newer release brings, Update, and Check now.
// 8. The workspace carries lattice-web-ui's name and icon: in the browser tab,
//    on a phone's home screen, and on Lattice's welcome screen.
// 9. "Phone and app": how to open the workspace on a phone, install it as an
//    app, and manage paired devices, as a dialog built from Lattice's own
//    Settings parts.
// 10. The workspace fills the screen under a phone's status bar, keeps its
//     content in the safe area, and paints the strip in the colour at the top.
// 11. A project held by a Lattice window on the host's own display, which
//     nobody can see, is opened in the tab: that window is closed first. A tab
//     cannot hand its workspace to that display ("Open in Lattice app").
// 12. "Save … as" (a PDF, a review copy, a project archive) downloads the file
//     to this device, instead of asking for a place on the host's disk in a
//     panel on its display.
// 13. Lattice's message and confirm dialogs, and its Open panels (a folder, a
//     file to import), are shown in the tab, made like Lattice's own dialogs:
//     the Open panel browses the host's home folder through the gateway and
//     uploads a file from this device into the folder it shows.
// 14. The compiled PDF crosses the network once. Lattice reads it each time a
//     project is entered (a tab click, a reload) and again after each build,
//     the whole file every time. The tab keeps the PDFs it was sent (in
//     IndexedDB, so a reload has them too) and asks the gateway for the
//     file's fingerprint first: the same file costs one small request, and a
//     new one comes as plain HTTP, not base64 inside Lattice's WebSocket.
//     When the gateway cannot be sure which file Lattice reads, Lattice is
//     asked as before.
(() => {
  "use strict";

  const LOOPBACK_AGENT = /^http:\/\/127\.0\.0\.1:(\d{1,5})$/;
  let agentConfig;
  const agentOrigin = () => (agentConfig ??= fetch("/__gateway/api/config", { credentials: "same-origin", cache: "no-store" })
    .then((r) => (r.ok ? r.json() : {})).then((c) => c.agentOrigin || null).catch(() => null));
  async function publicAgent(info) {
    const match = info && typeof info.origin === "string" && LOOPBACK_AGENT.exec(info.origin);
    const origin = match && await agentOrigin();
    if (!origin) return info;
    const response = await fetch("/__gateway/api/agent-port", {
      method: "POST", credentials: "same-origin", cache: "no-store",
      headers: { "Content-Type": "application/json" }, body: JSON.stringify({ port: Number(match[1]) }),
    });
    return response.ok ? { ...info, origin } : info;
  }
  async function installTexDependency(args, fallback) {
    const name = (args && (args.package || args.missingFile)) || "";
    const response = await fetch("/__gateway/api/tex-install", {
      method: "POST", credentials: "same-origin", cache: "no-store",
      headers: { "Content-Type": "application/json" }, body: JSON.stringify({ name }),
    });
    // Not the owner, or no TeX the gateway may change: Lattice answers as it would.
    if (response.status === 401 || response.status === 404) return fallback();
    const value = await response.json().catch(() => ({}));
    if (response.ok) return { kind: "installed" };
    if (value?.error?.code === "choose_package") return { kind: "choosePackage", packages: value.error.packages };
    throw new Error(value?.error?.message || "Installing the TeX package failed.");
  }

  // A Lattice window on the host's own display is one nobody can see. When
  // the project asked for is shown in one, Lattice would only focus it, and
  // the tab would stay where it was. That window is closed the way Lattice
  // closes a window (it saves first), so the project opens in the tab.
  const zh = () => /^zh/i.test(document.documentElement.lang || navigator.language || "");
  const noDesktopMessage = () => zh()
    ? "这台主机上的 Lattice 应用没有能看到的屏幕，工作区留在这个标签页里。"
    : "The Lattice app on this host has no screen anyone can see, so the workspace stays in this tab.";
  let lastRelease = { path: null, at: 0 };
  async function hiddenWindowHolding(invoke, path) {
    if (typeof path !== "string" || !path) return null;
    const labels = await invoke("plugin:window|get_all_windows").catch(() => null);
    if (!Array.isArray(labels)) return null;
    // A window's title starts with its project's name: Lattice's own, or the folder's.
    const names = [String(path).replace(/\/+$/, "").split("/").pop()];
    try {
      const listed = JSON.parse(localStorage.getItem(RECENT) || "[]").find((item) => item?.path === path);
      if (typeof listed?.name === "string") names.unshift(listed.name);
    } catch { /* the folder's name is enough */ }
    for (const label of labels) {
      // Browser tabs' hosts and PDF windows never hold a project.
      if (typeof label !== "string" || label.startsWith("browser-") || label.startsWith("pdf-")) continue;
      const title = await invoke("plugin:window|title", { label }).catch(() => null);
      if (typeof title === "string" && names.some((name) => name && (title === name || title.startsWith(`${name} — `)))) return label;
    }
    return null;
  }
  async function releaseFromHiddenWindow(invoke, path) {
    // Tried a moment ago (the window would not close): Lattice answers as it would.
    if (lastRelease.path === path && Date.now() - lastRelease.at < 20000) return false;
    const label = await hiddenWindowHolding(invoke, path);
    if (!label) return false;
    lastRelease = { path, at: Date.now() };
    try { await invoke("plugin:window|close", { label }); } catch { return false; }
    for (let i = 0; i < 50; i++) {
      await new Promise((resolve) => setTimeout(resolve, 300));
      const shown = await invoke("project_shown_elsewhere", { path }).catch(() => null);
      if (shown === false) { lastRelease = { path: null, at: 0 }; return true; }
      if (shown === null) return false;
    }
    return false;
  }

  // Lattice's save panel would open on the host's display, which nobody sees,
  // and the file would stay on the host. The gateway gives a fresh folder for
  // the file instead; the command that writes it there names that path (in its
  // arguments, or base64 in a header for binary saves), and once it returns
  // the tab downloads the file.
  const downloads = [];
  const base64 = (text) => btoa(Array.from(new TextEncoder().encode(text), (b) => String.fromCharCode(b)).join(""));
  async function downloadDestination(args) {
    const options = args?.options || {};
    let name = String(options.defaultPath || "").split(/[\\/]/).pop() || "download";
    const extension = options.filters?.[0]?.extensions?.[0];
    if (extension && extension !== "*" && !name.toLowerCase().endsWith(`.${extension.toLowerCase()}`)) name += `.${extension}`;
    const response = await fetch("/__gateway/api/download", {
      method: "POST", credentials: "same-origin", cache: "no-store",
      headers: { "Content-Type": "application/json" }, body: JSON.stringify({ name }),
    });
    if (!response.ok) throw new Error(`download ${response.status}`);
    const { id, path } = await response.json();
    downloads.push({ id, path, encoded: base64(path), at: Date.now() });
    return path;
  }
  function downloadWrittenBy(args, options) {
    for (let i = downloads.length - 1; i >= 0; i--) if (Date.now() - downloads[i].at > 30 * 60 * 1000) downloads.splice(i, 1);
    if (!downloads.length) return null;
    const values = [];
    if (args && typeof args === "object" && !ArrayBuffer.isView(args) && !(args instanceof ArrayBuffer)) values.push(...Object.values(args));
    const headers = options?.headers;
    if (headers instanceof Headers) values.push(...headers.values());
    else if (headers && typeof headers === "object") values.push(...Object.values(headers));
    const index = downloads.findIndex((d) => values.some((v) => v === d.path || v === d.encoded));
    return index < 0 ? null : downloads.splice(index, 1)[0];
  }
  function startDownload({ id, path }) {
    const link = document.createElement("a");
    link.href = `/__gateway/download/${id}`;
    link.download = path.split("/").pop();
    link.hidden = true;
    document.body.append(link);
    link.click();
    link.remove();
  }

  // The compiled PDF, kept by project folder and fingerprint (sha256 and
  // size, the gateway's ETag for the file `read_compiled_pdf` reads). The
  // gateway answers 304 when the tab names the file's tag, the bytes when it
  // does not, and anything else (409: it cannot be sure which file Lattice
  // reads; 404: Lattice would find none) leaves the read to Lattice. Kept
  // bytes are checked against their tag before they are used, and a fresh
  // copy goes to Lattice each time: pdf.js may hand a buffer to its worker.
  const PDF_TAG = /^(?:W\/)?"sha256-([0-9a-f]{64})-(\d+)"$/;
  const PDF_CAP = 100 * 1024 * 1024, PDF_COUNT = 8, PDF_PER_PROJECT = 2, PDF_MEMORY = 48 * 1024 * 1024;
  const pdfKey = (root, tag) => `${root}\n${tag}`;
  const pdfMemory = new Map();
  let pdfDb, pdfIndex;
  function openPdfDb() {
    return (pdfDb ??= new Promise((resolve) => {
      try {
        const request = indexedDB.open("lattice-gateway-pdf", 1);
        request.onupgradeneeded = () => {
          request.result.createObjectStore("meta", { keyPath: "key" });
          request.result.createObjectStore("bytes");
        };
        request.onsuccess = () => {
          request.result.onversionchange = () => request.result.close();
          resolve(request.result);
        };
        request.onerror = request.onblocked = () => resolve(null);
      } catch { resolve(null); }
    }));
  }
  // One transaction over both stores; `work` gets them and a setter for the
  // result. Resolves once it commits, undefined when storage fails.
  async function pdfStores(mode, work) {
    const db = await openPdfDb();
    if (!db) return undefined;
    return new Promise((resolve) => {
      let result;
      try {
        const tx = db.transaction(["meta", "bytes"], mode);
        tx.oncomplete = () => resolve(result);
        tx.onerror = tx.onabort = () => resolve(undefined);
        work(tx.objectStore("meta"), tx.objectStore("bytes"), (value) => { result = value; });
      } catch { resolve(undefined); }
    });
  }
  const pdfIndexReady = () => (pdfIndex ??= pdfStores("readonly", (meta, _bytes, done) => {
    const all = meta.getAll();
    all.onsuccess = () => done(all.result);
  }).then((list) => new Map((Array.isArray(list) ? list : [])
    .filter((m) => m && typeof m.key === "string" && typeof m.root === "string" && PDF_TAG.test(m.tag))
    .map((m) => [m.key, m]))).catch(() => new Map()));
  function rememberPdf(key, bytes) {
    pdfMemory.delete(key);
    pdfMemory.set(key, bytes);
    let total = 0;
    for (const value of pdfMemory.values()) total += value.byteLength;
    for (const [old, value] of pdfMemory) {
      if (total <= PDF_MEMORY || old === key) break;
      pdfMemory.delete(old);
      total -= value.byteLength;
    }
  }
  async function pdfMatches(bytes, tag) {
    const [, sha, size] = PDF_TAG.exec(tag) || [];
    if (!sha || !(bytes instanceof ArrayBuffer) || bytes.byteLength !== Number(size)) return false;
    if (new TextDecoder().decode(new Uint8Array(bytes, 0, 5)) !== "%PDF-") return false;
    // Without WebCrypto (a page not served securely) the size has to do.
    if (!globalThis.crypto?.subtle) return true;
    const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
    return Array.from(digest, (b) => b.toString(16).padStart(2, "0")).join("") === sha;
  }
  async function keptPdf(key) {
    const held = pdfMemory.get(key) ?? await pdfStores("readonly", (_meta, bytes, done) => {
      const read = bytes.get(key);
      read.onsuccess = () => done(read.result);
    });
    if (!(held instanceof ArrayBuffer)) return null;
    rememberPdf(key, held);
    return held.slice(0);
  }
  // The newest per project, a few in all, the oldest dropped first.
  function evictPdfs(index) {
    const order = [...index.values()].sort((a, b) => b.at - a.at);
    const perProject = new Map();
    let total = 0, count = 0;
    const dropped = [];
    for (const m of order) {
      const mine = (perProject.get(m.root) || 0) + 1;
      perProject.set(m.root, mine);
      if (mine > PDF_PER_PROJECT || count + 1 > PDF_COUNT || total + m.size > PDF_CAP) { dropped.push(m.key); continue; }
      total += m.size;
      count += 1;
    }
    for (const key of dropped) { index.delete(key); pdfMemory.delete(key); }
    return dropped;
  }
  function keepPdf(index, root, tag, bytes) {
    const key = pdfKey(root, tag);
    const entry = { key, root, tag, size: bytes.byteLength, at: Date.now() };
    index.set(key, entry);
    const dropped = evictPdfs(index);
    if (dropped.includes(key)) return;
    rememberPdf(key, bytes);
    void pdfStores("readwrite", (meta, store) => {
      meta.put(entry);
      store.put(bytes, key);
      for (const old of dropped) { meta.delete(old); store.delete(old); }
    });
  }
  function touchPdf(index, root, tag) {
    const entry = index.get(pdfKey(root, tag));
    if (!entry) return;
    entry.at = Date.now();
    void pdfStores("readwrite", (meta) => { meta.put(entry); });
  }
  const fetchCompiledPdf = (root, tags) => fetch(`/__gateway/api/compiled-pdf?root=${encodeURIComponent(root)}`, {
    credentials: "same-origin", cache: "no-store",
    headers: tags.length ? { "If-None-Match": tags.join(", ") } : {},
  });
  async function compiledPdf(root, ask) {
    try {
      const index = await pdfIndexReady();
      const held = [...index.values()].filter((m) => m.root === root).sort((a, b) => b.at - a.at).map((m) => m.tag);
      let response = await fetchCompiledPdf(root, held);
      if (response.status === 304) {
        const tag = (response.headers.get("ETag") || "").replace(/^W\//, "");
        const bytes = held.includes(tag) ? await keptPdf(pdfKey(root, tag)) : null;
        if (bytes && await pdfMatches(bytes, tag)) {
          touchPdf(index, root, tag);
          return bytes;
        }
        // Gone from storage, or not what it should be: the file itself.
        index.delete(pdfKey(root, tag));
        pdfMemory.delete(pdfKey(root, tag));
        response = await fetchCompiledPdf(root, []);
      }
      if (response.status === 200) {
        const tag = (response.headers.get("ETag") || "").replace(/^W\//, "");
        const bytes = await response.arrayBuffer();
        if (await pdfMatches(bytes, tag)) {
          keepPdf(index, root, tag, bytes.slice(0));
          return bytes;
        }
      }
    } catch { /* Lattice answers */ }
    return ask();
  }

  // Message, confirm and Open dialogs shown in this tab (filled in below).
  const tabDialogs = { message: null, open: null };

  // Lattice installs its IPC adapter on window.__TAURI_INTERNALS__; wrap its invoke as it arrives.
  let internals;
  Object.defineProperty(window, "__TAURI_INTERNALS__", {
    configurable: true,
    get: () => internals,
    set(value) {
      if (value && typeof value.invoke === "function" && !value.__latticeGateway) {
        const invoke = value.invoke;
        value.invoke = async (command, args, options) => {
          if (command === "start_tex_dependency_install") {
            return installTexDependency(args, () => invoke(command, args, options));
          }
          // The Lattice app on the host has no screen anyone sees: the workspace stays in the tab.
          if (command === "return_to_desktop") throw noDesktopMessage();
          if (command === "project_shown_elsewhere") {
            const shown = await invoke(command, args, options);
            return shown === true && await releaseFromHiddenWindow(invoke, args?.path) ? false : shown;
          }
          if (command === "open_project") {
            // Reopening the last project when the page loads asks Lattice directly.
            try {
              return await invoke(command, args, options);
            } catch (error) {
              if (!/already open in another Lattice window/.test(String(error?.message ?? error))
                || !await releaseFromHiddenWindow(invoke, args?.path)) throw error;
              return invoke(command, args, options);
            }
          }
          if (command === "open_project_window") {
            const shown = await invoke("project_shown_elsewhere", { path: args?.path }).catch(() => false);
            if (shown === true) await releaseFromHiddenWindow(invoke, args?.path);
            return invoke(command, args, options);
          }
          if (command === "read_compiled_pdf" && typeof args?.projectRoot === "string" && args.projectRoot.startsWith("/")) {
            return compiledPdf(args.projectRoot, () => invoke(command, args, options));
          }
          if (command === "plugin:dialog|message" && tabDialogs.message) return tabDialogs.message(args);
          if (command === "plugin:dialog|open" && tabDialogs.open) return tabDialogs.open(args?.options);
          if (command === "plugin:dialog|save") {
            // Nothing chosen (as if cancelled) rather than a panel nobody sees.
            return downloadDestination(args).catch(() => null);
          }
          const download = downloadWrittenBy(args, options);
          if (download) {
            const result = await invoke(command, args, options);
            startDownload(download);
            // Lattice says "Saved to <path>": the path on the host means nothing here.
            return result === download.path ? download.path.split("/").pop() : result;
          }
          // Lattice reaches this host through lattice-web-ui's release, never a download.
          if (command === "check_linux_release") return null;
          const result = await invoke(command, args, options);
          return command === "synara_ensure_ready" ? publicAgent(result) : result;
        };
        value.__latticeGateway = true;
      }
      internals = value;
    },
  });
  let lastInput = 0;
  for (const type of ["pointerdown", "keydown"]) {
    addEventListener(type, () => { lastInput = Date.now(); }, { capture: true, passive: true });
  }

  // A project open this tab asked for may answer after a reload: follow it until then.
  const FOLLOW = "lattice-gateway.follow-until";
  const following = () => {
    try { return Number(sessionStorage.getItem(FOLLOW) || 0) > Date.now(); } catch { return false; }
  };
  async function poll() {
    if (document.visibilityState === "visible" && (Date.now() - lastInput < 20000 || following())) {
      try {
        const response = await fetch("/__gateway/api/open-request", {
          method: "POST", credentials: "same-origin", cache: "no-store",
          headers: { "Content-Type": "application/json" }, body: "{}",
        });
        if (response.status === 200) {
          const { url } = await response.json();
          if (url === "/__gateway/devices") {
            // The plugin's Phone and app command: a dialog in this tab, no page change.
            openPhoneGuide();
          } else if (typeof url === "string" && /^\/\?entry=[\w-]+$/.test(url)) {
            try { sessionStorage.removeItem(FOLLOW); } catch { /* storage unavailable */ }
            location.assign(url);
            return;
          }
        }
      } catch { /* the next poll retries */ }
    }
    setTimeout(poll, 1500);
  }
  setTimeout(poll, 1500);

  // Lattice covers a tab that another tab took the workspace from with a
  // "open in another browser tab" notice. Here that tab instead keeps showing
  // the workspace as it was, and takes the workspace back (by reloading, which
  // resumes the same session) when it is shown, focused, tapped or typed in.
  const REPLACED = new Set([
    "This Lattice workspace is open in another browser tab.",
    "此 Lattice 工作区已在另一个浏览器标签页中打开。",
  ]);
  let replaced = false;
  function reclaim(event) {
    if (!replaced || (event && event.isTrusted === false)) return;
    // Phone and app and Version talk to the gateway only; using them leaves the workspace where it is.
    if (event?.target?.closest?.("[data-gateway-dialog], [data-gateway-dialog-backdrop], [data-gateway-page]")) return;
    if (event?.type === "visibilitychange" && document.visibilityState !== "visible") return;
    if (event?.type === "focus" && event.target !== window) return;
    if (event?.cancelable) { event.preventDefault(); event.stopImmediatePropagation(); }
    replaced = false;
    location.reload();
  }
  function noticeReplaced() {
    const message = document.getElementById("lattice-browser-runtime-message");
    if (replaced || !message || !REPLACED.has(message.textContent.trim())) return;
    replaced = true;
    const overlay = document.getElementById("lattice-browser-runtime-error");
    if (overlay) {
      // Transparent, so the workspace stays in view; it still takes every tap.
      overlay.style.background = "transparent";
      overlay.dataset.latticeGatewayReclaim = "";
      for (const child of overlay.children) child.style.visibility = "hidden";
    }
  }
  // When the gateway restarts (an update), Lattice loses its connection and
  // shows "disconnected" with a Reload button. Lattice keeps the workspace for
  // five seconds, so reload as soon as the gateway answers again: the tab
  // resumes its own workspace instead of starting a new one.
  const DISCONNECTED = new Set([
    "The local Lattice app disconnected.",
    "与本地 Lattice 应用的连接已断开。",
  ]);
  let reconnecting = false;
  function noticeDisconnected() {
    const message = document.getElementById("lattice-browser-runtime-message");
    if (reconnecting || !message || !DISCONNECTED.has(message.textContent.trim())) return;
    const STAMP = "lattice-gateway.reconnected-at";
    let last = 0;
    try { last = Number(sessionStorage.getItem(STAMP) || 0); } catch { /* storage unavailable */ }
    if (Date.now() - last < 15000) return; // reloaded a moment ago: leave the Reload button to the person
    reconnecting = true;
    const deadline = Date.now() + 120000;
    const poll = async () => {
      try {
        const health = await (await fetch("/__gateway/health", { cache: "no-store" })).json();
        if (health.lattice) {
          try { sessionStorage.setItem(STAMP, String(Date.now())); } catch { /* storage unavailable */ }
          location.reload();
          return;
        }
      } catch { /* still restarting */ }
      if (Date.now() < deadline) setTimeout(poll, 400);
    };
    poll();
  }
  new MutationObserver(() => { noticeReplaced(); noticeDisconnected(); }).observe(document, { childList: true, subtree: true });
  for (const type of ["pointerdown", "keydown"]) addEventListener(type, reclaim, { capture: true });
  addEventListener("focus", reclaim);
  document.addEventListener("visibilitychange", reclaim);

  // Lattice lists up to eight recent projects in its project menu and reads that
  // list once, when it starts. The host's projects are fetched right away and
  // merged into what Lattice reads, so each can be opened from its own menu.
  const RECENT = "lattice.recent-projects.v1";
  let hostProjects = [];
  let hostFolder = null;
  let hostPaths = new Set();
  fetch("/__gateway/api/projects", { credentials: "same-origin", cache: "no-store" })
    .then((response) => (response.ok ? response.json() : { projects: [] }))
    .then(({ root, projects }) => {
      hostFolder = typeof root === "string" ? root.replace(/\/+$/, "") + "/" : null;
      hostPaths = new Set(projects.map((project) => project.path));
      hostProjects = projects.filter((project) => project.hasTex);
    })
    .catch(() => {});
  function withHostProjects(stored) {
    if (hostFolder === null) return stored;
    let recent = [];
    try { recent = JSON.parse(stored || "[]"); } catch { return stored; }
    if (!Array.isArray(recent)) return stored;
    // Lattice's own list comes first, in its order, with projects outside the
    // projects folder too (an Overleaf project, say): its first entry is the
    // project a new workspace opens.
    // A project deleted from the projects folder leaves the list; one outside
    // that folder stays, since only Lattice knows whether it still exists.
    const gone = (path) => hostFolder !== null && path.startsWith(hostFolder) && !hostPaths.has(path);
    const kept = recent.filter((item) => item && typeof item.path === "string" && !gone(item.path));
    const known = new Set(kept.map((item) => item.path));
    const added = hostProjects.filter((project) => !known.has(project.path)).map(({ name, path }) => ({ name, path }));
    return JSON.stringify([...kept, ...added].slice(0, 8));
  }
  const getItem = Storage.prototype.getItem;
  Storage.prototype.getItem = function (key) {
    const value = getItem.call(this, key);
    return this === localStorage && key === RECENT ? withHostProjects(value) : value;
  };

  const LAYOUT = "lattice.trellis-layout.v1:";
  const WIDE = "lattice-gateway.wide-layout:";
  const PHONE_PANEL = "panel-phone";
  function panelViews(node, out = []) {
    if (!node) return out;
    if (node.kind === "panel") out.push(...node.views);
    else (node.children || []).forEach((child) => panelViews(child, out));
    return out;
  }
  function adaptLayouts() {
    const narrow = matchMedia("(max-width: 700px)").matches;
    const wide = matchMedia("(min-width: 900px)").matches;
    let changed = false;
    for (const key of Object.keys(localStorage).filter((k) => k.startsWith(LAYOUT))) {
      let saved;
      try { saved = JSON.parse(localStorage.getItem(key)); } catch { continue; }
      const root = saved?.document?.root;
      if (!root) continue;
      const isPhone = root.kind === "panel" && root.id === PHONE_PANEL;
      if (narrow && !isPhone) {
        const views = panelViews(root);
        const files = views.filter((v) => v.startsWith("file-"));
        const order = [...files, ...views.filter((v) => v === "pdf"), ...views.filter((v) => !files.includes(v) && v !== "pdf")];
        if (!order.length) continue;
        localStorage.setItem(WIDE + key, JSON.stringify(root));
        saved.document.root = { kind: "panel", id: PHONE_PANEL, views: order, selected: files[0] || order[0] };
        saved.savedAt = Date.now();
        localStorage.setItem(key, JSON.stringify(saved));
        changed = true;
      } else if (wide && isPhone) {
        let previous = null;
        try { previous = JSON.parse(localStorage.getItem(WIDE + key) || "null"); } catch { previous = null; }
        if (!previous) continue;
        // Views opened on the phone stay: they are added to the panel that had the first file.
        const known = new Set(panelViews(previous));
        const added = root.views.filter((v) => !known.has(v));
        if (added.length) {
          (function addTo(node) {
            if (node.kind === "panel") { if (node.views.some((v) => v.startsWith("file-")) || !added.length) { node.views.push(...added); added.length = 0; } return; }
            (node.children || []).forEach(addTo);
          })(previous);
        }
        saved.document.root = previous;
        saved.savedAt = Date.now();
        localStorage.setItem(key, JSON.stringify(saved));
        localStorage.removeItem(WIDE + key);
        changed = true;
      }
    }
    return changed;
  }
  (async () => {
    const deadline = Date.now() + 30000;
    while (!window.__LATTICE_BROWSER_RUNTIME__) {
      if (Date.now() > deadline) return;
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
    // After the host's copy of localStorage has arrived.
    await new Promise((resolve) => setTimeout(resolve, 1500));
    const marker = "lattice-gateway.layout-reloaded";
    if (adaptLayouts() && sessionStorage.getItem(marker) !== String(innerWidth < 800)) {
      sessionStorage.setItem(marker, String(innerWidth < 800));
      // Give the change a moment to reach the host before the page reads it again.
      setTimeout(() => location.reload(), 800);
      return;
    }

  })();

  // Lattice-style notifications, shared by the update prompt and the phone hint.
  const toastApi = { show: () => {}, close: () => {}, busy: () => false };
  let openPhoneGuide = () => {};
  // The update prompt's state and its Update, shared with the Version page.
  const updateApi = { state: null, start: () => {}, apply: () => {}, listeners: new Set() };
  // Lattice's interface language: its saved choice, else the browser's.
  const zhUI = (() => {
    try {
      const saved = JSON.parse(localStorage.getItem("lattice.appearance.v5") || "{}").interfaceLanguage;
      if (saved === "zh-CN" || saved === "en") return saved === "zh-CN";
    } catch { /* fall back to the browser */ }
    return /^zh\b/i.test(navigator.language || "");
  })();

  // The owner's update prompt, built from the markup and classes of Lattice's
  // own notifications (src/telemetry/toast-stack.tsx), so it looks like one.
  (() => {
    const T = zhUI ? {
      available: (v) => `Lattice-web-ui ${v} 可以更新`,
      brings: (l) => `包含 Lattice ${l}。`,
      takes: "更新约一分钟，期间工作区会重新连接。",
      update: "更新", later: "稍后", dismiss: "关闭通知", ok: "好",
      running: (v) => `正在更新到 ${v}…`,
      phase: { requested: "正在开始…", downloading: "正在下载并校验新版本…", installing: "正在安装…", restarting: "正在重启，完成后页面会自动刷新。" },
      failed: (v) => `没能更新到 ${v}`,
      wentBack: (v) => `已回到 ${v}，工作区照常可用。`,
      notBack: (v) => `也没能回到 ${v}，请在服务器上运行 lattice-host status 查看。`,
      reason: "原因：",
      startFailed: "没能开始更新。",
      chip: (v) => `更新到 ${v}`,
    } : {
      available: (v) => `Lattice-web-ui ${v} is available`,
      brings: (l) => `Includes Lattice ${l}. `,
      takes: "Updating takes about a minute; the workspace reconnects.",
      update: "Update", later: "Later", dismiss: "Dismiss notification", ok: "OK",
      running: (v) => `Updating to ${v}…`,
      phase: { requested: "Starting…", downloading: "Downloading and checking the new version…", installing: "Installing…", restarting: "Restarting. The page reloads when it is done." },
      failed: (v) => `Could not update to ${v}`,
      wentBack: (v) => `Back on ${v}; the workspace works as before.`,
      notBack: (v) => `Going back to ${v} failed too. Run lattice-host status on the host.`,
      reason: "Reason: ",
      startFailed: "Could not start the update.",
      chip: (v) => `Update to ${v}`,
    };
    const svg = (paths, size) => `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;
    const ICON = {
      info: svg('<circle cx="12" cy="12" r="10"></circle><path d="M12 16v-4"></path><path d="M12 8h.01"></path>', 16),
      error: svg('<circle cx="12" cy="12" r="10"></circle><line x1="12" x2="12" y1="8" y2="12"></line><line x1="12" x2="12.01" y1="16" y2="16"></line>', 16),
      loader: `<span class="ui-infinity-loader" aria-hidden="true" style="width: 16px; height: 16px;"><img alt="" aria-hidden="true" src="data:image/svg+xml,%3csvg%20xmlns='http://www.w3.org/2000/svg'%20width='24'%20height='24'%20viewBox='0%200%20100%20100'%20preserveAspectRatio='xMidYMid'%20role='img'%20aria-labelledby='infinity-loader-title'%20%3e%3ctitle%20id='infinity-loader-title'%3eLoading%3c/title%3e%3cpath%20d='M24.3%2030C11.4%2030%205%2043.3%205%2050s6.4%2020%2019.3%2020c19.3%200%2032.1-40%2051.4-40C88.6%2030%2095%2043.3%2095%2050s-6.4%2020-19.3%2020C56.4%2070%2043.6%2030%2024.3%2030z'%20fill='none'%20stroke='currentColor'%20stroke-width='10'%20stroke-dasharray='205.271142578125%2051.317785644531256'%20stroke-linecap='round'%20transform='scale(0.8)'%20transform-origin='50px%2050px'%20%3e%3canimate%20attributeName='stroke-dashoffset'%20repeatCount='indefinite'%20dur='2s'%20keyTimes='0;1'%20values='0;256.58892822265625'%20/%3e%3c/path%3e%3c/svg%3e"></span>`,
      close: svg('<path d="M18 6 6 18"></path><path d="m6 6 12 12"></path>', 24),
    };
    const api = (path, body) => fetch(path, {
      method: body === undefined ? "GET" : "POST", credentials: "same-origin", cache: "no-store",
      headers: body === undefined ? {} : { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });

    let viewport = null;
    let hiddenFor = null;
    function close() { viewport?.remove(); viewport = null; }
    function show({ level, icon, title, detail, actions = [], progress = false, closable = true, onClose }) {
      close();
      viewport = document.createElement("div");
      viewport.className = "app-toast-viewport";
      viewport.dataset.position = "bottom-right";
      viewport.dataset.latticeGateway = "update";
      viewport.setAttribute("role", "region");
      viewport.setAttribute("aria-live", "polite");
      viewport.style.zIndex = "2147483647";
      const toast = document.createElement("div");
      toast.className = `app-toast ${level} multiline`;
      toast.dataset.type = level;
      toast.dataset.appToast = "";
      toast.setAttribute("role", "dialog");
      toast.setAttribute("aria-modal", "false");
      for (const [name, value] of Object.entries({ "--toast-swipe-movement-x": "0px", "--toast-swipe-movement-y": "0px", "--toast-index": "0", "--toast-offset-y": "0px" })) {
        toast.style.setProperty(name, value);
      }
      const content = document.createElement("div");
      content.className = "app-toast-content";
      const iconBox = document.createElement("span");
      iconBox.className = "app-toast-icon";
      iconBox.innerHTML = icon;
      const body = document.createElement("div");
      body.className = "app-toast-body";
      const heading = document.createElement("h2");
      heading.className = "app-toast-title";
      heading.textContent = title;
      body.append(heading);
      if (detail) {
        const text = document.createElement("p");
        text.className = "app-toast-description";
        text.title = detail;
        text.textContent = detail;
        body.append(text);
      }
      if (progress) {
        const row = document.createElement("div");
        row.className = "app-toast-progress-row";
        row.innerHTML = '<div class="app-toast-progress" role="progressbar" data-indeterminate=""><div class="app-toast-progress-fill"></div></div>';
        row.firstElementChild.setAttribute("aria-label", title);
        body.append(row);
      }
      if (actions.length) {
        const row = document.createElement("div");
        row.className = "app-toast-actions";
        actions.forEach(({ label, run }, index) => {
          const button = document.createElement("button");
          button.type = "button";
          button.className = `ui-button ui-button--${index === 0 ? "tinted" : "secondary"} ui-button--compact`;
          button.textContent = label;
          button.addEventListener("click", run);
          row.append(button);
        });
        body.append(row);
      }
      content.append(iconBox, body);
      if (closable) {
        const x = document.createElement("button");
        x.type = "button";
        x.className = "ui-icon-button";
        x.dataset.slot = "icon-button";
        x.dataset.size = "compact";
        x.dataset.tone = "neutral";
        x.setAttribute("aria-label", T.dismiss);
        x.innerHTML = ICON.close;
        x.addEventListener("click", () => { hiddenFor = title; close(); onClose?.(); });
        content.append(x);
      } else {
        content.append(document.createElement("span"));
      }
      toast.append(content);
      viewport.append(toast);
      document.body.append(viewport);
      // Lattice's stack sizes each card from Base UI's measurements; this one measures itself.
      const fit = () => {
        if (!viewport) return;
        const height = `${content.scrollHeight}px`;
        toast.style.setProperty("--toast-height", height);
        viewport.style.setProperty("--toast-frontmost-height", height);
        // Stand above Lattice's own notifications rather than on top of them.
        const own = [...document.querySelectorAll(".app-toast-viewport:not([data-lattice-gateway])")]
          .find((node) => node.dataset.position === "bottom-right" && node.childElementCount);
        const below = own ? parseFloat(getComputedStyle(own).getPropertyValue("--toast-frontmost-height")) || 0 : 0;
        viewport.style.bottom = below ? `${44 + below + 8}px` : "";
      };
      requestAnimationFrame(fit);
      new ResizeObserver(fit).observe(content);
    }

    // The phone dialog's first-visit hint is one of these notifications too.
    toastApi.show = (options) => show({ level: "info", icon: ICON.info, ...options });
    toastApi.close = close;
    toastApi.busy = () => Boolean(viewport) || following;

    function offer(state, asked = false) {
      const title = T.available(state.latest);
      if (hiddenFor === title && !asked) return;
      const lattice = state.lattice && state.lattice.latest && state.lattice.latest !== state.lattice.current
        ? T.brings(state.lattice.latest) : "";
      show({
        level: "info", icon: ICON.info, title, detail: lattice + T.takes,
        actions: [
          { label: T.update, run: () => start(state.latest) },
          { label: T.later, run: () => { api("/__gateway/api/update/dismiss", { version: state.latest }).catch(() => {}); close(); } },
        ],
      });
    }

    // The chip stays while an update is on offer, whatever happened to the card:
    // in the title bar, or on a narrow screen as a round button in the corner.
    let chip = null;
    let chipWired = false;
    function placeChip() {
      if (!chip) return;
      const narrow = innerWidth < 700;
      const anchor = narrow ? null
        : document.querySelector(".titlebar .canvas-actions") || document.querySelector(".welcome-titlebar .icon-button");
      const bar = anchor?.closest(".titlebar, .welcome-titlebar");
      chip.dataset.compact = anchor ? "false" : "true";
      if (anchor && bar) {
        const a = anchor.getBoundingClientRect();
        const b = bar.getBoundingClientRect();
        chip.style.top = `${Math.round(b.top + (b.height - 24) / 2)}px`;
        chip.style.right = `${Math.round(innerWidth - a.left + 8)}px`;
        chip.style.bottom = "";
      } else {
        chip.style.top = "";
        chip.style.right = "max(12px, env(safe-area-inset-right))";
        chip.style.bottom = "calc(max(12px, env(safe-area-inset-bottom)) + 52px)";
      }
    }
    function showChip(state) {
      if (!chip) {
        chip = document.createElement("button");
        chip.type = "button";
        chip.dataset.latticeGateway = "update-chip";
        chip.addEventListener("click", () => offer(chip.state, true));
        document.body.append(chip);
      }
      if (!chipWired) {
        chipWired = true;
        const style = document.createElement("style");
        style.dataset.latticeGateway = "update-chip-style";
        style.textContent = `
          [data-lattice-gateway="update-chip"] { position: fixed; z-index: 2147483000; display: flex; align-items: center; justify-content: center; gap: 6px;
            height: 24px; padding: 0 10px; border-radius: 999px; border: 1px solid var(--border-subtle, #1c1c1f17);
            background: var(--surface-panel-raised, #fff); color: var(--text-primary, #242426); box-shadow: var(--elevation-floating);
            font: var(--weight-medium, 500) var(--type-caption-size, 12px)/1 var(--ui-font, system-ui, sans-serif); white-space: nowrap; cursor: pointer; }
          [data-lattice-gateway="update-chip"]:hover { background: var(--chrome-hover-surface, #f0f0ef); }
          [data-lattice-gateway="update-chip"]::before { content: ""; width: 6px; height: 6px; border-radius: 50%; background: var(--status-info, #3b6fd8); flex: none; }
          [data-lattice-gateway="update-chip"] > svg { display: none; }
          [data-lattice-gateway="update-chip"][data-compact="true"] { width: 40px; height: 40px; padding: 0; }
          [data-lattice-gateway="update-chip"][data-compact="true"]::before { display: none; }
          [data-lattice-gateway="update-chip"][data-compact="true"] > span { display: none; }
          [data-lattice-gateway="update-chip"][data-compact="true"] > svg { display: block; color: var(--status-info, #3b6fd8); }
        `;
        document.head.append(style);
        addEventListener("resize", placeChip);
        new MutationObserver(() => requestAnimationFrame(placeChip)).observe(document.body, { childList: true, subtree: true });
      }
      chip.state = state;
      const label = document.createElement("span");
      label.textContent = T.chip(state.latest);
      chip.replaceChildren(label);
      chip.insertAdjacentHTML("beforeend", svg('<circle cx="12" cy="12" r="10"></circle><path d="m16 12-4-4-4 4"></path><path d="M12 16V8"></path>', 20));
      chip.setAttribute("aria-label", T.available(state.latest));
      chip.title = T.available(state.latest);
      placeChip();
    }
    function hideChip() {
      chip?.remove();
      chip = null;
    }

    async function start(version) {
      hideChip();
      showRunning(version, "requested");
      try {
        const response = await api("/__gateway/api/update/start", { version });
        if (!response.ok) {
          const value = await response.json().catch(() => ({}));
          throw new Error(value?.error?.message || T.startFailed);
        }
      } catch (error) {
        showFailed(version, error.message || T.startFailed);
        return;
      }
      follow(version);
    }

    function showRunning(version, phase) {
      show({ level: "info", icon: ICON.loader, title: T.running(version), detail: T.phase[phase] || T.phase.requested, progress: true, closable: false });
    }

    function showFailed(version, detail) {
      show({ level: "error", icon: ICON.error, title: T.failed(version), detail, actions: [{ label: T.ok, run: close }] });
    }

    // While the host updates, the gateway restarts and Lattice may drop its
    // connection; the page reloads once the new gateway answers.
    let following = false;
    function follow(version) {
      if (following) return;
      following = true;
      let phase = "requested";
      const tick = async () => {
        try {
          const health = await (await fetch("/__gateway/health", { cache: "no-store" })).json();
          if (health.version === version) {
            setTimeout(() => location.reload(), 1500);
            return;
          }
        } catch { /* restarting */ }
        try {
          const state = await (await api("/__gateway/api/update")).json();
          const status = state.status || {};
          if (status.phase === "failed" && status.target === version) {
            following = false;
            const back = status.rolledBack === true ? T.wentBack(status.previous)
              : status.rolledBack === false ? T.notBack(status.previous) : "";
            showFailed(version, `${back}${back ? " " : ""}${T.reason}${status.error || ""}`);
            return;
          }
          if (status.phase && status.phase !== phase && T.phase[status.phase]) {
            phase = status.phase;
          }
        } catch { /* restarting */ }
        showRunning(version, phase);
        // Lattice covers the page when its connection drops; stay in front of that cover.
        if (viewport && viewport !== document.body.lastElementChild) document.body.append(viewport);
        setTimeout(tick, 2000);
      };
      tick();
    }

    // `quiet`: the owner is looking at the Version page, which shows the offer itself.
    function apply(state, quiet = false) {
      updateApi.state = state;
      for (const listener of updateApi.listeners) listener(state);
      if (following) return;
      if (state.running) {
        hideChip();
        follow(state.status?.target || state.latest);
      } else if (state.available) {
        showChip(state);
        if (!state.dismissed && !quiet) offer(state);
      } else {
        hideChip();
      }
    }
    updateApi.apply = (state) => apply(state, true);
    updateApi.start = start;

    async function check() {
      if (following) return;
      try {
        const response = await api("/__gateway/api/update");
        if (!response.ok) return;
        apply(await response.json());
      } catch { /* the next check retries */ }
    }
    fetch("/__gateway/api/whoami", { credentials: "same-origin", cache: "no-store" })
      .then((response) => response.json())
      .then((me) => {
        if (me.kind !== "owner") return;
        setTimeout(check, 5000);
        setInterval(check, 30 * 60 * 1000);
      })
      .catch(() => {});
  })();

  // lattice-web-ui's name and icon. Lattice's own links are later in the page
  // than this script, so they are swapped once the page is parsed.
  (() => {
    const NAME = "Lattice-web-ui";
    const LINKS = [
      ["icon", "/__gateway/brand/logo.svg", "image/svg+xml"],
      ["icon", "/__gateway/brand/favicon-32.png", "image/png"],
      ["apple-touch-icon", "/__gateway/brand/apple-touch-icon.png"],
      ["manifest", "/__gateway/brand/manifest.webmanifest"],
    ];
    function brandHead() {
      for (const node of document.querySelectorAll('link[rel~="icon"], link[rel="apple-touch-icon"], link[rel="manifest"]')) {
        if (!node.dataset.latticeGateway) node.remove();
      }
      if (document.querySelector("link[data-lattice-gateway]")) return;
      for (const [rel, href, type] of LINKS) {
        const link = document.createElement("link");
        link.rel = rel;
        link.href = href;
        if (type) link.type = type;
        link.dataset.latticeGateway = "brand";
        document.head.append(link);
      }
      const meta = document.createElement("meta");
      meta.name = "apple-mobile-web-app-title";
      meta.content = NAME;
      meta.dataset.latticeGateway = "brand";
      document.head.append(meta);
    }
    // "Lattice" alone, or after a separator, is the app's name; a project's own name is left alone.
    const renamed = (title) => title === "Lattice" ? NAME : title.replace(/(\s[—–-]\s)Lattice$/, `$1${NAME}`);
    function brandTitle() {
      const next = renamed(document.title);
      if (next !== document.title) document.title = next;
    }
    // On the welcome screen, the mark and the heading.
    const style = document.createElement("style");
    style.dataset.latticeGateway = "brand";
    style.textContent = `
      .welcome-content > .brand-mark { background: url("/__gateway/brand/logo.svg") center / 100% 100% no-repeat; border: 0; box-shadow: none; }
      .welcome-content > .brand-mark > svg { visibility: hidden; }
    `;
    function brandWelcome() {
      const heading = document.querySelector(".welcome-content > h1");
      const text = heading?.firstChild;
      // The text node itself, so React keeps its own node and never redraws it.
      if (text && text.nodeType === Node.TEXT_NODE && text.nodeValue === "Lattice") text.nodeValue = NAME;
    }
    const start = () => {
      document.head.append(style);
      brandHead();
      brandTitle();
      brandWelcome();
      new MutationObserver(() => { brandTitle(); brandWelcome(); })
        .observe(document.documentElement, { childList: true, subtree: true, characterData: true });
    };
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
    else start();
  })();

  // The workspace fills the screen, under the status bar too. Lattice's page
  // stops at the safe area, and iOS shows the top of the page blurred under
  // the status bar, which an open iPhone Duo or an iPad keeps in landscape.
  // With viewport-fit=cover, Lattice's root keeps its content inside the safe
  // area, and the strip under the status bar is painted in the colour of what
  // is at the top of the page. Where there is no safe area, nothing changes.
  (() => {
    const style = document.createElement("style");
    style.dataset.latticeGateway = "safe-area";
    style.textContent = `
      #root { padding: env(safe-area-inset-top, 0px) env(safe-area-inset-right, 0px) env(safe-area-inset-bottom, 0px) env(safe-area-inset-left, 0px); }
      #root:has(.titlebar) { background: linear-gradient(var(--surface-titlebar) env(safe-area-inset-top, 0px), var(--surface-window) 0); }
      #root:has(.titlebar.titlebar-focus) { background: linear-gradient(var(--surface-shell) env(safe-area-inset-top, 0px), var(--surface-window) 0); }
      #root:has(.welcome-screen) { background: var(--surface-backdrop); }
    `;
    // Lattice's viewport tag comes later in the head than this script.
    const cover = () => {
      const meta = document.querySelector('meta[name="viewport"]');
      if (!meta) return false;
      if (!/viewport-fit/.test(meta.content)) meta.content = `${meta.content}, viewport-fit=cover`;
      return true;
    };
    if (!cover()) {
      const watch = new MutationObserver(() => { if (cover()) watch.disconnect(); });
      watch.observe(document.documentElement, { childList: true, subtree: true });
      document.addEventListener("DOMContentLoaded", () => { watch.disconnect(); cover(); }, { once: true });
    }
    document.head.append(style);
  })();

  // "Phone and app", for the owner. It uses Lattice's own modal, Settings
  // header, groups and rows (dialogs.css, components/ui/chrome.css), so it
  // looks and behaves like a page of Lattice's Settings.
  (() => {
    const svg = (paths, size) => `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;
    const api = (path, body) => fetch(path, {
      method: body === undefined ? "GET" : "POST", credentials: "same-origin", cache: "no-store",
      headers: body === undefined ? {} : { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    }).then(async (response) => {
      const value = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(value?.error?.message || "The request failed.");
      return value;
    });
    const el = (tag, props = {}, ...children) => {
      const node = document.createElement(tag);
      for (const [key, value] of Object.entries(props)) {
        if (key === "class") node.className = value;
        else if (key === "html") node.innerHTML = value;
        else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
        else if (value !== undefined && value !== null && value !== false) node.setAttribute(key, value === true ? "" : value);
      }
      for (const child of children.flat()) if (child !== null && child !== undefined && child !== false) node.append(child);
      return node;
    };
    const button = (label, variant, onclick) =>
      el("button", { type: "button", class: `ui-button ui-button--${variant}`, "data-slot": "button", onclick }, label);
    const row = (label, description, ...control) => el("div", { class: "ui-row ui-row--data ui-settings-row", "data-slot": "settings-row" },
      el("div", { class: "ui-settings-row-copy" },
        el("span", { class: "ui-settings-row-label" }, label),
        description ? el("p", { class: "ui-settings-row-description" }, description) : null),
      control.length ? el("div", { class: "ui-settings-row-control" }, ...control) : null);
    const group = (title, ...rows) => el("section", { class: "ui-settings-group", "data-slot": "settings-group" },
      el("h3", { class: "ui-settings-group-title" }, title),
      el("div", { class: "ui-settings-group-body" }, ...rows));

    function platform() {
      const ua = navigator.userAgent;
      if (/iPhone|iPad/.test(ua) || (/Macintosh/.test(ua) && navigator.maxTouchPoints > 1)) return "ios";
      if (/Android/.test(ua)) return "android";
      if (/Macintosh/.test(ua)) return "mac";
      return "desktop";
    }
    let installPrompt = null;
    addEventListener("beforeinstallprompt", (event) => { event.preventDefault(); installPrompt = event; });

    const style = document.createElement("style");
    style.dataset.latticeGateway = "phone-style";
    style.textContent = `
      [data-gateway-dialog] .settings-modal { width: min(640px, calc(100vw - 32px)); height: auto; max-height: min(760px, calc(100dvh - 48px)); display: flex; flex-direction: column; background: var(--surface-panel); }
      [data-gateway-dialog] { outline: none; }
      .modal-backdrop[data-gateway-dialog-backdrop] { z-index: 2147483646; }
      .modal-dialog-content[data-gateway-dialog] { z-index: 2147483647; }
      [data-gateway-dialog] .settings-content { overflow-y: auto; overscroll-behavior: contain; }
      [data-gateway-dialog] .settings-section { max-width: none; }
      [data-gateway-page] .phone-badge { margin-inline-start: var(--space-3); padding: 1px 6px; border-radius: 999px; background: var(--control-active-soft); color: var(--control-active); font-size: var(--type-micro-size, 11px); font-weight: var(--weight-semibold, 600); vertical-align: 1px; }
      [data-gateway-page="version"] .version-date { margin-inline-start: var(--space-3, 6px); color: var(--text-tertiary); font-weight: normal; }
      [data-gateway-page="version"] .version-changes { margin: var(--space-2, 4px) 0 0; padding-inline-start: 1.1em; color: var(--text-secondary); font-size: var(--type-caption-size, 12px); line-height: 1.55; }
      [data-gateway-page="version"] .version-changes li::marker { color: var(--text-tertiary); }
      [data-gateway-page="version"] .version-external { display: inline-flex; margin-inline-start: 4px; }
      [data-gateway-page="version"] .version-github { display: inline-flex; margin-inline-end: 6px; }
      [data-gateway-page] a.ui-button, [data-gateway-page] a.ui-icon-button, [data-lattice-gateway="version-row"] a { text-decoration: none; }
      /* Settings' pane shows the gateway's page in place of Lattice's. */
      .settings-modal[data-gateway-page] .settings-content [data-slot="scroll-area-content"] > :not(.settings-back):not([data-gateway-page]) { display: none; }
      .settings-modal[data-gateway-page] .settings-nav-group:not([data-lattice-gateway]) > button.active:not(:hover) { background: transparent; color: var(--text-secondary); font-weight: inherit; }
      [data-gateway-page="phone"] .phone-qr { width: 104px; height: 104px; padding: 6px; border-radius: var(--radius-control); background: #fff; image-rendering: pixelated; }
      [data-gateway-page="phone"] .phone-qr.large { width: 168px; height: 168px; }
      [data-gateway-page="phone"] .phone-address { font-family: var(--mono-font, ui-monospace, monospace); overflow-wrap: anywhere; }
      [data-gateway-page="phone"] .phone-pair { display: grid; justify-items: center; gap: var(--space-4); padding-block: var(--space-6); text-align: center; color: var(--text-secondary); font-size: var(--type-caption-size); }
      @media (max-width: 560px) { [data-gateway-dialog] .settings-modal { width: calc(100vw - 16px); max-height: calc(100dvh - 16px); } }
    `;

    let open = null;
    let closed = null;
    function close() {
      open?.forEach((node) => node.remove());
      open = null;
      closed?.();
      closed = null;
      removeEventListener("keydown", onEscape, true);
    }
    // Escape closes this dialog and not the Settings under it, wherever focus is.
    function onEscape(event) {
      if (event.key !== "Escape" || !open) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      close();
    }

    // One of these dialogs, made like Lattice's Settings modal.
    function present(kind, label, section, onClosed) {
      section.dataset.gatewayPage = kind;
      const closeButton = el("button", { type: "button", class: "ui-icon-button settings-close", "data-slot": "icon-button", "data-size": "compact", "data-tone": "neutral", "aria-label": zhUI ? "关闭" : "Close", html: svg('<path d="M18 6 6 18"></path><path d="m6 6 12 12"></path>', 16), onclick: close });
      const backdrop = el("div", { class: "modal-backdrop", "data-lattice-gateway": `${kind}-backdrop`, "data-gateway-dialog-backdrop": true, onclick: close });
      const dialog = el("div", { class: "modal-dialog-content", "data-lattice-gateway": kind, "data-gateway-dialog": true, role: "dialog", "aria-modal": "true", "aria-label": label, tabindex: "-1" },
        el("div", { class: "settings-modal" }, closeButton, el("div", { class: "settings-content" }, section)));
      document.body.append(backdrop, dialog);
      open = [backdrop, dialog];
      closed = onClosed || null;
      liftAboveCover();
      dialog.focus({ preventScroll: true });
      addEventListener("keydown", onEscape, true);
    }

    async function show(inSettings = false) {
      if (open) return;
      if (!style.isConnected) document.head.append(style);
      const T = {
        title: "Phone and app",
        intro: "Open this workspace on your phone, keep it on the home screen or in the Dock, and see which devices are paired.",
        openGroup: "Open on your phone",
        scan: "Scan with your phone",
        scanTailnet: "A phone signed in to the same Tailscale account opens the workspace with no sign-in.",
        scanPublic: "Opens the sign-in page. Let the phone in with a pairing code below.",
        address: "Address",
        copy: "Copy", copied: "Copied",
        pairLabel: "Phone not on your tailnet",
        pairHint: "A one-time code lets one more device in. It works once and expires in two minutes.",
        pair: "Pair a device", pairScan: "Scan this code on the other device.", copyLink: "Copy link",
        installGroup: "Install the app",
        thisDevice: "This device",
        platforms: [
          ["ios", "iPhone and iPad", "In Safari: Share › Add to Home Screen."],
          ["android", "Android", "In Chrome: ⋮ › Install app."],
          ["mac", "Mac", "In Safari: File › Add to Dock. In Chrome: the install icon in the address bar."],
          ["desktop", "Windows and Linux", "In Chrome or Edge: the install icon in the address bar."],
        ],
        install: "Install",
        appNote: "The installed app keeps its own sign-in. On your tailnet it still opens directly; from the internet, pair it once.",
        devicesGroup: "Paired devices",
        noDevices: "No devices are paired yet.",
        revoke: "Revoke",
        paired: (date) => `Paired ${date}`,
        close: "Close",
        browserGroup: "This browser",
        signedInHere: "Signed in with the access token",
        signedInHint: "Sign out when this is not your own device.",
        signOut: "Sign out",
      };
      const [phone, devices] = await Promise.all([
        api("/__gateway/api/phone").catch(() => ({})),
        api("/__gateway/api/devices").then((v) => v.devices).catch(() => []),
      ]);
      const address = `${phone.address || location.origin}/`;
      const qr = (svgText, large) => el("img", { class: `phone-qr${large ? " large" : ""}`, alt: "", src: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svgText)}` });

      const copyButton = button(T.copy, "secondary", async () => {
        try { await navigator.clipboard.writeText(address); copyButton.textContent = T.copied; setTimeout(() => { copyButton.textContent = T.copy; }, 1500); } catch { /* clipboard refused */ }
      });
      let pairCode = null;
      const pairButton = button(T.pair, "secondary", async () => {
        pairButton.disabled = true;
        try {
          const { url, qrSvg } = await api("/__gateway/api/invites", {});
          const next = el("div", { class: "phone-pair" }, qr(qrSvg, true), el("span", {}, T.pairScan),
            button(T.copyLink, "secondary", () => navigator.clipboard.writeText(url).catch(() => {})));
          if (pairCode) pairCode.replaceWith(next); else pairButton.closest(".ui-settings-row").after(next);
          pairCode = next;
        } finally { pairButton.disabled = false; }
      });

      const here = platform();
      const installRows = T.platforms.map(([id, name, how]) => row(
        el("span", {}, name, id === here ? el("span", { class: "phone-badge" }, T.thisDevice) : null), how,
        id === here && installPrompt ? button(T.install, "tinted", async () => { installPrompt.prompt(); await installPrompt.userChoice.catch(() => {}); installPrompt = null; }) : null));

      const deviceRows = devices.length
        ? devices.map((device) => {
          const revoke = button(T.revoke, "secondary", async () => {
            revoke.disabled = true;
            try { await api(`/__gateway/api/devices/${encodeURIComponent(device.id)}/revoke`, {}); deviceRow.remove(); } catch { revoke.disabled = false; }
          });
          const when = device.createdAt ? new Date(device.createdAt * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" }) : "";
          const deviceRow = row(device.label, when ? T.paired(when) : "", revoke);
          return deviceRow;
        })
        : [row(T.noDevices, "")];

      const section = el("div", { class: "settings-section" },
        el("div", { class: "ui-settings-section-header" }, el("div", { class: "ui-settings-section-header-copy" },
          el("h2", { class: "ui-settings-section-header-title" }, T.title),
          el("p", { class: "ui-settings-section-header-description" }, T.intro))),
        group(T.openGroup,
          row(T.scan, phone.tailnet ? T.scanTailnet : T.scanPublic, phone.qrSvg ? qr(phone.qrSvg) : null),
          row(T.address, el("span", { class: "phone-address" }, address), copyButton),
          row(T.pairLabel, T.pairHint, pairButton)),
        group(T.installGroup, ...installRows, el("p", { class: "ui-settings-row-description" }, T.appNote)),
        group(T.devicesGroup, ...deviceRows),
        phone.signedIn ? group(T.browserGroup, row(T.signedInHere, T.signedInHint, button(T.signOut, "secondary", async () => {
          await api("/__gateway/api/logout", {}).catch(() => {});
          location.replace("/__gateway/login");
        }))) : null);
      if (inSettings) mount("phone", T.title, section);
      else present("phone", T.title, section);
    }

    // Gateway pages shown in Settings' own content pane, like Lattice's pages:
    // the item in the sidebar is selected, Lattice's page is set aside, and on
    // a narrow screen the page replaces the list, with Lattice's back button.
    // Opened from elsewhere (the plugin's command, a hint), a page gets its
    // own dialog instead.
    let page = null;
    function markNav() {
      for (const item of document.querySelectorAll(".settings-nav [data-gateway-page-item]")) {
        const on = page?.kind === item.dataset.gatewayPageItem;
        item.classList.toggle("active", on);
        if (on) item.setAttribute("aria-current", "page"); else item.removeAttribute("aria-current");
      }
    }
    function leavePage(back = false) {
      if (!page) return;
      const { section, modal, cleanup } = page;
      page = null;
      section.remove();
      delete modal.dataset.gatewayPage;
      if (back) modal.querySelector(".settings-body")?.setAttribute("data-narrow-view", "root");
      cleanup?.();
      markNav();
    }
    function mount(kind, label, section, cleanup) {
      if (!style.isConnected) document.head.append(style);
      section.dataset.gatewayPage = kind;
      const modal = document.querySelector(".settings-modal:has(> .settings-body)");
      const content = modal?.querySelector('.settings-content [data-slot="scroll-area-content"]');
      if (!content) {
        present(kind, label, section, cleanup);
        return;
      }
      leavePage();
      content.append(section);
      modal.dataset.gatewayPage = kind;
      modal.querySelector(".settings-body")?.setAttribute("data-narrow-view", "page");
      const viewport = modal.querySelector('.settings-content [data-slot="scroll-area-viewport"]');
      if (viewport) viewport.scrollTop = 0;
      page = { kind, section, modal, cleanup };
      markNav();
    }
    // Any of Lattice's own items, or a search result, brings its page back.
    document.addEventListener("click", (event) => {
      const target = page && event.target.closest?.("button");
      if (!target || !page.modal.contains(target)) return;
      if (target.matches(".settings-back")) leavePage(true);
      else if (target.closest(".settings-nav") && !target.dataset.gatewayPageItem) leavePage();
    }, true);
    // On a narrow screen Escape goes back to the list, as from Lattice's pages.
    addEventListener("keydown", (event) => {
      if (event.key !== "Escape" || !page || open) return;
      const nav = page.modal.querySelector(".settings-nav");
      if (!nav || getComputedStyle(nav).display !== "none") return;
      event.preventDefault();
      event.stopImmediatePropagation();
      // Lattice's own back button, so its idea of the open page follows too.
      const back = page.modal.querySelector(".settings-back");
      if (back) back.click(); else leavePage(true);
    }, true);

    // "Version": what this host runs, the update on offer, what it brings,
    // and Check now. Its words and controls are those of Lattice's own App
    // updates (Settings › Editor & builds), whose Linux row it stands in for.
    const V = zhUI ? {
      nav: "版本",
      phoneNav: "手机与应用",
      title: "版本",
      releases: "GitHub 发布页",
      updatesGroup: "更新",
      lattice: "Lattice",
      latticeDetail: "版本由 Lattice-web-ui 固定，随它一起更新。",
      ready: (v) => `新版本 ${v} 可安装`,
      brings: (l) => `，包含 Lattice ${l}`,
      takes: "更新约需一分钟，期间工作区会重新连接。",
      update: (v) => `更新到 ${v}`,
      upToDate: "已是最新",
      checked: (when) => `${when}检查过`,
      never: "还没有检查过",
      running: (v) => `正在更新到 ${v}…`,
      runningDetail: "进度显示在右下角的通知里，完成后页面会自动刷新。",
      check: "检查更新",
      checking: "正在检查…",
      failed: (why) => `检查更新失败：${why}`,
      timedOut: "主机没有回应，请稍后再试",
      noUpdates: "这台主机不是从发布包安装的，不能在浏览器里更新。",
      whatsNew: (v) => `${v} 的新功能`,
      notes: (v) => `打开 ${v} 的发布说明`,
      available: "有可用更新",
    } : {
      nav: "Version",
      phoneNav: "Phone and app",
      title: "Version",
      releases: "GitHub Releases",
      updatesGroup: "App updates",
      lattice: "Lattice",
      latticeDetail: "Pinned by Lattice-web-ui, and updated with it.",
      ready: (v) => `Version ${v} is ready to install`,
      brings: (l) => `, with Lattice ${l}`,
      takes: "Updating takes about a minute; the workspace reconnects.",
      update: (v) => `Update to ${v}`,
      upToDate: "Up to date",
      checked: (when) => `checked ${when}`,
      never: "Not checked yet",
      running: (v) => `Updating to ${v}…`,
      runningDetail: "The notification in the corner shows how it goes. The page reloads when it is done.",
      check: "Check for updates",
      checking: "Checking…",
      failed: (why) => `Couldn’t check for updates: ${why}`,
      timedOut: "the host did not answer. Try again later",
      noUpdates: "This host was not installed from a release kit, so it cannot update from the browser.",
      whatsNew: (v) => `What’s new in ${v}`,
      notes: (v) => `Open the release notes for ${v}`,
      available: "Updates available",
    };
    const RELEASES = "https://github.com/lattice-writer/lattice-web-ui/releases";
    const GITHUB_MARK = '<svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M8 0c4.42 0 8 3.58 8 8a8.013 8.013 0 0 1-5.45 7.59c-.4.08-.55-.17-.55-.38 0-.27.01-1.13.01-2.2 0-.75-.25-1.23-.54-1.48 1.78-.2 3.65-.88 3.65-3.95 0-.88-.31-1.59-.82-2.15.08-.2.36-1.02-.08-2.12 0 0-.67-.22-2.2.82-.64-.18-1.32-.27-2-.27-.68 0-1.36.09-2 .27-1.53-1.03-2.2-.82-2.2-.82-.44 1.1-.16 1.92-.08 2.12-.51.56-.82 1.28-.82 2.15 0 3.06 1.86 3.75 3.64 3.95-.23.2-.44.55-.51 1.07-.46.21-1.61.55-2.33-.66-.15-.24-.6-.83-1.23-.82-.67.01-.27.38.01.53.34.19.73.9.82 1.13.16.45.68 1.31 2.69.94 0 .67.01 1.3.01 1.49 0 .21-.15.45-.55.38A7.995 7.995 0 0 1 0 8c0-4.42 3.58-8 8-8Z"></path></svg>';
    const EXTERNAL = '<path d="M15 3h6v6"></path><path d="M10 14 21 3"></path><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"></path>';
    const REFRESH = '<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"></path><path d="M21 3v5h-5"></path><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"></path><path d="M8 16H3v5"></path>';
    const ago = (seconds) => {
      const format = new Intl.RelativeTimeFormat(zhUI ? "zh-CN" : "en", { numeric: "auto" });
      const delta = Math.round(seconds - Date.now() / 1000);
      for (const [unit, size] of [["day", 86400], ["hour", 3600], ["minute", 60]]) {
        if (Math.abs(delta) >= size) return format.format(Math.round(delta / size), unit);
      }
      return format.format(0, "minute");
    };
    const day = (iso) => {
      const date = new Date(iso);
      return Number.isNaN(date.getTime()) ? "" : date.toLocaleDateString(zhUI ? "zh-CN" : "en", { year: "numeric", month: "short", day: "numeric" });
    };
    const external = (href, label) => el("a", { class: "ui-icon-button", "data-slot": "icon-button", "data-size": "compact", "data-tone": "neutral", href, target: "_blank", rel: "noopener noreferrer", "aria-label": label, title: label, html: svg(EXTERNAL, 14) });
    const reloadButton = (busy) => {
      const node = el("button", { type: "button", class: "ui-button ui-button--secondary ui-button--compact ui-reload-button", "data-slot": "button", "data-size": "compact", "data-variant": "secondary", "aria-busy": busy ? "true" : null, onclick: () => checkNow() },
        el("span", { class: `ui-reload-icon${busy ? " spin" : ""}`, "aria-hidden": "true", html: svg(REFRESH, 14) }), busy ? V.checking : V.check);
      node.disabled = busy;
      return node;
    };

    // Check now, shared by the Version page and the row in Editor & builds.
    const checker = { busy: false, error: "" };
    const redraw = () => { for (const listener of updateApi.listeners) listener(updateApi.state || {}); };
    async function checkNow() {
      if (checker.busy) return;
      checker.busy = true;
      checker.error = "";
      redraw();
      try {
        let next = await api("/__gateway/api/update/check", {});
        const end = Date.now() + 90_000;
        while (next.checking && Date.now() < end) {
          await new Promise((resolve) => setTimeout(resolve, 1500));
          next = await api("/__gateway/api/update");
        }
        if (next.checking) checker.error = V.timedOut;
        else if (next.checkFailed) checker.error = next.checkFailed.error || "";
        checker.busy = false;
        updateApi.apply(next);
      } catch (error) {
        checker.error = error.message;
        checker.busy = false;
        redraw();
      }
    }

    // The Lattice-web-ui row: its version, where updates stand, and the one
    // thing to do about it, as Lattice's own Version row says it.
    function versionRow(state, label, prefix = "") {
      const current = `${prefix}${state.current || ""}`;
      const lattice = state.lattice || {};
      if (state.updates === false) return row(label, `${current} · ${V.noUpdates}`);
      if (state.running) {
        const target = state.status?.target || state.latest;
        return row(label, `${current} · ${V.running(target)} ${V.runningDetail}`);
      }
      if (state.available && !checker.busy) {
        const brings = lattice.latest && lattice.latest !== lattice.current ? V.brings(lattice.latest) : "";
        const update = el("button", { type: "button", class: "ui-button ui-button--tinted ui-button--compact", "data-slot": "button", "data-size": "compact", "data-variant": "tinted",
          onclick: () => updateApi.start(state.latest) }, V.update(state.latest));
        return row(label, `${current} · ${V.ready(state.latest)}${brings}${zhUI ? "。" : ". "}${V.takes}`, update);
      }
      const status = checker.busy ? V.checking
        : checker.error ? V.failed(checker.error)
          : state.checkedAt ? `${V.upToDate} · ${V.checked(ago(state.checkedAt))}` : V.never;
      return row(label, `${current} · ${status}`, reloadButton(checker.busy));
    }

    async function showVersion() {
      const section = el("div", { class: "settings-section" });
      function render(state) {
        const lattice = state.lattice || {};
        const releases = state.available && !state.running ? (state.releases || []) : [];
        const releaseRows = releases.map((release) => {
          const date = day(release.publishedAt);
          const changes = (release.changes || []).map((change) => change.replace(/\s*\(#\d+\)$/, ""));
          return el("div", { class: "ui-row ui-row--data ui-settings-row", "data-slot": "settings-row" },
            el("div", { class: "ui-settings-row-copy" },
              el("span", { class: "ui-settings-row-label" }, release.version, date ? el("span", { class: "version-date" }, date) : null),
              changes.length ? el("ul", { class: "version-changes" }, changes.map((change) => el("li", {}, change))) : null),
            release.url ? el("div", { class: "ui-settings-row-control" }, external(release.url, V.notes(release.version))) : null);
        });
        section.replaceChildren(
          el("div", { class: "ui-settings-section-header", "data-slot": "settings-section-header" },
            el("div", { class: "ui-settings-section-header-copy" }, el("h2", { class: "ui-settings-section-header-title" }, V.title)),
            el("div", { class: "ui-settings-section-header-actions" },
              el("a", { class: "ui-button ui-button--ghost ui-button--compact", "data-slot": "button", "data-size": "compact", "data-variant": "ghost", href: RELEASES, target: "_blank", rel: "noopener noreferrer" },
                el("span", { class: "version-github", html: GITHUB_MARK }), V.releases, el("span", { class: "version-external", html: svg(EXTERNAL, 12) })))),
          group(V.updatesGroup,
            versionRow(state, "Lattice-web-ui"),
            lattice.current ? row(V.lattice, `${lattice.current} · ${V.latticeDetail}`) : null),
          ...(releaseRows.length ? [group(V.whatsNew(state.latest), ...releaseRows)] : []));
      }
      const listener = (state) => render(state);
      render(updateApi.state || {});
      updateApi.listeners.add(listener);
      mount("version", V.title, section, () => updateApi.listeners.delete(listener));
      api("/__gateway/api/update").then((state) => updateApi.apply(state)).catch(() => {});
    }

    // Lattice's own App updates row says Lattice on Linux doesn't update
    // itself; here Lattice-web-ui does, so its row takes that one's place.
    let editorRow = null;
    function placeEditorRow() {
      const theirs = document.querySelector('.settings-section:not([data-gateway-page]) [data-setting="version"]:not([data-lattice-gateway])');
      if (!theirs || !theirs.querySelector(".ui-button--ghost")) return;
      if (!theirs.hasAttribute("data-gateway-replaced")) theirs.setAttribute("data-gateway-replaced", "");
      if (editorRow?.isConnected && editorRow.previousElementSibling === theirs) return;
      editorRow?.remove();
      editorRow = versionRow(updateApi.state || {}, V.nav, "Lattice-web-ui ");
      editorRow.dataset.latticeGateway = "version-row";
      theirs.after(editorRow);
    }
    updateApi.listeners.add(() => {
      if (!editorRow?.isConnected) return;
      const next = versionRow(updateApi.state || {}, V.nav, "Lattice-web-ui ");
      next.dataset.latticeGateway = "version-row";
      editorRow.replaceWith(next);
      editorRow = next;
    });

    // Lattice covers the page and makes the rest inert when this tab loses
    // the workspace or its connection. The dialog needs neither, so it stays
    // in front of that cover and usable: pairing a phone hands it the
    // workspace, and the owner can still see and revoke it here.
    function liftAboveCover() {
      if (!open) return;
      const cover = document.getElementById("lattice-browser-runtime-error");
      for (const node of open) {
        if (node.hasAttribute("inert")) node.removeAttribute("inert");
        if (cover && (node.compareDocumentPosition(cover) & Node.DOCUMENT_POSITION_FOLLOWING)) document.body.append(node);
      }
    }
    new MutationObserver(liftAboveCover).observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["inert"] });

    // Where the owner looks for it: Lattice's Settings, as one more item in its
    // sidebar, under a Lattice-web-ui group, made like Lattice's own items.
    function addToSettings() {
      const navStyle = document.createElement("style");
      navStyle.dataset.latticeGateway = "settings-style";
      navStyle.textContent = `
        .settings-nav:has(.settings-search-results) [data-lattice-gateway="settings-group"] { display: none; }
        [data-setting="version"][data-gateway-replaced] { display: none; }
        .gw-phone .ph-body, .gw-version .up-arrow { transform-box: view-box; transform-origin: 128px 128px; }
        @keyframes gw-phone-buzz {
          0% { transform: rotate(0deg); animation-timing-function: cubic-bezier(0.25,0.7,0.2,1); }
          12% { transform: rotate(-14deg); } 24% { transform: rotate(11deg); } 36% { transform: rotate(-8deg); }
          48% { transform: rotate(5deg); } 60% { transform: rotate(-2deg); }
          72%, 100% { transform: rotate(0deg); }
        }
        @keyframes gw-version-up {
          0% { transform: translateY(0px); animation-timing-function: cubic-bezier(0.25,0.7,0.2,1); }
          22% { transform: translateY(-30px); animation-timing-function: cubic-bezier(0.25,0.7,0.2,1); }
          44% { transform: translateY(6px); animation-timing-function: cubic-bezier(0.25,0.7,0.2,1); }
          62% { transform: translateY(-14px); animation-timing-function: cubic-bezier(0.25,0.7,0.2,1); }
          80%, 100% { transform: translateY(0px); }
        }
        .bakai-icon.is-playing:not(.is-reduced) .gw-phone .ph-body { animation: gw-phone-buzz 0.9s both; }
        .bakai-icon.is-playing:not(.is-reduced) .gw-version .up-arrow { animation: gw-version-up 1.1s both; }
      `;
      document.head.append(navStyle);
      // Lucide at Lattice's own icons' weight (1.6 at 15px, like its Phosphor set).
      const icon = (paths, size, cls, width = 1.6) => `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="${width}" stroke-linecap="round" stroke-linejoin="round" class="${cls}" aria-hidden="true">${paths}</svg>`;
      const navIcon = (kind, paths) => `<span class="animated-product-icon animated-product-icon--gw-${kind}" aria-hidden="true" style="pointer-events: none;"><span class="bakai-icon"><svg width="15" height="15" viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="16" stroke-linecap="round" stroke-linejoin="round" class="gw-${kind}">${paths}</svg></span></span>`;
      const replay = (event) => {
        const glyph = event.currentTarget.querySelector(".bakai-icon");
        if (!glyph) return;
        glyph.classList.remove("is-playing");
        glyph.classList.toggle("is-reduced", matchMedia("(prefers-reduced-motion: reduce)").matches);
        void glyph.offsetWidth;
        glyph.classList.add("is-playing");
      };
      const item = (kind, label, paths, open, badge = false) => el("button", { type: "button", "data-lattice-gateway": kind === "phone" ? "settings-item" : "settings-version", "data-gateway-page-item": kind, onclick: open, onpointerenter: replay, onfocus: replay,
        html: navIcon(kind, paths)
          + `<span title="${label}">${label}</span>`
          + (badge ? `<span class="settings-nav-badge" role="img" aria-label="${V.available}" hidden></span>` : "")
          + icon('<path d="m9 18 6-6-6-6"></path>', 14, "settings-nav-chevron") });
      const place = () => {
        if (page && !page.section.isConnected) { const { cleanup } = page; page = null; cleanup?.(); }
        placeEditorRow();
        const nav = document.querySelector(".settings-modal .settings-nav");
        if (!nav) return;
        let group = nav.querySelector('[data-lattice-gateway="settings-group"]');
        if (!group) {
          group = el("div", { class: "settings-nav-group", role: "group", "aria-label": "Lattice-web-ui", "data-lattice-gateway": "settings-group" },
            el("div", { class: "settings-nav-group-label" }, "Lattice-web-ui"),
            item("phone", V.phoneNav, '<g class="ph-body"><rect x="64" y="24" width="128" height="208" rx="16"></rect><path d="M64 56H192"></path><path d="M64 200H192"></path></g>', () => show(true)),
            item("version", V.nav, '<circle cx="128" cy="128" r="96"></circle><g class="up-arrow"><path d="M94 122L128 88L162 122"></path><path d="M128 168V88"></path></g>', () => showVersion(), true));
          markNav();
        }
        const badge = group.querySelector(".settings-nav-badge");
        if (badge && badge.hidden === Boolean(updateApi.state?.available)) badge.hidden = !updateApi.state?.available;
        // After Lattice's own groups, which it may redraw after a search.
        if (nav.lastElementChild !== group) nav.append(group);
      };
      new MutationObserver(place).observe(document.body, { childList: true, subtree: true });
      updateApi.listeners.add(place);
      place();
    }

    openPhoneGuide = show;
    // The plugin's command, the devices address and old links land here.
    const params = new URLSearchParams(location.search);
    const asked = params.get("lattice-gateway") === "phone";
    if (asked) {
      params.delete("lattice-gateway");
      history.replaceState(null, "", `${location.pathname}${params.size ? `?${params}` : ""}${location.hash}`);
    }
    fetch("/__gateway/api/whoami", { credentials: "same-origin", cache: "no-store" })
      .then((response) => response.json())
      .then(async (me) => {
        if (me.kind !== "owner") return;
        addToSettings();
        if (asked) { setTimeout(show, 1500); return; }
        // Once, on a computer, and not inside the installed app: point the owner at the phone setup.
        const standalone = matchMedia("(display-mode: standalone)").matches || navigator.standalone === true;
        if (innerWidth < 700 || standalone) return;
        const phone = await api("/__gateway/api/phone").catch(() => null);
        if (!phone || phone.hintDismissed) return;
        setTimeout(() => {
          if (toastApi.busy() || document.querySelector("[data-lattice-gateway=phone]")) return;
          const dismiss = () => api("/__gateway/api/phone/hint", {}).catch(() => {});
          toastApi.show({
            title: "Use this workspace on your phone",
            detail: "Scan a code to open it on your phone, and install it as an app.",
            onClose: dismiss,
            actions: [
              { label: "Set up phone", run: () => { dismiss(); toastApi.close(); show(); } },
              { label: "Don't show again", run: () => { dismiss(); toastApi.close(); } },
            ],
          });
        }, 9000);
      })
      .catch(() => {});
  })();

  // Lattice's message and confirm dialogs, and its Open panels, shown in this
  // tab: their native panels would open on the host's display, which nobody
  // sees, and the tab would wait for an answer that never comes. The dialogs
  // use Lattice's own confirm dialog markup (confirm-action-dialog.tsx); the
  // Open panel browses the host's home folder through the gateway.
  (() => {
    const T = zhUI ? {
      ok: "确定", cancel: "取消", yes: "是", no: "否",
      chooseFolder: "选择文件夹", chooseFile: "选择文件", chooseFiles: "选择文件",
      useFolder: "选择此文件夹", open: "打开", newFolder: "新建文件夹", upload: "从此设备上传…",
      folderName: "文件夹名称", create: "新建", home: "主文件夹", empty: "这里没有可选的内容。",
      uploading: (done, total) => `正在上传 ${done} / ${total}…`, failed: "出错了：", project: "项目",
      truncated: "这里的内容太多，只列出了前 2000 项。",
    } : {
      ok: "OK", cancel: "Cancel", yes: "Yes", no: "No",
      chooseFolder: "Choose a folder", chooseFile: "Choose a file", chooseFiles: "Choose files",
      useFolder: "Choose this folder", open: "Open", newFolder: "New folder", upload: "Upload from this device…",
      folderName: "Folder name", create: "Create", home: "Home", empty: "Nothing to choose here.",
      uploading: (done, total) => `Uploading ${done} of ${total}…`, failed: "Something went wrong: ", project: "Project",
      truncated: "This folder holds too much; the first 2,000 entries are listed.",
    };
    const svg = (paths, size = 15) => `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;
    const FOLDER = '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"></path>';
    const FILE = '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"></path><path d="M14 2v4a2 2 0 0 0 2 2h4"></path>';
    const CHECK = '<path d="M20 6 9 17l-5-5"></path>';
    const el = (tag, props = {}, ...children) => {
      const node = document.createElement(tag);
      for (const [key, value] of Object.entries(props)) {
        if (key === "class") node.className = value;
        else if (key === "html") node.innerHTML = value;
        else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
        else if (value !== undefined && value !== null && value !== false) node.setAttribute(key, value === true ? "" : value);
      }
      for (const child of children.flat()) if (child !== null && child !== undefined && child !== false) node.append(child);
      return node;
    };
    const button = (label, variant, onclick) =>
      el("button", { type: "button", class: `ui-button ui-button--${variant}`, "data-slot": "button", "data-variant": variant, onclick }, label);
    const api = async (path, body) => {
      const response = await fetch(path, {
        method: "POST", credentials: "same-origin", cache: "no-store",
        headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
      });
      const value = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(value?.error?.message || `HTTP ${response.status}`);
      return value;
    };

    const style = el("style", { "data-lattice-gateway": "tab-dialog-style" });
    style.textContent = `
      /* Above Lattice's own layers; .modal-dialog-content's z-index would otherwise win and leave the card under its backdrop. */
      .modal-backdrop[data-lattice-gateway="tab-dialog-backdrop"] { z-index: 2147483645; }
      .modal-dialog-content[data-lattice-gateway="tab-dialog"] { z-index: 2147483646; }
      [data-lattice-gateway="tab-dialog"] { outline: none; }
      .gw-picker { width: min(560px, calc(100vw - 24px)); display: flex; flex-direction: column; max-height: min(640px, calc(100dvh - 48px)); }
      .gw-picker > h2 { margin-bottom: var(--space-4); }
      .gw-crumbs { display: flex; flex-wrap: wrap; align-items: center; gap: 2px; margin: 0 0 var(--space-4); font-size: var(--type-caption-size); color: var(--text-secondary); }
      .gw-crumbs button { padding: 2px 6px; border-radius: var(--radius-control); background: transparent; color: inherit; font: inherit; cursor: pointer; }
      .gw-crumbs button:hover { background: var(--control-hover-surface); color: var(--text-primary); }
      .gw-crumbs button[aria-current] { color: var(--text-primary); font-weight: var(--weight-semibold, 600); }
      .gw-crumbs span { color: var(--text-tertiary); }
      .gw-list { flex: 1 1 auto; min-height: 160px; overflow-y: auto; overscroll-behavior: contain; margin: 0 calc(var(--space-4) * -1); padding: var(--space-2) var(--space-4); border-block: 1px solid var(--border-subtle); }
      .gw-row { width: 100%; min-height: 32px; display: flex; align-items: center; gap: var(--space-4); padding: 0 var(--space-4); border-radius: var(--radius-control); background: transparent; color: var(--text-primary); font-size: var(--type-body-compact-size, 13px); text-align: left; cursor: pointer; }
      .gw-row:hover { background: var(--control-hover-surface); }
      .gw-row[aria-selected="true"] { background: var(--control-active-soft); color: var(--control-active); }
      .gw-row > svg { flex: none; color: var(--text-tertiary); }
      .gw-row[aria-selected="true"] > svg { color: currentColor; }
      .gw-row-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
      .gw-row-meta { flex: none; color: var(--text-tertiary); font-size: var(--type-caption-size); font-variant-numeric: tabular-nums; }
      .gw-empty { padding: var(--space-8) var(--space-4); color: var(--text-tertiary); font-size: var(--type-caption-size); text-align: center; }
      .gw-new { display: flex; gap: var(--space-3); padding: var(--space-2) 0 var(--space-3); }
      .gw-new input { flex: 1; min-width: 0; }
      .gw-status { margin: var(--space-4) 0 0; font-size: var(--type-caption-size); color: var(--text-secondary); }
      .gw-status[data-tone="danger"] { color: var(--status-danger); }
      .gw-picker .modal-actions { margin-top: var(--space-6); flex-wrap: wrap; }
      .gw-picker .modal-actions .gw-left { margin-right: auto; }
      @media (max-width: 560px) { .gw-picker .modal-actions .gw-left { width: 100%; justify-content: flex-start; } }
      @media (pointer: coarse) { .gw-row { min-height: 40px; } }
    `;

    // One dialog at a time, made like Lattice's ModalDialog: backdrop, card, Escape closes.
    let current = null;
    function present(card, label, onEscape) {
      if (!style.isConnected) document.head.append(style);
      current?.close();
      const backdrop = el("div", { class: "modal-backdrop confirm-action-backdrop", "data-lattice-gateway": "tab-dialog-backdrop" });
      const dialog = el("div", { class: "modal-dialog-content", "data-lattice-gateway": "tab-dialog", role: "dialog", "aria-modal": "true", "aria-label": label, tabindex: "-1" }, card);
      const returnTo = document.activeElement;
      const keydown = (event) => {
        if (event.key !== "Escape") return;
        event.preventDefault();
        event.stopImmediatePropagation();
        onEscape();
      };
      const close = () => {
        backdrop.remove();
        dialog.remove();
        removeEventListener("keydown", keydown, true);
        if (current?.dialog === dialog) current = null;
        if (returnTo?.isConnected) returnTo.focus?.({ preventScroll: true });
      };
      backdrop.addEventListener("click", onEscape);
      addEventListener("keydown", keydown, true);
      document.body.append(backdrop, dialog);
      current = { dialog, close };
      return { dialog, close };
    }

    // plugin:dialog|message, which message(), ask() and confirm() all send:
    // answered with the name of the button pressed, as the plugin does.
    tabDialogs.message = ({ message, title, kind, buttons } = {}) => new Promise((resolve) => {
      let actions;
      if (buttons === "OkCancel") actions = [["Cancel", T.cancel, "ghost"], ["Ok", T.ok, "primary"]];
      else if (buttons === "YesNo") actions = [["No", T.no, "ghost"], ["Yes", T.yes, "primary"]];
      else if (buttons === "YesNoCancel") actions = [["Cancel", T.cancel, "ghost"], ["No", T.no, "secondary"], ["Yes", T.yes, "primary"]];
      else if (Array.isArray(buttons?.OkCancelCustom)) actions = [[buttons.OkCancelCustom[1], buttons.OkCancelCustom[1], "ghost"], [buttons.OkCancelCustom[0], buttons.OkCancelCustom[0], "primary"]];
      else if (Array.isArray(buttons?.YesNoCancelCustom)) {
        const [yes, no, cancel] = buttons.YesNoCancelCustom;
        actions = [[cancel, cancel, "ghost"], [no, no, "secondary"], [yes, yes, "primary"]];
      } else if (typeof buttons?.OkCustom === "string") actions = [[buttons.OkCustom, buttons.OkCustom, "primary"]];
      else actions = [["Ok", T.ok, "primary"]];
      // Escape and the backdrop give the cautious answer: the first button.
      const cautious = actions[0][0];
      let shown;
      const settle = (answer) => { shown.close(); resolve(answer); };
      const card = el("div", { class: "modal confirm-action-modal", "data-has-alternative": actions.length > 2 ? "true" : null, "data-kind": kind || null },
        el("h2", {}, typeof title === "string" && title ? title : "Lattice"),
        typeof message === "string" && message ? el("p", {}, message) : null,
        el("div", { class: "modal-actions" }, actions.map(([answer, label, variant]) => button(label, variant, () => settle(answer)))));
      shown = present(card, typeof title === "string" && title ? title : "Lattice", () => settle(cautious));
      card.querySelector(".ui-button--primary")?.focus({ preventScroll: true });
    });

    const size = (bytes) => bytes < 1024 ? `${bytes} B` : bytes < 1048576 ? `${(bytes / 1024).toFixed(1)} KB`
      : bytes < 1073741824 ? `${(bytes / 1048576).toFixed(1)} MB` : `${(bytes / 1073741824).toFixed(2)} GB`;

    // A file sent to `folder` on the host, with progress; resolves its host path.
    const sendFile = (file, folder, progress) => new Promise((resolve, reject) => {
      const request = new XMLHttpRequest();
      request.open("POST", "/__gateway/api/upload");
      request.withCredentials = true;
      request.setRequestHeader("X-Upload-Folder", encodeURIComponent(folder));
      request.setRequestHeader("X-Upload-Name", encodeURIComponent(file.name));
      request.upload.onprogress = (event) => progress(event.loaded);
      request.onload = () => {
        let value = {};
        try { value = JSON.parse(request.responseText); } catch { /* not JSON */ }
        if (request.status === 200 && typeof value.path === "string") resolve(value.path);
        else reject(new Error(value?.error?.message || `HTTP ${request.status}`));
      };
      request.onerror = () => reject(new Error("network"));
      request.send(file);
    });

    // plugin:dialog|open: a folder, a file or several, on the host. Resolves
    // a path, a list of paths, or null when cancelled.
    tabDialogs.open = (options = {}) => new Promise((resolve) => {
      const directory = options.directory === true;
      const multiple = options.multiple === true && !directory;
      const extensions = (Array.isArray(options.filters) ? options.filters : [])
        .flatMap((filter) => (Array.isArray(filter?.extensions) ? filter.extensions : []))
        .filter((ext) => typeof ext === "string");
      let shown;
      let listing = null;
      const chosen = new Set();
      const finish = (value) => { shown.close(); resolve(value); };
      const cancel = () => finish(null);

      const heading = el("h2", {}, typeof options.title === "string" && options.title ? options.title
        : directory ? T.chooseFolder : multiple ? T.chooseFiles : T.chooseFile);
      const crumbs = el("nav", { class: "gw-crumbs", "aria-label": T.home });
      const list = el("div", { class: "gw-list", role: "listbox", "aria-multiselectable": multiple ? "true" : null });
      const status = el("p", { class: "gw-status", hidden: true, role: "status" });
      const primary = button(directory ? T.useFolder : T.open, "primary", () => {
        if (directory) finish(listing?.path ?? null);
        else if (chosen.size) finish(multiple ? [...chosen] : [...chosen][0]);
      });
      const say = (text, tone) => { status.textContent = text; status.hidden = !text; if (tone) status.dataset.tone = tone; else delete status.dataset.tone; };
      const refresh = () => { primary.disabled = directory ? !listing : chosen.size === 0; };

      const input = el("input", { type: "file", hidden: true, multiple: multiple ? true : null,
        accept: extensions.length ? extensions.map((ext) => `.${ext}`).join(",") : null });
      input.addEventListener("change", async () => {
        const files = [...input.files];
        input.value = "";
        if (!files.length || !listing) return;
        const total = files.reduce((sum, file) => sum + file.size, 0);
        let before = 0;
        const paths = [];
        primary.disabled = true;
        try {
          for (const file of files) {
            paths.push(await sendFile(file, listing.path, (loaded) => say(T.uploading(size(before + loaded), size(total)))));
            before += file.size;
          }
        } catch (error) {
          say(T.failed + error.message, "danger");
          await go(listing.path);
          return;
        }
        say("");
        // What was uploaded is what was chosen.
        finish(multiple ? paths : paths[0]);
      });

      let newRow = null;
      const newFolder = () => {
        if (newRow) { newRow.querySelector("input").focus(); return; }
        const field = el("input", { class: "ui-input", "data-slot": "input", placeholder: T.folderName, "aria-label": T.folderName });
        const make = async () => {
          const name = field.value.trim();
          if (!name || !listing) return;
          try {
            const made = await api("/__gateway/api/files/folder", { path: listing.path, name });
            newRow?.remove(); newRow = null;
            await go(made.path);
          } catch (error) { say(T.failed + error.message, "danger"); }
        };
        field.addEventListener("keydown", (event) => { if (event.key === "Enter") { event.preventDefault(); make(); } });
        newRow = el("div", { class: "gw-new" }, field, button(T.create, "secondary", make));
        list.before(newRow);
        field.focus();
      };
      const left = el("div", { class: "gw-left" },
        directory ? button(T.newFolder, "ghost", newFolder) : button(T.upload, "ghost", () => input.click()));

      function draw() {
        crumbs.replaceChildren();
        const home = listing.home;
        const rest = listing.path === home ? [] : listing.path.slice(home.length + 1).split("/");
        const steps = [[T.home, home], ...rest.map((name, i) => [name, `${home}/${rest.slice(0, i + 1).join("/")}`])];
        steps.forEach(([name, path], i) => {
          if (i) crumbs.append(el("span", { "aria-hidden": "true" }, "/"));
          crumbs.append(el("button", { type: "button", "aria-current": i === steps.length - 1 ? "location" : null, onclick: () => go(path) }, name));
        });
        list.replaceChildren();
        for (const entry of listing.entries) {
          const path = `${listing.path}/${entry.name}`;
          const folder = entry.kind === "folder";
          const row = el("button", { type: "button", class: "gw-row", role: "option", "aria-selected": chosen.has(path) ? "true" : "false",
            html: svg(folder ? FOLDER : chosen.has(path) && multiple ? CHECK : FILE) },
          el("span", { class: "gw-row-name" }, entry.name),
          folder && entry.project ? el("span", { class: "gw-row-meta" }, T.project) : null,
          !folder ? el("span", { class: "gw-row-meta" }, size(entry.size)) : null);
          row.addEventListener("click", () => {
            if (folder) { go(path); return; }
            if (!multiple) chosen.clear();
            if (chosen.has(path)) chosen.delete(path); else chosen.add(path);
            draw();
          });
          if (!folder) row.addEventListener("dblclick", () => { chosen.clear(); chosen.add(path); finish(multiple ? [path] : path); });
          list.append(row);
        }
        if (!listing.entries.length) list.append(el("p", { class: "gw-empty" }, T.empty));
        if (listing.truncated) say(T.truncated);
        refresh();
      }
      async function go(path, start = false) {
        try {
          listing = await api("/__gateway/api/files", { path, start, files: !directory, extensions });
          if (!multiple) chosen.clear();
          draw();
          list.scrollTop = 0;
        } catch (error) {
          say(T.failed + error.message, "danger");
        }
      }

      const card = el("div", { class: "modal gw-picker" }, heading, crumbs, list, status,
        el("div", { class: "modal-actions" }, left, button(T.cancel, "ghost", cancel), primary), input);
      shown = present(card, heading.textContent, cancel);
      refresh();
      shown.dialog.focus({ preventScroll: true });
      go(typeof options.defaultPath === "string" ? options.defaultPath : null, true);
    });
  })();

})();
