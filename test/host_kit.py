"""lattice-host without systemd: staging, verification, unit files, update and rollback.

Run: python3 test/host_kit.py
"""

import hashlib
import http.server
import importlib.util
import io
import json
import os
import stat
import sys
import tarfile
import tempfile
import threading
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOL = HERE.parent / "host" / "lattice-host.py"

FAKE_APPIMAGE = """#!/bin/sh
# Stands in for a Lattice AppImage: only --appimage-extract is supported.
[ "$1" = "--appimage-extract" ] || exit 64
mkdir -p squashfs-root/usr/bin
printf '#!/bin/sh\necho bundled-opener\n' > squashfs-root/usr/bin/xdg-open
printf '#!/bin/sh\\necho lattice VERSION "$@"\\n' > squashfs-root/AppRun
chmod +x squashfs-root/AppRun
"""


def load_tool(config_home: Path):
    os.environ["XDG_CONFIG_HOME"] = str(config_home)
    spec = importlib.util.spec_from_file_location("lattice_host", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fake_kit(folder: Path, version: str, marker: Path) -> Path:
    """An unpacked host kit whose lattice-host only records that it ran."""
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "release-version.txt").write_text(version + "\n")
    tool = folder / "lattice-host"
    tool.write_text(f"#!/usr/bin/env python3\nimport sys\nopen({str(marker)!r}, 'a').write({version!r} + ' ' + ' '.join(sys.argv[1:]) + '\\n')\n")
    tool.chmod(0o755)
    return folder


class Server:
    """A local stand-in for GitHub's release API and the gateway's health check."""

    def __init__(self):
        self.files = {}
        files = self.files

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                body = files.get(self.path)
                if body is None:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.end_headers()
                self.wfile.write(body() if callable(body) else body)

            def log_message(self, *args):
                pass

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.httpd.server_address[1]
        self.base = f"http://127.0.0.1:{self.port}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class HostKitTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        base = Path(self.temp.name)
        self.root = base / "host"
        self.config_home = base / "config"
        self.tool = load_tool(self.config_home)
        self.releases = []
        for version in ("0.18.0", "0.19.0"):
            image = base / f"Lattice_{version}.AppImage"
            image.write_text(FAKE_APPIMAGE.replace("VERSION", version))
            image.chmod(0o755)
            self.releases.append({"version": version, "url": f"file://{image}", "sha256": hashlib.sha256(image.read_bytes()).hexdigest()})
        self.releases[0]["frontendRewrite"] = {"http": 4, "ws": 2, "protocol": 1, "hostname": 2, "port": 1}
        self.manifest = base / "lattice-releases.json"
        self.manifest.write_text(json.dumps({"default": "0.18.0", "releases": self.releases}))
        self.gateway = base / "lattice-gateway"
        self.gateway.write_text("#!/bin/sh\n")
        self.gateway.chmod(0o755)
        self.xvfb = base / "Xvfb"
        self.xvfb.write_text("#!/bin/sh\n")
        self.xvfb.chmod(0o755)

    def tearDown(self):
        self.temp.cleanup()

    def run_tool(self, *argv):
        return self.tool.main(["--root", str(self.root), *argv])

    def install(self, *extra):
        return self.run_tool(
            "install", "--public-origin", "https://w.example.ts.net", "--manifest", str(self.manifest),
            "--gateway-binary", str(self.gateway), "--xvfb", str(self.xvfb), "--no-start", *extra,
        )

    def test_install_writes_a_private_tree_and_three_units(self):
        self.assertEqual(self.install(), 0)
        self.assertEqual(stat.S_IMODE(self.root.stat().st_mode), 0o700)
        self.assertEqual(os.readlink(self.root / "current"), "releases/0.18.0")
        self.assertEqual(stat.S_IMODE((self.root / "host.json").stat().st_mode), 0o600)
        units = self.config_home / "systemd/user"
        for unit in self.tool.UNITS:
            self.assertTrue((units / unit).exists(), unit)
        gateway_unit = (units / "lattice-gateway.service").read_text()
        self.assertIn("--public-origin https://w.example.ts.net", gateway_unit)
        self.assertIn("--upstream 127.0.0.1:18452", gateway_unit)
        launcher = (self.root / "bin/run-lattice").read_text()
        self.assertIn("exec env -i", launcher)
        self.assertIn("DISPLAY=:99", launcher)
        self.assertIn("dbus-run-session --", launcher)
        self.assertIn("current/squashfs-root/AppRun --browser-host", launcher)
        self.assertIn(f"PATH={self.root / 'bin'}:", launcher, "the no-op xdg-open comes first")
        self.assertNotIn("WAYLAND_DISPLAY", launcher)

    def test_lattice_opener_forwards_only_entry_addresses(self):
        import subprocess
        self.assertEqual(self.install("--projects-root", "~/papers", "--agent-origin", "https://w.example.ts.net:10000"), 0)
        spool = self.root / "state/open-requests"
        self.assertEqual(stat.S_IMODE(spool.stat().st_mode), 0o700)
        bundled = self.root / "releases/0.18.0/squashfs-root/usr/bin/xdg-open"
        self.assertEqual(bundled.read_text(), (self.root / "bin/xdg-open").read_text(), "the AppImage's own opener is replaced")
        for url in ("https://example.org/", "http://127.0.0.1:18452/", "file:///etc/passwd"):
            subprocess.run([str(bundled), url], check=True)
        self.assertEqual(list(spool.iterdir()), [])
        subprocess.run([str(bundled), "http://127.0.0.1:18452/?entry=abc123"], check=True)
        files = list(spool.iterdir())
        self.assertEqual(len(files), 1)
        self.assertEqual(files[0].read_text(), "http://127.0.0.1:18452/?entry=abc123\n")
        unit = (self.config_home / "systemd/user/lattice-gateway.service").read_text()
        self.assertIn("--rewrite-loopback-page", unit)
        self.assertIn(f"--open-requests {spool}", unit)
        self.assertIn(f"--projects-root {Path('~/papers').expanduser()}", unit)
        self.assertIn("--agent-listen 127.0.0.1:18491 --agent-origin https://w.example.ts.net:10000", unit)
        self.assertIn(f"--agent-process-marker {self.root / 'releases'}", unit)

    def test_warns_about_projects_in_hidden_folders(self):
        home = Path(self.temp.name) / "home"
        self.assertTrue(self.tool.hidden_under_home(home / ".local/share/x/projects", home))
        self.assertFalse(self.tool.hidden_under_home(home / "lattice-projects", home))
        self.assertFalse(self.tool.hidden_under_home(Path("/srv/papers"), home))

    def test_refuses_a_file_whose_checksum_does_not_match(self):
        self.releases[0]["sha256"] = "0" * 64
        self.manifest.write_text(json.dumps({"default": "0.18.0", "releases": self.releases}))
        self.assertEqual(self.install(), 1)
        self.assertFalse((self.root / "releases/0.18.0").exists())
        self.assertFalse((self.root / "current").exists())

    def test_update_and_rollback_move_the_current_link(self):
        self.assertEqual(self.install(), 0)
        self.gateway.write_text("#!/bin/sh\necho newer gateway\n")
        self.assertEqual(self.run_tool("update", "--to", "0.19.0", "--no-start", "--manifest", str(self.manifest), "--gateway-binary", str(self.gateway)), 0)
        self.assertEqual((self.root / "bin/lattice-gateway").read_text(), "#!/bin/sh\necho newer gateway\n", "update installs the kit's gateway")
        self.assertEqual(os.readlink(self.root / "current"), "releases/0.19.0")
        self.assertEqual(os.readlink(self.root / "previous"), "releases/0.18.0")
        self.assertIn("0.19.0", (self.config_home / "systemd/user/lattice-browser-host.service").read_text())
        self.assertNotIn("--rewrite-loopback-page", (self.config_home / "systemd/user/lattice-gateway.service").read_text(),
                         "a release that pins no rewrites gets none")
        self.assertEqual(self.run_tool("rollback", "--no-start"), 0)
        self.assertEqual(os.readlink(self.root / "current"), "releases/0.18.0")
        self.assertEqual(self.run_tool("update", "--to", "9.9.9", "--no-start", "--manifest", str(self.manifest)), 1)

    def test_a_kit_install_can_update_itself_later(self):
        kit = fake_kit(Path(self.temp.name) / "kit-0.9.0", "0.9.0", Path(self.temp.name) / "ran.txt")
        self.tool.KIT = kit
        self.assertEqual(self.install(), 0)
        self.assertEqual(os.readlink(self.root / "kit"), "kits/0.9.0")
        units = self.config_home / "systemd/user"
        for unit in self.tool.UPDATE_FILES:
            self.assertTrue((units / unit).exists(), unit)
        self.assertIn(f"PathExists={self.root / 'state/update/request'}", (units / "lattice-update.path").read_text())
        self.assertIn(f"PathExists={self.root / 'state/update/check'}", (units / "lattice-update-check.path").read_text())
        self.assertIn(f"{self.root / 'kit/lattice-host'} --root {self.root} self-update", (units / "lattice-update.service").read_text())
        self.assertIn(f"--update-dir {self.root / 'state/update'}", (units / "lattice-gateway.service").read_text())

    def test_phone_shows_the_address_as_a_code(self):
        self.assertEqual(self.install("--tailscale", "--owner-login", "me@github"), 0)
        (self.root / "bin/lattice-gateway").write_text('#!/bin/sh\n[ "$1" = --print-qr ] && echo "CODE FOR $2"\n')
        import contextlib, io
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            self.assertEqual(self.run_tool("phone"), 0)
        text = out.getvalue()
        self.assertIn("CODE FOR https://w.example.ts.net/", text)
        self.assertIn("same Tailscale account", text)
        self.assertIn("Phone and app", text)

    def test_a_source_checkout_install_has_no_update_units(self):
        self.assertEqual(self.install(), 0)
        self.assertFalse((self.root / "kit").exists())
        self.assertFalse((self.config_home / "systemd/user/lattice-update.path").exists())
        self.assertNotIn("--update-dir", (self.config_home / "systemd/user/lattice-gateway.service").read_text())

    def test_tailscale_sockets_need_an_owner_login(self):
        self.assertEqual(self.install("--tailscale"), 1)
        self.assertEqual(self.install("--tailscale", "--owner-login", "me@github", "--agent-origin", "https://w.example.ts.net:10000"), 0)
        unit = (self.config_home / "systemd/user/lattice-gateway.service").read_text()
        self.assertIn(f"--serve-socket {self.root / 'serve.sock'} --owner-logins {self.root / 'owner-logins'}", unit)
        self.assertIn(f"--agent-serve-socket {self.root / 'agent.sock'}", unit)
        self.assertEqual((self.root / "owner-logins").read_text(), "me@github\n")
        opener = (self.root / "bin/xdg-open").read_text()
        self.assertIn("https://w.example.ts.net/__gateway/devices) ;;", opener)


class SelfUpdateTest(unittest.TestCase):
    """The owner's Update button: check-update finds a release, self-update installs it."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        base = Path(self.temp.name)
        self.base = base
        self.root = base / "host"
        self.tool = load_tool(base / "config")
        self.server = Server()
        os.environ["LATTICE_HOST_RELEASES_API"] = self.server.base + "/api"
        self.marker = base / "ran.txt"
        # An installed 0.8.3 kit, with its gateway answering on the server.
        fake_kit(self.root / "kits/0.8.3", "0.8.3", self.marker)
        (self.root / "kit").symlink_to("kits/0.8.3")
        self.root.chmod(0o700)
        (self.root / "host.json").write_text(json.dumps({"version": "0.19.0", "gateway_port": self.server.port}))
        self.health = "0.8.3"
        self.server.files["/__gateway/health"] = lambda: json.dumps({"version": self.health}).encode()
        self.publish("0.9.0")

    def tearDown(self):
        os.environ.pop("LATTICE_HOST_RELEASES_API", None)
        self.server.close()
        self.temp.cleanup()

    def publish(self, version, tamper=False):
        tag = f"v{version}"
        name = f"lattice-remote-host-{version}-linux-x86_64.tar.gz"
        kit = fake_kit(self.base / "build" / f"lattice-remote-host-{version}", version, self.marker)
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
            tar.add(kit, arcname=kit.name)
        archive = buffer.getvalue()
        digest = hashlib.sha256(archive).hexdigest()
        files = self.server.files
        files[f"/dl/{name}"] = archive + (b"x" if tamper else b"")
        files[f"/dl/SHA256SUMS-{tag}.txt"] = f"{digest}  {name}\n".encode()
        files[f"/dl/release-{tag}.json"] = json.dumps({"lattice": {"default": "0.20.0"}, "assets": {name: digest}}).encode()
        release = {"tag_name": tag, "html_url": f"https://github.com/lattice-writer/lattice-web-ui/releases/tag/{tag}",
                   "published_at": "2026-11-24T22:46:04Z",
                   "body": f"## Changes\n\n- Release {version}: the version page (#42) (fac9233)\n- A version page in Settings (#41) (dd1fc14)\n\nBuild verification: x\n",
                   "assets": [{"name": n, "browser_download_url": f"{self.server.base}/dl/{n}"}
                              for n in (name, f"SHA256SUMS-{tag}.txt", f"release-{tag}.json")]}
        files["/api/releases/latest"] = json.dumps(release).encode()
        files["/api/releases?per_page=30"] = json.dumps([release,
            {"tag_name": "v0.8.9", "body": "- Between (#30) (aaaaaaa)"},
            {"tag_name": "v0.9.5", "prerelease": True, "body": "- Not stable"},
            {"tag_name": "v0.8.3", "body": "- Already here"}]).encode()

    def run_tool(self, *argv):
        return self.tool.main(["--root", str(self.root), *argv])

    def state(self, name):
        return json.loads((self.root / "state/update" / name).read_text())

    def request(self, version):
        (self.root / "state/update/request").write_text(version)

    def test_check_update_records_the_newest_release(self):
        self.assertEqual(self.run_tool("check-update"), 0)
        found = self.state("available.json")
        self.assertEqual((found["latest"], found["current"], found["lattice"]), ("0.9.0", "0.8.3", {"current": "0.19.0", "latest": "0.20.0"}))
        self.assertEqual(stat.S_IMODE((self.root / "state/update/available.json").stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE((self.root / "state/update").stat().st_mode), 0o700)

    def test_check_update_lists_what_each_newer_version_brings(self):
        self.assertEqual(self.run_tool("check-update"), 0)
        releases = self.state("available.json")["releases"]
        self.assertEqual([r["version"] for r in releases], ["0.9.0", "0.8.9"], "stable, newer than 0.8.3, newest first")
        self.assertEqual(releases[0]["changes"], ["A version page in Settings (#41)"])
        self.assertEqual(releases[0]["url"], "https://github.com/lattice-writer/lattice-web-ui/releases/tag/v0.9.0")
        self.assertEqual(releases[1]["url"], "")

    def test_check_now_is_answered_and_a_failure_is_recorded(self):
        folder = self.root / "state/update"
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "check").write_text("1")
        self.assertEqual(self.run_tool("check-update"), 0)
        self.assertFalse((folder / "check").exists(), "the request is answered")
        self.assertFalse((folder / "check-failed.json").exists())
        del self.server.files["/api/releases/latest"]
        (folder / "check").write_text("1")
        self.assertNotEqual(self.run_tool("check-update"), 0)
        self.assertFalse((folder / "check").exists(), "a failed run does not ask again")
        self.assertIn("404", self.state("check-failed.json")["error"])
        self.assertEqual(self.state("available.json")["latest"], "0.9.0", "the last answer stays")

    def test_self_update_installs_the_requested_kit(self):
        self.assertEqual(self.run_tool("check-update"), 0)
        self.request("0.9.0")
        self.health = "0.9.0"  # the new gateway comes up
        self.assertEqual(self.run_tool("self-update", "--wait", "5"), 0)
        self.assertFalse((self.root / "state/update/request").exists(), "the request is consumed")
        self.assertEqual(self.state("status.json")["phase"], "done")
        self.assertEqual(self.marker.read_text(), f"0.9.0 --root {self.root} update\n", "the new kit's update ran, once")
        self.assertEqual(self.tool.kit_version(self.root / "kits/0.9.0"), "0.9.0")

    def test_a_gateway_that_does_not_come_up_goes_back(self):
        self.assertEqual(self.run_tool("check-update"), 0)
        self.request("0.9.0")
        self.assertEqual(self.run_tool("self-update", "--wait", "2"), 1)
        status = self.state("status.json")
        self.assertEqual(status["phase"], "failed")
        self.assertEqual((status["previous"], status["rolledBack"]), ("0.8.3", True))
        self.assertIn("did not answer", status["error"])
        self.assertEqual(self.marker.read_text().splitlines(), [f"0.9.0 --root {self.root} update", f"0.8.3 --root {self.root} update"])

    def test_a_kit_whose_checksum_differs_is_never_run(self):
        self.publish("0.9.0", tamper=True)
        self.assertEqual(self.run_tool("check-update"), 0)
        self.request("0.9.0")
        self.assertEqual(self.run_tool("self-update", "--wait", "2"), 1)
        self.assertIn("checksum mismatch", self.state("status.json")["error"])
        self.assertFalse(self.marker.exists(), "nothing ran")
        self.assertFalse((self.root / "kits/0.9.0").exists())

    def test_only_the_offered_newer_version_is_installed(self):
        self.assertEqual(self.run_tool("check-update"), 0)
        self.request("0.9.1")
        self.assertEqual(self.run_tool("self-update", "--wait", "2"), 1)
        self.assertIn("not the update on offer", self.state("status.json")["error"])
        self.assertFalse(self.marker.exists())

    def test_installing_a_kit_keeps_it(self):
        kit = fake_kit(self.base / "kit-0.9.0", "0.9.0", self.marker)
        (kit / "lattice-releases.json").write_text("{}")
        self.tool.KIT = kit
        config = {"version": "0.19.0"}
        self.assertTrue(self.tool.adopt_kit(self.root, config))
        self.assertEqual(os.readlink(self.root / "kit"), "kits/0.9.0")
        self.assertEqual(config["manifest"], str(self.root / "kits/0.9.0/lattice-releases.json"))
        self.tool.KIT = HERE.parent / "host"
        self.assertFalse(self.tool.adopt_kit(self.root, {}), "a source checkout is not kept")


if __name__ == "__main__":
    sys.exit(0 if unittest.main(exit=False, verbosity=2).result.wasSuccessful() else 1)
