# Lattice host kit

This kit runs the real Lattice on a Linux machine. Your Mac, iPhone or any
other browser opens it as one Lattice window: every project opens in that
window, and nothing is added to Lattice's interface. The only page that is not
Lattice is the sign-in page, and on your tailnet you don't see that either.

```
browser ── HTTPS (Tailscale Funnel/Serve, or Caddy) ── lattice-gateway ── Lattice (browser host, 127.0.0.1:18452)
```

Lattice's Linux AppImage runs unmodified, in browser-host mode, on a private
virtual display. `lattice-gateway` decides who reaches it and adapts it to a
remote host without changing what you see.

## Requirements

- Linux on x86-64 with systemd user services. Run `loginctl enable-linger "$USER"`
  once so that the services keep running after you log out.
- Xvfb (`sudo apt install xvfb`). Without sudo, pass `--fetch-xvfb` to unpack
  Ubuntu's package into the host directory.
- `dbus-run-session` (package `dbus`) and the WebKitGTK 4.1 runtime that Lattice
  uses (`libwebkit2gtk-4.1-0`).
- bubblewrap (`bwrap`), for Lattice's Agent.
- TeX with `latexmk`. The agent CLIs (Claude Code, Codex) need to be signed in
  as the same user.
- Tailscale on the host, or Caddy with a public hostname.

## Install

From the extracted kit:

```sh
./lattice-host install --tailscale --owner-login you@github \
  --public-origin https://host.example.ts.net:8443 \
  --agent-origin  https://host.example.ts.net:10000 \
  --projects-root ~/lattice-projects
tailscale funnel --bg --https=8443  unix:$HOME/.local/share/lattice-host/serve.sock
tailscale funnel --bg --https=10000 unix:$HOME/.local/share/lattice-host/agent.sock
```

- **Funnel ports.** Funnel makes the workspace reachable from the internet, and
  it only offers ports 443, 8443 and 10000. To keep the workspace on your
  tailnet only, use `tailscale serve` with the same sockets instead.
- **Agent origin.** Lattice's Agent has a page of its own, so it needs a second
  origin on the same host.
- **Projects.** Keep projects in an ordinary folder. Lattice's Agent refuses
  projects inside hidden folders such as `~/.local/share`.

`install` downloads the Lattice release pinned in `lattice-releases.json`,
checks its SHA-256, and starts three systemd user services:

| Service | What it runs |
|---|---|
| `lattice-display` | Xvfb on display `:99`, with no TCP listener |
| `lattice-browser-host` | Lattice with `--browser-host`, in a scrubbed environment with its own session bus |
| `lattice-gateway` | The gateway on the Tailscale sockets, plus `127.0.0.1:18490` and `:18491` for Caddy |

## Signing in

- **On your tailnet**, Tailscale tells the gateway who is asking. If that is
  one of the `--owner-login` accounts, Lattice opens straight away, with no
  sign-in page.
- **From the internet**, you see the sign-in page from v0.4.x, **Open
  workspace**:
  - The owner signs in with the server access token, which is in
    `~/.local/share/lattice-host/access-token`.
  - A new device scans a pairing QR code instead. The owner opens **Phone and
    app** and selects **Pair a device**. The phone scans the code and selects
    **Connect this device**. The code works once and expires after two
    minutes. The device stays signed in for 90 days, and **Revoke** cuts it off
    at once.

## Phone and app

**Phone and app** is a dialog in the workspace, made from Lattice's own
Settings parts. It has three groups:

- **Open on your phone.** A QR code for the workspace's address: a phone
  signed in to the same Tailscale account goes straight in. It also offers a
  one-time pairing code for any other phone.
- **Install the app.** The steps for iPhone and iPad, Android, Mac, and
  Windows or Linux, with the current device marked. Chrome and Edge get an
  **Install** button.
- **Paired devices.** Each device, with **Revoke**.

How to open it:

- **Lattice's Settings.** The sidebar ends with a **Lattice-web-ui** group
  holding **Phone and app**.
- **The plugin.** Lattice-web-ui's plugin adds a **Phone and app** command to
  Lattice's command palette. Install the plugin in the host's Lattice from
  Settings › Plugins.
- **The address.** `/__gateway/devices` opens the dialog.
- **A one-time hint.** The first time the owner opens the workspace on a
  computer, a notification offers it.

`./lattice-host phone` shows the address as a QR code on the host's terminal,
as `install` does when it finishes.

The gateway trusts Tailscale's identity headers only on its Unix sockets, and
only from tailscaled (root, or `--ingress-uid` for a tailscaled running in a
container). Requests that come in over TCP never carry an identity.

## One window

Pick a project from Lattice's own project menu, and it opens in the same tab.
The gateway lists every project in `--projects-root` there, among the recent
projects.

Opening the workspace's address on a device or in a tab that has no workspace
of its own shows the workspace you used last, on whichever device.

You can keep the workspace open on several devices. Lattice gives it to one
tab at a time, so the device you use last has it. The others keep showing the
workspace, and take it back as soon as you switch to them, tap or type. The
reload that takes it back takes a second or two.

## Overleaf

Lattice signs in to Overleaf in a native window, so the sign-in happens on the
host, never on a browser device:

```sh
./lattice-host overleaf-login
```

This shares the host's hidden display over VNC on `127.0.0.1:5999` for one
session, with a one-time password. It prints these steps:

1. On your Mac, run `ssh -N -L 5999:127.0.0.1:5999 <host>`.
2. Open `vnc://localhost:5999` in Screen Sharing.
3. In the workspace, choose **Open from Overleaf** › **Connect to Overleaf**,
   and sign in in the window that appears.

The session ends when you disconnect, or after 30 minutes. x11vnc is unpacked
into the host directory if it is not installed.

## What else the gateway does, invisibly

- **Lattice's page.** Lattice 0.19 only starts at `http://127.0.0.1:18452`. The
  gateway rewrites ten loopback-only expressions in its scripts as it forwards
  them. Each pinned release lists the rewrites it needs, and `install` and
  `update` refuse a release whose page differs. This goes away once Lattice
  accepts a public origin
  ([leo1oel/lattice#1190](https://github.com/leo1oel/lattice/issues/1190)).
- **TeX packages.** Lattice has no one-click package install on Linux. When
  the owner clicks **Install** for a missing package, the gateway installs it
  with `tlmgr`. This needs a TeX tree the host user can write to, such as
  TinyTeX.
- **Phones.** Below 700 px, Lattice gets one panel with every view as a tab.
  The wide layout comes back on a screen at least 900 px wide.

## Update and roll back

**From the browser.** When a newer stable lattice-web-ui release is out, the
owner sees it in the workspace as a Lattice notification: **Lattice-web-ui
0.9.1 is available**, with **Update** and **Later**.

- **Checking.** The host checks GitHub's latest release every six hours, using
  the `lattice-update-check.timer` unit.
- **Update.** The host downloads the release, checks its SHA-256 and installs
  it. The workspace reconnects, and the page reloads when it is done.
- **If it fails.** When the new gateway does not answer within 90 seconds, the
  host goes back to the kit it had, and the notification says why.
- **Later.** Hides the notification until a newer release comes out.
- **The chip stays.** While an update is on offer, the title bar shows
  **Update to 0.9.1** beside Lattice's own buttons, whatever happened to the
  notification. Select it to bring the notification back. On a phone it is a
  round button in the bottom corner.
- **Settings › Lattice-web-ui › Version.** A page of Lattice's Settings with
  the versions this host runs, **Check for updates** (the
  `lattice-update-check.path` unit runs the check at once), **Update to …**,
  and what each newer release brings, from its release notes. The same row
  stands in for Lattice's own **App updates › Version** under Editor &
  builds, which would say Lattice on Linux doesn't update itself.

Paired devices see none of this. Only a host installed from a release kit
updates itself this way. The kit it runs is kept in
`~/.local/share/lattice-host/kits`.

**From a shell:**

```sh
./lattice-host status
./lattice-host update --to 0.19.0   # a version pinned in lattice-releases.json
./lattice-host rollback
```

`update` stages the new release and restarts Lattice. If Lattice does not
answer within 90 seconds, it switches back. `./lattice-host uninstall` stops
everything. Add `--purge` to also delete releases, paired devices and the
token.

## Without Tailscale

`examples/Caddyfile` terminates TLS for a public hostname and forwards to
`127.0.0.1:18490`. Then everyone signs in or pairs, and the owner signs in with
the token.

## Known limits

These come from Lattice, and are tracked in
[leo1oel/lattice#1190](https://github.com/leo1oel/lattice/issues/1190).

- **Switching projects reloads the page.** Lattice opens the project in a new
  workspace, and the gateway sends the tab to it. Once
  [#1206](https://github.com/leo1oel/lattice/pull/1206) is merged, Lattice
  opens it in place, as a project tab.
- **No terminal.** Lattice's terminal panel works only in its native window.
- **Restarts.** If Lattice restarts, text typed in the last second before the
  restart is lost.
- **Memory.** Every workspace keeps a hidden WebKit view on the host, about
  200 MB each.
